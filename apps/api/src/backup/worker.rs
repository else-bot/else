//! The one serialized backup worker. It runs inside the API on a schedule
//! (a missed set runs once at startup, not once per missed interval),
//! retries failures with a bounded backoff, keeps the newest completed
//! local sets, and records every run. A file lock on the backup directory
//! (released by the kernel if the process dies) serializes capture,
//! retention, and cleanup with a manual `else-api backup` run.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use super::capture::{BackupConfig, Coordinator, SetSummary, create_set};
use super::{BackupError, PgParams};
use crate::credentials::SecretKey;

const FIRST_RETRY: Duration = Duration::from_secs(60);
const MAX_RETRY: Duration = Duration::from_secs(3600);

pub struct BackupWorker {
    pool: PgPool,
    params: PgParams,
    key: Arc<SecretKey>,
    pub config: BackupConfig,
    coordinator: Coordinator,
}

impl BackupWorker {
    pub fn new(pool: PgPool, params: PgParams, key: Arc<SecretKey>, config: BackupConfig) -> Self {
        Self {
            pool,
            params,
            key,
            config,
            coordinator: Coordinator::default(),
        }
    }

    /// Coordinate with the scheduler and the bot computers: whole-assistant
    /// sets (POC-022). Without this a set is database only.
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
        let _lock = super::try_lock(&self.config.dir)?
            .ok_or_else(|| BackupError::Refused("another backup is running".into()))?;
        self.recover_locked().await?;
        self.locked_run(cancel).await
    }

    async fn locked_run(&self, cancel: &CancellationToken) -> Result<SetSummary, BackupError> {
        let run = uuid::Uuid::new_v4();
        let set_id = super::new_set_id();
        sqlx::query("INSERT INTO backup_runs (id, set_id) VALUES ($1, $2)")
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
                    "UPDATE backup_runs SET local_status = 'completed', finished_at = now(),
                         ciphertext_bytes = $2, coverage = $3 WHERE id = $1",
                )
                .bind(run)
                .bind(bytes as i64)
                .bind(sqlx::types::Json(json!(summary.manifest.coverage)))
                .execute(&self.pool)
                .await?;
                let dir = self.config.dir.clone();
                let keep = self.config.keep;
                let protect = summary.set_id.clone();
                let removed = tokio::task::spawn_blocking(move || {
                    super::apply_retention(&dir, keep, &protect)
                })
                .await
                .map_err(|e| BackupError::Io(e.to_string()))??;
                tracing::info!(set = %summary.set_id, bytes, removed = removed.len(), "backup set completed");
            }
            Err(err) => {
                let message: String = err.to_string().chars().take(500).collect();
                tracing::warn!(set = %set_id, error = %message, "backup failed");
                sqlx::query(
                    "UPDATE backup_runs SET local_status = 'failed', finished_at = now(), error = $2
                     WHERE id = $1",
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
    /// an earlier process are failed, and staging or unfinished sets this
    /// application made are removed. Skipped while a backup is running.
    pub async fn recover(&self) -> Result<usize, BackupError> {
        match super::try_lock(&self.config.dir)? {
            Some(_lock) => self.recover_locked().await,
            None => Ok(0),
        }
    }

    async fn recover_locked(&self) -> Result<usize, BackupError> {
        sqlx::query(
            "UPDATE backup_runs SET local_status = 'failed', finished_at = now(),
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

    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        if self.config.interval.is_zero() {
            return;
        }
        if let Err(err) = self.recover().await {
            tracing::warn!(error = %err, "backup recovery failed");
        }
        let mut retry = FIRST_RETRY;
        loop {
            let wait = self.next_due().unwrap_or(FIRST_RETRY);
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
            // Shutdown kills the dump and waits for it to be reaped.
            let result = self.run_once_until(&cancel).await;
            if cancel.is_cancelled() {
                return;
            }
            match result {
                Ok(_) => retry = FIRST_RETRY,
                Err(_) => {
                    let delay = retry.min(self.config.interval);
                    retry = (retry * 2).min(MAX_RETRY);
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }
}
