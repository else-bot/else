//! The one serialized backup worker. It runs inside the API on a schedule
//! (a missed set runs once at startup, not once per missed interval),
//! retries failures with a bounded backoff, keeps the newest completed
//! local sets, and records every run. A file lock on the backup directory
//! (released by the kernel if the process dies) serializes capture,
//! retention, and cleanup with a manual `else-api backup` run.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use super::capture::{BackupConfig, Coordinator, SetSummary, create_set};
use super::{BackupError, DatabaseParams};
use crate::credentials::SecretKey;

const FIRST_RETRY: Duration = Duration::from_secs(60);
const MAX_RETRY: Duration = Duration::from_secs(3600);

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
        let _lock = super::try_lock(&self.config.dir)?
            .ok_or_else(|| BackupError::Refused("another backup is running".into()))?;
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
        let _lock = super::try_lock(&self.config.dir)?
            .ok_or_else(|| BackupError::Refused("another backup is running".into()))?;
        self.recover_locked().await?;
        let mut summary = self.locked_run(cancel).await?;
        if upload && let Some(replicator) = &self.replicator {
            summary.remote = Some(
                replicator
                    .replicate_pending(&self.config.dir, &self.key)
                    .await
                    .map_err(Arc::new),
            );
        }
        self.apply_local_retention(&summary.set_id).await?;
        Ok(summary)
    }

    async fn apply_local_retention(&self, newest: &str) -> Result<(), BackupError> {
        let awaiting = match &self.replicator {
            Some(replicator) => replicator.awaiting(&self.config.dir).await?,
            None => Vec::new(),
        };
        let dir = self.config.dir.clone();
        let keep = self.config.keep;
        let newest = newest.to_owned();
        let removed = tokio::task::spawn_blocking(move || {
            super::apply_retention_protecting(&dir, keep, &newest, &awaiting)
        })
        .await
        .map_err(|e| BackupError::Io(e.to_string()))??;
        if !removed.is_empty() {
            tracing::info!(removed = removed.len(), "old local backup sets removed");
        }
        Ok(())
    }

    async fn locked_run(&self, cancel: &CancellationToken) -> Result<SetSummary, BackupError> {
        let run = uuid::Uuid::new_v4();
        let set_id = super::new_set_id();
        sqlx::query("INSERT INTO backup_runs (id, set_id) VALUES (?1, ?2)")
            .bind(run)
            .bind(&set_id)
            .execute(&self.pool)
            .await?;
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
                sqlx::query(
                    "UPDATE backup_runs SET local_status = 'completed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                         ciphertext_bytes = ?2, coverage = ?3 WHERE id = ?1",
                )
                .bind(run)
                .bind(bytes as i64)
                .bind(sqlx::types::Json(json!(summary.manifest.coverage)))
                .execute(&self.pool)
                .await?;
                tracing::info!(set = %summary.set_id, bytes, "backup set completed");
            }
            Err(err) => {
                let message: String = err.to_string().chars().take(500).collect();
                tracing::warn!(set = %set_id, error = %message, "backup failed");
                sqlx::query(
                    "UPDATE backup_runs SET local_status = 'failed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), error = ?2
                     WHERE id = ?1",
                )
                .bind(run)
                .bind(message)
                .execute(&self.pool)
                .await?;
            }
        }
        result
    }

    /// How long until the next set is due: zero when none has completed or
    /// the newest is older than the interval.
    pub fn next_due(&self) -> Result<Duration, BackupError> {
        let sets = super::completed_sets(&self.config.dir)?;
        let Some(newest) = sets.last() else {
            return Ok(Duration::ZERO);
        };
        let path = self.config.dir.join(newest).join(super::COMPLETE);
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map_err(|e| BackupError::io(path.display(), e))?;
        let age = modified.elapsed().unwrap_or_default();
        Ok(self.config.interval.saturating_sub(age))
    }

    /// Startup housekeeping under the backup lock: runs left `running` by
    /// an earlier process are completed when their set was published and
    /// failed otherwise, and staging or unfinished sets this application
    /// made are removed. Skipped while a backup is running.
    pub async fn recover(&self) -> Result<usize, BackupError> {
        match super::try_lock(&self.config.dir)? {
            Some(_lock) => self.recover_locked().await,
            None => Ok(0),
        }
    }

    async fn recover_locked(&self) -> Result<usize, BackupError> {
        let dir = self.config.dir.clone();
        let published = tokio::task::spawn_blocking(move || super::completed_sets(&dir))
            .await
            .map_err(|e| BackupError::Io(e.to_string()))??;
        sqlx::query(
            "UPDATE backup_runs SET local_status = 'completed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 error = NULL
             WHERE local_status = 'running' AND set_id IN (SELECT value FROM json_each(?1))",
        )
        .bind(sqlx::types::Json(published))
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE backup_runs SET local_status = 'failed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 error = 'interrupted before it finished'
             WHERE local_status = 'running'",
        )
        .execute(&self.pool)
        .await?;
        let dir = self.config.dir.clone();
        tokio::task::spawn_blocking(move || super::clean_incomplete(&dir))
            .await
            .map_err(|e| BackupError::Io(e.to_string()))?
    }

    /// Run the schedule: captures and uploads have separate deadlines, so
    /// a bucket outage (or a conflicting old set) never stops local
    /// backups, and pending uploads start at once after a restart.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        if self.config.interval.is_zero() {
            return;
        }
        if let Err(err) = self.recover().await {
            tracing::warn!(error = %err, "backup recovery failed");
        }
        use tokio::time::Instant;
        let mut capture_retry = FIRST_RETRY;
        let mut upload_retry = FIRST_RETRY;
        let mut capture_at = Instant::now() + self.next_due().unwrap_or(FIRST_RETRY);
        let mut upload_at = match &self.replicator {
            Some(replicator) => match replicator.awaiting(&self.config.dir).await {
                Ok(awaiting) if !awaiting.is_empty() => Some(Instant::now()),
                _ => None,
            },
            None => None,
        };
        loop {
            let wake = upload_at.map_or(capture_at, |u| u.min(capture_at));
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep_until(wake) => {}
            }
            if upload_at.is_some_and(|u| u <= Instant::now()) {
                match self.replicate_now().await {
                    Ok(_) => {
                        upload_at = None;
                        upload_retry = FIRST_RETRY;
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "replication will be retried");
                        upload_at = Some(Instant::now() + upload_retry);
                        upload_retry = (upload_retry * 2).min(MAX_RETRY);
                    }
                }
            }
            if capture_at <= Instant::now() {
                let result = self.run_once_until(&cancel).await;
                if cancel.is_cancelled() {
                    return;
                }
                match result {
                    Ok(summary) => {
                        capture_retry = FIRST_RETRY;
                        capture_at =
                            Instant::now() + self.next_due().unwrap_or(self.config.interval);
                        if matches!(summary.remote, Some(Err(_))) && upload_at.is_none() {
                            upload_at = Some(Instant::now() + upload_retry);
                            upload_retry = (upload_retry * 2).min(MAX_RETRY);
                        }
                    }
                    Err(_) => {
                        capture_at = Instant::now() + capture_retry.min(self.config.interval);
                        capture_retry = (capture_retry * 2).min(MAX_RETRY);
                    }
                }
            }
        }
    }
}
