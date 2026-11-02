//! The one serialized backup worker. It runs inside the API on a schedule
//! (a missed set runs once at startup, not once per missed interval),
//! retries failures with a bounded backoff, keeps the newest completed
//! local sets, and records every run. A session advisory lock serializes it
//! with a manual `else-api backup` run.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use super::capture::{BackupConfig, SetSummary, create_set};
use super::{BackupError, PgParams};
use crate::credentials::SecretKey;

const LOCK: &str = "SELECT pg_try_advisory_lock(hashtext('else.bot/backup'))";
const UNLOCK: &str = "SELECT pg_advisory_unlock(hashtext('else.bot/backup'))";
const FIRST_RETRY: Duration = Duration::from_secs(60);
const MAX_RETRY: Duration = Duration::from_secs(3600);

pub struct BackupWorker {
    pool: PgPool,
    params: PgParams,
    key: Arc<SecretKey>,
    pub config: BackupConfig,
}

impl BackupWorker {
    pub fn new(pool: PgPool, params: PgParams, key: Arc<SecretKey>, config: BackupConfig) -> Self {
        Self {
            pool,
            params,
            key,
            config,
        }
    }

    /// Capture one set now, record the run, then apply retention.
    pub async fn run_once(&self) -> Result<SetSummary, BackupError> {
        let mut lock = self.pool.acquire().await?;
        let locked: bool = sqlx::query_scalar(LOCK).fetch_one(&mut *lock).await?;
        if !locked {
            return Err(BackupError::Refused("another backup is running".into()));
        }
        let result = self.locked_run().await;
        let _ = sqlx::query_scalar::<_, bool>(UNLOCK)
            .fetch_one(&mut *lock)
            .await;
        result
    }

    async fn locked_run(&self) -> Result<SetSummary, BackupError> {
        let run = uuid::Uuid::new_v4();
        let set_id = super::new_set_id();
        sqlx::query("INSERT INTO backup_runs (id, set_id) VALUES ($1, $2)")
            .bind(run)
            .bind(&set_id)
            .execute(&self.pool)
            .await?;
        let result = create_set(&self.pool, &self.params, &self.key, &self.config, &set_id).await;
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
                let removed =
                    tokio::task::spawn_blocking(move || super::apply_retention(&dir, keep))
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

    /// Startup housekeeping: runs left `running` by an earlier process are
    /// failed, and staging or unfinished sets are removed.
    pub async fn recover(&self) -> Result<usize, BackupError> {
        sqlx::query(
            "UPDATE backup_runs SET local_status = 'failed', finished_at = now(),
                 error = 'interrupted by an API restart'
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
            let result = tokio::select! {
                // Shutdown drops the capture; staging is cleaned next start.
                _ = cancel.cancelled() => return,
                result = self.run_once() => result,
            };
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
