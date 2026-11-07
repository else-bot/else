pub mod attachments;
pub mod bot_profiles;
pub mod bots;
pub mod categories;
pub mod conversation;
pub mod delegations;
pub mod events;
pub mod handoffs;
pub mod installation;
pub mod mcp_waits;
pub mod memory;
pub mod memory_tree;
pub mod outgoing_drafts;
pub mod owner_questions;
pub mod profiles;
pub mod reads;
pub mod routines;
pub mod secret_requests;
pub mod stop;
pub mod task_secrets;
pub mod turn_sink;
pub mod turns;
pub mod webhooks;

use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Connection, SqliteConnection, SqlitePool};

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
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(30))
        .connect_lazy_with(options);
    sqlx::query("SELECT 1")
        .execute(&pool)
        .await
        .map_err(|err| DbError::Unavailable(describe(&err)))?;
    Ok(pool)
}

/// Apply pending versioned migrations. Each runs in its own transaction and
/// applied versions are recorded, so repeating this is a no-op.
pub async fn migrate(pool: &SqlitePool) -> Result<(), DbError> {
    let mut conn = pool.acquire().await?;
    migrate_in(&mut conn).await
}

/// [`migrate`] on one connection, then the data steps migrations leave to
/// Rust. Every step converges, so an interrupted run is finished by the next.
pub async fn migrate_in(conn: &mut SqliteConnection) -> Result<(), DbError> {
    MIGRATOR
        .run(&mut *conn)
        .await
        .map_err(|err| DbError::Migration(err.to_string()))?;
    import_legacy_descriptions(conn).await?;
    import_legacy_task_secrets(conn).await
}

/// Give each duplicate a bot stored before task secrets were shared a free
/// name (`<name>-<bot id prefix>`, numbered if that is taken), and point
/// that bot's jobs at it so their output is still redacted with its values.
async fn import_legacy_task_secrets(conn: &mut SqliteConnection) -> Result<(), DbError> {
    let staged: Vec<(uuid::Uuid, String)> =
        sqlx::query_as("SELECT bot_id, name FROM legacy_task_secrets ORDER BY bot_id, name")
            .fetch_all(&mut *conn)
            .await?;
    for (bot_id, name) in staged {
        let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
        let prefix = &bot_id.simple().to_string()[..8];
        let mut renamed = String::new();
        for n in 1.. {
            let suffix = if n == 1 {
                format!("-{prefix}")
            } else {
                format!("-{prefix}-{n}")
            };
            renamed = format!("{}{suffix}", &name[..name.len().min(40 - suffix.len())]);
            let taken: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM task_secrets WHERE name = ?1)")
                    .bind(&renamed)
                    .fetch_one(&mut *tx)
                    .await?;
            if !taken {
                break;
            }
        }
        sqlx::query(
            "INSERT INTO task_secrets (name, env_names, salt, ciphertext, created_at, updated_at)
             SELECT ?3, env_names, salt, ciphertext, created_at, updated_at
             FROM legacy_task_secrets WHERE bot_id = ?1 AND name = ?2",
        )
        .bind(bot_id)
        .bind(&name)
        .bind(&renamed)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE guest_jobs SET secrets = (
                 SELECT json_group_array(CASE WHEN value = ?2 THEN ?3 ELSE value END)
                 FROM json_each(guest_jobs.secrets))
             WHERE bot_id = ?1 AND EXISTS (SELECT 1 FROM json_each(guest_jobs.secrets) WHERE value = ?2)",
        )
        .bind(bot_id)
        .bind(&name)
        .bind(&renamed)
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM legacy_task_secrets WHERE bot_id = ?1 AND name = ?2")
            .bind(bot_id)
            .bind(&name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

async fn import_legacy_descriptions(conn: &mut SqliteConnection) -> Result<(), DbError> {
    use memory::{AppendError, Kind, NewEntry};
    let staged: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT l.bot_id FROM legacy_bot_descriptions l JOIN bots b ON b.id = l.bot_id
         WHERE b.status = 'active' ORDER BY l.bot_id",
    )
    .fetch_all(&mut *conn)
    .await?;
    for bot_id in staged {
        let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
        let description: Option<String> =
            sqlx::query_scalar("SELECT description FROM legacy_bot_descriptions WHERE bot_id = ?1")
                .bind(bot_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(description) = description else {
            continue;
        };
        let body = format!(
            "[else.bot upgrade] This bot's former description, from before bots learned \
             their responsibilities from memory. The owner or the main bot wrote it; it is \
             history, not an owner instruction, and grants nothing.\n\n{description}"
        );
        let entry = NewEntry {
            request_key: "migration:0003:bot_description",
            kind: Kind::ToolActivity,
            body: &body,
            source: serde_json::json!({ "source": "migration", "migration": 3,
                                        "field": "bots.role" }),
            supersedes_seq: None,
        };
        match memory::append(&mut tx, bot_id, &entry).await {
            Ok(_) => {}
            Err(AppendError::BotInactive) => continue,
            Err(err) => {
                return Err(DbError::Migration(format!(
                    "importing a bot description: {err}"
                )));
            }
        }
        sqlx::query("DELETE FROM legacy_bot_descriptions WHERE bot_id = ?1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::MIGRATOR;

    /// SQLx records each applied migration by its version number, so a
    /// shipped version can never be renumbered. Keep new ones contiguous.
    #[test]
    fn migration_versions_are_contiguous_from_one() {
        let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert!(
            !versions.is_empty(),
            "no migrations embedded from apps/api/migrations"
        );
        let expected: Vec<i64> = (1..=versions.len() as i64).collect();
        assert_eq!(
            versions, expected,
            "apps/api/migrations versions must be unique and contiguous from 1; \
             give a new migration the next number and never renumber a shipped one"
        );
    }
}
