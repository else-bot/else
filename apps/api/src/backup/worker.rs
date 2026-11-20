//! The one serialized backup worker. It captures sets, keeps the newest
//! completed local sets, and records every run; the backup service
//! ([`super::service`]) schedules it. A file lock on the backup directory
//! (released by the kernel if the process dies) serializes capture,
//! retention, and cleanup with a manual `else-api backup` run.

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use super::capture::{BackupConfig, Coordinator, SetSummary, create_set};
use super::{BackupError, DatabaseParams};
use crate::credentials::SecretKey;

pub struct BackupWorker {
    pool: SqlitePool,
    params: DatabaseParams,
    key: Arc<SecretKey>,
    pub config: BackupConfig,
    coordinator: Coordinator,
    replicator: Option<super::replicate::Replicator>,
}

impl BackupWorker {
    pub fn new(
        pool: SqlitePool,
        params: DatabaseParams,
        key: Arc<SecretKey>,
        config: BackupConfig,
    ) -> Self {
        Self {
            pool,
            params,
            key,
            config,
            coordinator: Coordinator::default(),
            replicator: None,
        }
    }

    /// Replicate completed sets to a bucket after each capture.
    pub fn with_replicator(mut self, replicator: super::replicate::Replicator) -> Self {
        self.replicator = Some(replicator);
        self
    }

    pub fn replicator(&self) -> Option<&super::replicate::Replicator> {
        self.replicator.as_ref()
    }

    /// Replicate awaiting sets now (no capture), under the backup lock.
    pub async fn replicate_now(&self) -> Result<Option<super::replicate::Stats>, BackupError> {
        let Some(replicator) = &self.replicator else {
            return Ok(None);
        };
        let lock = Arc::new(
            super::try_lock(&self.config.dir)?
                .ok_or_else(|| BackupError::Refused("another backup is running".into()))?,
        );
        self.adopt_sets(&lock).await?;
        replicator
            .replicate_pending(&self.config.dir, &self.key)
            .await
            .map(Some)
    }

    /// Coordinate with the scheduler and the bot computers: whole-assistant
    /// sets. Without this a set is database only.
    pub fn with_coordinator(mut self, coordinator: Coordinator) -> Self {
        self.coordinator = coordinator;
        self
    }

    /// Capture one set now, record the run, then apply retention. Leftovers
    /// of interrupted runs are cleaned first, under the same lock.
    pub async fn run_once(&self) -> Result<SetSummary, BackupError> {
        self.run_once_until(&CancellationToken::new()).await
    }

    /// As [`Self::run_once`]; cancellation kills and reaps the dump.
    pub async fn run_once_until(
        &self,
        cancel: &CancellationToken,
    ) -> Result<SetSummary, BackupError> {
        self.capture_until(cancel, true).await
    }

    pub async fn run_local_until(
        &self,
        cancel: &CancellationToken,
    ) -> Result<SetSummary, BackupError> {
        self.capture_until(cancel, false).await
    }

    async fn capture_until(
        &self,
        cancel: &CancellationToken,
        upload: bool,
    ) -> Result<SetSummary, BackupError> {
        let lock = Arc::new(
            super::try_lock(&self.config.dir)?
                .ok_or_else(|| BackupError::Refused("another backup is running".into()))?,
        );
        self.recover_locked(&lock).await?;
        let mut summary = self.locked_run(cancel).await?;
        self.adopt_sets(&lock).await?;
        if upload && let Some(replicator) = &self.replicator {
            summary.remote = Some(
                replicator
                    .replicate_pending(&self.config.dir, &self.key)
                    .await
                    .map_err(Arc::new),
            );
        }
        self.apply_local_retention(&summary.set_id, &lock).await?;
        Ok(summary)
    }

    /// Move every whole local set, the one just captured and any an earlier
    /// version kept, into the chunk store. A set that fails stays whole and
    /// is tried again on the next run.
    async fn adopt_sets(&self, lock: &Arc<super::DirLock>) -> Result<(), BackupError> {
        let (dir, key, lock) = (self.config.dir.clone(), self.key.clone(), lock.clone());
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            for set in super::completed_sets(&dir)? {
                if let Err(err) = super::store::adopt(&dir.join(&set), &key) {
                    tracing::warn!(set = %set, error = %err, "a backup set stays whole for now");
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| BackupError::Io(e.to_string()))?
    }

    /// Blocking work that removes sets or chunks holds `lock` itself, so a
    /// cancelled caller does not release it while that work still runs.
    async fn apply_local_retention(
        &self,
        newest: &str,
        lock: &Arc<super::DirLock>,
    ) -> Result<(), BackupError> {
        let awaiting = match &self.replicator {
            Some(replicator) => replicator.awaiting(&self.config.dir).await?,
            None => Vec::new(),
        };
        let dir = self.config.dir.clone();
        let keep = self.config.keep;
        let newest = newest.to_owned();
        let key = self.key.clone();
        let lock = lock.clone();
        let (removed, swept) = tokio::task::spawn_blocking(move || {
            let _lock = lock;
            let removed = super::apply_retention_protecting(&dir, keep, &newest, &awaiting)?;
            // The published set is fine either way; unused chunks wait for
            // the next run.
            let swept = super::store::sweep(&dir, &key).unwrap_or_else(|err| {
                tracing::warn!(error = %err, "unused backup chunks were not removed");
                0
            });
            Ok::<_, BackupError>((removed, swept))
        })
        .await
        .map_err(|e| BackupError::Io(e.to_string()))??;
        if !removed.is_empty() || swept > 0 {
            tracing::info!(
                removed = removed.len(),
                swept,
                "old local backup sets and unused chunks removed"
            );
        }
        Ok(())
    }

    async fn locked_run(&self, cancel: &CancellationToken) -> Result<SetSummary, BackupError> {
        let set_id = super::new_set_id();
        let run = super::runs::start(&self.pool, &set_id).await?;
        let result = create_set(
            &self.pool,
            &self.params,
            &self.key,
            &self.config,
            &set_id,
            cancel,
            &self.coordinator,
        )
        .await;
        match &result {
            Ok(summary) => {
                let bytes: u64 = summary
                    .manifest
                    .artifacts
                    .iter()
                    .map(|a| a.ciphertext_bytes)
                    .sum();
                super::runs::complete(&self.pool, run, bytes, &summary.manifest.coverage).await?;
                tracing::info!(set = %summary.set_id, bytes, "backup set completed");
            }
            Err(err) => {
                let message = err.to_string();
                tracing::warn!(set = %set_id, error = %message, "backup failed");
                super::runs::fail(&self.pool, run, &message).await?;
            }
        }
        result
    }

    /// How long until the next set is due `interval` after the newest one:
    /// zero when none has completed or the newest is older than that.
    pub fn next_due(&self, interval: Duration) -> Result<Duration, BackupError> {
        let sets = super::completed_sets(&self.config.dir)?;
        let Some(newest) = sets.last() else {
            return Ok(Duration::ZERO);
        };
        let path = self.config.dir.join(newest).join(super::COMPLETE);
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map_err(|e| BackupError::io(path.display(), e))?;
        let age = modified.elapsed().unwrap_or_default();
        Ok(interval.saturating_sub(age))
    }

    /// Startup housekeeping under the backup lock: runs left `running` by
    /// an earlier process are completed when their set was published and
    /// failed otherwise, and staging or unfinished sets this application
    /// made are removed. Skipped while a backup is running.
    pub async fn recover(&self) -> Result<usize, BackupError> {
        match super::try_lock(&self.config.dir)? {
            Some(lock) => self.recover_locked(&Arc::new(lock)).await,
            None => Ok(0),
        }
    }

    async fn recover_locked(&self, lock: &Arc<super::DirLock>) -> Result<usize, BackupError> {
        let dir = self.config.dir.clone();
        let published = tokio::task::spawn_blocking(move || super::completed_sets(&dir))
            .await
            .map_err(|e| BackupError::Io(e.to_string()))??;
        super::runs::recover(&self.pool, &published).await?;
        let dir = self.config.dir.clone();
        let (key, lock) = (self.key.clone(), lock.clone());
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            let removed = super::clean_incomplete(&dir)?;
            // Chunks an interrupted capture left could hold the space the
            // next one needs to stage a whole set.
            if let Err(err) = super::store::sweep(&dir, &key) {
                tracing::warn!(error = %err, "unused backup chunks were not removed");
            }
            Ok(removed)
        })
        .await
        .map_err(|e| BackupError::Io(e.to_string()))?
    }
}
