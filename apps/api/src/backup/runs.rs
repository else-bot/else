//! The `backup_runs` lifecycle: one row per local capture, `running` until
//! it is `completed` or `failed`, and the outcome of the set's last remote
//! run. Every transition is guarded by its source state.

use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{BackupError, Coverage};

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')";

/// `backup_runs.local_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum LocalStatus {
    Running,
    Completed,
    Failed,
}

impl From<LocalStatus> for else_wire::backups::LocalBackupStatus {
    fn from(value: LocalStatus) -> Self {
        match value {
            LocalStatus::Running => Self::Running,
            LocalStatus::Completed => Self::Completed,
            LocalStatus::Failed => Self::Failed,
        }
    }
}

/// How a set's last remote run ended, as `backup_runs.remote_status`
/// records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum RemoteOutcome {
    Completed,
    Failed,
}

/// Record a capture of `set_id` as running.
pub async fn start(pool: &SqlitePool, set_id: &str) -> Result<Uuid, BackupError> {
    let run = Uuid::new_v4();
    sqlx::query("INSERT INTO backup_runs (id, set_id) VALUES (?1, ?2)")
        .bind(run)
        .bind(set_id)
        .execute(pool)
        .await?;
    Ok(run)
}

/// Move a running capture to `completed`.
pub async fn complete(
    pool: &SqlitePool,
    run: Uuid,
    bytes: u64,
    coverage: &Coverage,
) -> Result<(), BackupError> {
    let moved = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE backup_runs SET local_status = ?2, finished_at = {NOW}, ciphertext_bytes = ?3, coverage = ?4
         WHERE id = ?1 AND local_status = ?5"
    )))
    .bind(run)
    .bind(LocalStatus::Completed)
    .bind(i64::try_from(bytes).unwrap_or(i64::MAX))
    .bind(sqlx::types::Json(coverage))
    .bind(LocalStatus::Running)
    .execute(pool)
    .await?
    .rows_affected();
    still_running(moved)
}

/// Move a running capture to `failed`.
pub async fn fail(pool: &SqlitePool, run: Uuid, error: &str) -> Result<(), BackupError> {
    let error: String = error.chars().take(500).collect();
    let moved = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE backup_runs SET local_status = ?2, finished_at = {NOW}, error = ?3
         WHERE id = ?1 AND local_status = ?4"
    )))
    .bind(run)
    .bind(LocalStatus::Failed)
    .bind(error)
    .bind(LocalStatus::Running)
    .execute(pool)
    .await?
    .rows_affected();
    still_running(moved)
}

/// Only the backup lock's holder moves a running row, so a refused move
/// means another process took the run.
fn still_running(moved: u64) -> Result<(), BackupError> {
    match moved {
        1 => Ok(()),
        _ => Err(BackupError::Refused(
            "the backup run is no longer running".into(),
        )),
    }
}

/// Settle the runs an earlier process left running: `completed` when their
/// set was `published`, `failed` otherwise.
pub async fn recover(pool: &SqlitePool, published: &[String]) -> Result<(), BackupError> {
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE backup_runs SET local_status = ?2, finished_at = {NOW}, error = NULL
         WHERE local_status = ?3 AND set_id IN (SELECT value FROM json_each(?1))"
    )))
    .bind(sqlx::types::Json(published))
    .bind(LocalStatus::Completed)
    .bind(LocalStatus::Running)
    .execute(&mut *tx)
    .await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE backup_runs SET local_status = ?1, finished_at = {NOW},
             error = 'interrupted before it finished'
         WHERE local_status = ?2"
    )))
    .bind(LocalStatus::Failed)
    .bind(LocalStatus::Running)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Record how long the capture of `set_id` paused new work.
pub async fn paused(
    pool: &SqlitePool,
    set_id: &str,
    paused: std::time::Duration,
) -> Result<(), BackupError> {
    sqlx::query("UPDATE backup_runs SET paused_ms = ?2 WHERE set_id = ?1")
        .bind(set_id)
        .bind(i64::try_from(paused.as_millis()).unwrap_or(i64::MAX))
        .execute(pool)
        .await?;
    Ok(())
}

/// Record the remote outcome of `set_id`, inside the caller's transaction.
pub async fn remote(
    conn: &mut SqliteConnection,
    set_id: &str,
    outcome: RemoteOutcome,
) -> Result<(), BackupError> {
    sqlx::query("UPDATE backup_runs SET remote_status = ?2 WHERE set_id = ?1")
        .bind(set_id)
        .bind(outcome)
        .execute(conn)
        .await?;
    Ok(())
}

/// On a restored database: the run that captured it was still running in
/// its own snapshot, and the restore proves it completed. A set made
/// without a run row has nothing to move; a row in any state but running
/// or completed means the set does not match its own history.
pub async fn restored(conn: &mut SqliteConnection, set_id: &str) -> Result<(), BackupError> {
    let status: Option<LocalStatus> =
        sqlx::query_scalar("SELECT local_status FROM backup_runs WHERE set_id = ?1")
            .bind(set_id)
            .fetch_optional(&mut *conn)
            .await?;
    match status {
        None | Some(LocalStatus::Completed) => Ok(()),
        Some(LocalStatus::Failed) => Err(BackupError::Invalid(
            "the set's own run is recorded as failed".into(),
        )),
        Some(LocalStatus::Running) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE backup_runs SET local_status = ?2, finished_at = coalesce(finished_at, {NOW}), error = NULL
                 WHERE set_id = ?1 AND local_status = ?3"
            )))
            .bind(set_id)
            .bind(LocalStatus::Completed)
            .bind(LocalStatus::Running)
            .execute(conn)
            .await?;
            Ok(())
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_a_running_capture_completes_fails_or_is_restored() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let status = |set: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, String>(
                    "SELECT local_status FROM backup_runs WHERE set_id = ?1",
                )
                .bind(set)
                .fetch_one(&pool)
                .await
                .unwrap()
            }
        };
        let done = start(&pool, "a").await.unwrap();
        complete(&pool, done, 7, &super::super::database_only_coverage())
            .await
            .unwrap();
        assert_eq!(status("a").await, "completed");
        assert!(matches!(
            fail(&pool, done, "late").await,
            Err(BackupError::Refused(_))
        ));
        assert_eq!(
            status("a").await,
            "completed",
            "a refused move changes nothing"
        );
        let failed = start(&pool, "b").await.unwrap();
        fail(&pool, failed, "disk full").await.unwrap();
        assert_eq!(status("b").await, "failed");
        let mut conn = pool.acquire().await.unwrap();
        restored(&mut conn, "missing").await.unwrap();
        assert!(matches!(
            restored(&mut conn, "b").await,
            Err(BackupError::Invalid(_))
        ));
        drop(conn);
        start(&pool, "c").await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        restored(&mut conn, "c").await.unwrap();
        drop(conn);
        assert_eq!(
            status("c").await,
            "completed",
            "a restore completes its own run"
        );
    }
}
