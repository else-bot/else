//! PostgreSQL adapter: connection, versioned migrations, and the
//! transactional operations that persist application state.

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

use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use crate::config::DatabaseUrl;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("DATABASE_URL is not a valid PostgreSQL connection URL")]
    InvalidUrl,
    #[error("cannot reach PostgreSQL: {0}")]
    Unavailable(String),
    #[error("database migration failed: {0}")]
    Migration(String),
    #[error(transparent)]
    Query(#[from] sqlx::Error),
}

/// Connect with bounded timeouts. Errors never include the URL, which may
/// carry a password.
pub async fn connect(url: &DatabaseUrl) -> Result<PgPool, DbError> {
    let options: PgConnectOptions = url.expose().parse().map_err(|_| DbError::InvalidUrl)?;
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .acquire_timeout(Duration::from_secs(5))
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
pub async fn migrate(pool: &PgPool) -> Result<(), DbError> {
    MIGRATOR
        .run(pool)
        .await
        .map_err(|err| DbError::Migration(err.to_string()))
}

/// Readiness: the database answers a trivial query.
pub async fn ready(pool: &PgPool) -> bool {
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
