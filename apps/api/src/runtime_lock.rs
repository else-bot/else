//! Singleton guard for one installation. The API holds an exclusive lock on
//! `<data dir>/api.lock` and a PostgreSQL session advisory lock for its whole
//! lifetime, so a second API targeting the same runtime storage or database
//! refuses to start before it reconciles or dispatches anything.

use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use sqlx::postgres::PgConnection;
use sqlx::{Connection, PgPool};

const ADVISORY_KEY: &str = "else.bot/api-singleton";

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("another else.bot API is already running with data directory {0}")]
    StorageInUse(String),
    #[error("another else.bot API is already using this database")]
    DatabaseInUse,
    #[error("runtime lock {path}: {error}")]
    Io { path: String, error: std::io::Error },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Held for the process lifetime; dropping it releases both locks.
pub struct RuntimeLock {
    _file: File,
    db: Option<PgConnection>,
}

const LOCK_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

impl RuntimeLock {
    /// Watch the database session that holds the advisory lock. If it is
    /// lost (the server restarted or the connection was terminated), the lock
    /// is gone and another API could start; `lost` fires so this process
    /// stops dispatching and exits instead of competing.
    pub fn watch(&mut self, lost: tokio_util::sync::CancellationToken) {
        let Some(mut db) = self.db.take() else {
            return;
        };
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = lost.cancelled() => return,
                    _ = tokio::time::sleep(LOCK_CHECK_INTERVAL) => {}
                }
                let held: Result<bool, sqlx::Error> = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_locks WHERE locktype = 'advisory'
                         AND pid = pg_backend_pid() AND granted)",
                )
                .fetch_one(&mut db)
                .await;
                if !matches!(held, Ok(true)) {
                    tracing::error!("lost the database singleton lock; stopping this API");
                    lost.cancel();
                    return;
                }
            }
        });
    }
}

pub async fn acquire(data_dir: &Path, pool: &PgPool) -> Result<RuntimeLock, LockError> {
    let path = data_dir.join("api.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|error| LockError::Io {
            path: path.display().to_string(),
            error,
        })?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(LockError::StorageInUse(data_dir.display().to_string()));
        }
        Err(TryLockError::Error(error)) => {
            return Err(LockError::Io {
                path: path.display().to_string(),
                error,
            });
        }
    }
    // A dedicated connection outside the pool keeps the session lock.
    let mut db = PgConnection::connect_with(&pool.connect_options()).await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
        .bind(ADVISORY_KEY)
        .fetch_one(&mut db)
        .await?;
    if !locked {
        return Err(LockError::DatabaseInUse);
    }
    Ok(RuntimeLock {
        _file: file,
        db: Some(db),
    })
}
