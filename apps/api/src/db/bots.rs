//! Bot creation, listing, lookup, and soft deletion.
//!
//! Creation is idempotent under a stable request ID: a matching retry returns
//! the same bot and a changed payload conflicts. Deletion commits a tombstone
//! and clears the saved default in one transaction before returning; history
//! and conversations are retained.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use super::installation::insert_bot;
use crate::app::bots::{BotName, BotRole};

#[derive(Debug, Clone, PartialEq, Eq, sqlx::Type, serde::Serialize)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum BotStatus {
    Active,
    /// Tombstoned; shutdown of its work is still pending.
    Deleting,
    /// Tombstoned and shut down. Data is retained.
    Deleted,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct BotRecord {
    pub id: Uuid,
    pub name: String,
    pub role: String,
    pub conversation_id: Uuid,
    pub status: BotStatus,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub selected_provider_id: Option<String>,
}

const SELECT_BOT: &str = "SELECT b.id, b.name, b.role, c.id AS conversation_id, b.status,
        b.created_at, b.deleted_at, b.selected_provider_id
    FROM bots b JOIN conversations c ON c.bot_id = b.id";

#[derive(Debug)]
pub enum Created {
    New(BotRecord),
    /// A retry with the same request ID and payload.
    Existing(BotRecord),
}

#[derive(Debug, thiserror::Error)]
pub enum BotError {
    #[error("the request ID was already used for a different request")]
    RequestConflict,
    #[error("bot not found")]
    NotFound,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

fn create_hash(name: &BotName, role: &BotRole) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"create_bot\0");
    hasher.update(name.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(role.as_str().as_bytes());
    hasher.finalize().to_vec()
}

/// Serialize all operations that use one request ID within a transaction.
pub(crate) async fn lock_request(
    tx: &mut sqlx::PgConnection,
    request_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text, 7))")
        .bind(request_id)
        .execute(tx)
        .await?;
    Ok(())
}

pub async fn create(
    pool: &PgPool,
    request_id: Uuid,
    name: &BotName,
    role: &BotRole,
) -> Result<Created, BotError> {
    let hash = create_hash(name, role);
    let mut tx = pool.begin().await?;
    lock_request(&mut tx, request_id).await?;
    let receipt: Option<(String, Vec<u8>, Uuid)> = sqlx::query_as(
        "SELECT operation, payload_hash, bot_id FROM request_receipts WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((operation, saved_hash, bot_id)) = receipt {
        if operation != "create_bot" || saved_hash != hash {
            return Err(BotError::RequestConflict);
        }
        tx.commit().await?;
        // Retries report current state, including a later deletion.
        return Ok(Created::Existing(
            find(pool, bot_id).await?.ok_or(BotError::NotFound)?,
        ));
    }
    let bot_id = Uuid::new_v4();
    insert_bot(&mut tx, bot_id, name, role).await?;
    sqlx::query(
        "INSERT INTO request_receipts (request_id, operation, bot_id, payload_hash)
         VALUES ($1, 'create_bot', $2, $3)",
    )
    .bind(request_id)
    .bind(bot_id)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Created::New(
        find(pool, bot_id).await?.ok_or(BotError::NotFound)?,
    ))
}

pub async fn find(pool: &PgPool, bot_id: Uuid) -> Result<Option<BotRecord>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT_BOT} WHERE b.id = $1")))
        .bind(bot_id)
        .fetch_optional(pool)
        .await
}

pub struct Listing {
    pub default_bot_id: Option<Uuid>,
    pub bots: Vec<BotRecord>,
}

/// Active bots, plus tombstones whose shutdown is still pending when
/// requested. Completed tombstones never appear. Ordered oldest first.
pub async fn list(pool: &PgPool, include_deleting: bool) -> Result<Listing, sqlx::Error> {
    let bots = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT_BOT} WHERE b.status = 'active' OR ($1 AND b.status = 'deleting')
         ORDER BY b.created_at, b.id"
    )))
    .bind(include_deleting)
    .fetch_all(pool)
    .await?;
    let default_bot_id = sqlx::query_scalar("SELECT default_bot_id FROM installation")
        .fetch_optional(pool)
        .await?
        .flatten();
    Ok(Listing {
        default_bot_id,
        bots,
    })
}

/// Commit the tombstone and clear a matching default before returning.
/// Repeating it returns the existing tombstone unchanged.
pub async fn delete(pool: &PgPool, bot_id: Uuid) -> Result<BotRecord, BotError> {
    let mut tx = pool.begin().await?;
    let status: Option<BotStatus> =
        sqlx::query_scalar("SELECT status FROM bots WHERE id = $1 FOR UPDATE")
            .bind(bot_id)
            .fetch_optional(&mut *tx)
            .await?;
    match status {
        None => return Err(BotError::NotFound),
        Some(BotStatus::Active) => {
            // No provider work exists yet, so shutdown completes at once.
            sqlx::query("UPDATE bots SET status = 'deleted', deleted_at = now() WHERE id = $1")
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE installation SET default_bot_id = NULL WHERE default_bot_id = $1")
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
        }
        Some(BotStatus::Deleting | BotStatus::Deleted) => {}
    }
    tx.commit().await?;
    find(pool, bot_id).await?.ok_or(BotError::NotFound)
}
