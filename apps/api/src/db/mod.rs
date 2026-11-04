//! SQLite adapter: connection, versioned migrations, and the
//! transactional operations that persist application state.

pub mod bot_profiles;
pub mod bots;
pub mod categories;
pub mod conversation;
pub mod delegations;
pub mod events;
pub mod handoffs;
pub mod installation;
pub mod memory;
pub mod memory_tree;
pub mod profiles;
pub mod stop;
pub mod task_secrets;
pub mod turns;

use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

use crate::config::DatabaseUrl;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("DATABASE_URL is not a valid SQLite connection URL")]
    InvalidUrl,
    #[error("cannot reach SQLite: {0}")]
    Unavailable(String),
    #[error("database migration failed: {0}")]
    Migration(String),
    #[error(transparent)]
    Query(#[from] sqlx::Error),
}

/// Connect with bounded timeouts. Errors never include the configured URL.
pub async fn connect(url: &DatabaseUrl) -> Result<SqlitePool, DbError> {
    let options = options(url.expose())?;
    let path = options.get_filename();
    if path != std::path::Path::new(":memory:") {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            crate::credentials::ensure_private_dir(parent)
                .map_err(|e| DbError::Unavailable(e.to_string()))?;
        }
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| DbError::Unavailable(e.to_string()))?;
    }
    // Each SQLx SQLite connection owns its handle in a worker thread and uses
    // bounded command/row channels. SQLite arbitrates writers per database;
    // readers remain concurrent in WAL mode.
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(30))
        .connect_lazy_with(options);
    // Fail fast with a readable error instead of on the first request.
    sqlx::query("SELECT 1")
        .execute(&pool)
        .await
        .map_err(|err| DbError::Unavailable(describe(&err)))?;
    Ok(pool)
}

/// Apply pending versioned migrations. Each runs in its own transaction and
/// applied versions are recorded, so repeating this is a no-op.
pub async fn migrate(pool: &SqlitePool) -> Result<(), DbError> {
    MIGRATOR
        .run(pool)
        .await
        .map_err(|err| DbError::Migration(err.to_string()))
}

/// Readiness: the database answers a trivial query.
pub async fn ready(pool: &SqlitePool) -> bool {
    sqlx::query("SELECT 1").execute(pool).await.is_ok()
}

fn describe(err: &sqlx::Error) -> String {
    match err {
        sqlx::Error::Io(io) => io.to_string(),
        sqlx::Error::PoolTimedOut => "timed out connecting".to_owned(),
        sqlx::Error::Database(db) => db.message().to_owned(),
        other => other.to_string(),
    }
}

/// Use one timestamp representation in SQL and Rust (UTC RFC3339).
/// All connections, including restore/inspection connections, enable FKs.
pub fn options(url: &str) -> Result<SqliteConnectOptions, DbError> {
    if !url.starts_with("sqlite:") {
        return Err(DbError::InvalidUrl);
    }
    let options: SqliteConnectOptions = url.parse().map_err(|_| DbError::InvalidUrl)?;
    Ok(options
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Full)
        .busy_timeout(Duration::from_secs(30))
        .command_buffer_size(64)
        .row_buffer_size(64))
}
