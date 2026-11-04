//! Bot creation, listing, lookup, and soft deletion.
//!
//! Creation is idempotent under a stable request ID: a matching retry returns
//! the same bot and a changed payload conflicts. Deletion commits a tombstone
//! and clears the saved default in one transaction before returning; history
//! and conversations are retained.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

use super::installation::insert_bot;
use crate::app::bots::{BotName, BotRole};
use crate::app::providers::Profile;

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
    pub cancel_pending: bool,
    pub active_turn_id: Option<Uuid>,
    pub active_turn_status: Option<String>,
    pub queued_count: i64,
    /// Its sidebar category (POC-030), if any.
    pub category_id: Option<Uuid>,
}

const SELECT_BOT: &str = "SELECT b.id, b.name, b.role, c.id AS conversation_id, b.status,
        b.created_at, b.deleted_at, b.selected_provider_id, b.cancel_pending, b.category_id,
        a.id AS active_turn_id, a.status AS active_turn_status,
        (SELECT count(*) FROM turns q WHERE q.bot_id = b.id AND q.status = 'queued') AS queued_count
    FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
    LEFT JOIN turns a ON a.bot_id = b.id
        AND a.status IN ('preparing', 'sending', 'running', 'cancelling', 'waiting')
        AND a.id = (SELECT w.id FROM turns w WHERE w.bot_id = b.id
                    AND w.status IN ('preparing', 'sending', 'running', 'cancelling', 'waiting')
                    ORDER BY w.status = 'waiting', w.created_at LIMIT 1)";

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
    #[error("no such category")]
    CategoryNotFound,
    #[error("the main bot cannot be deleted")]
    MainBot,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub struct NewBot<'a> {
    pub name: &'a BotName,
    pub role: &'a BotRole,
    pub profile: Option<(&'a str, &'a Profile)>,
    pub category_id: Option<Uuid>,
}

fn create_hash(bot: &NewBot<'_>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"create_bot\0");
    hasher.update(bot.name.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(bot.role.as_str().as_bytes());
    // Keep receipts for plain creates compatible with earlier releases.
    if bot.profile.is_some() || bot.category_id.is_some() {
        hasher.update(b"\0options\0");
        hasher.update(
            serde_json::to_vec(&(bot.profile, bot.category_id)).expect("serializable options"),
        );
    }
    hasher.finalize().to_vec()
}

pub async fn create(
    pool: &SqlitePool,
    request_id: Uuid,
    bot: NewBot<'_>,
) -> Result<Created, BotError> {
    let hash = create_hash(&bot);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let receipt: Option<(String, Vec<u8>, Uuid)> = sqlx::query_as(
        "SELECT operation, payload_hash, bot_id FROM request_receipts WHERE request_id = ?1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if super::categories::used_by_category(&mut tx, request_id).await? {
        return Err(BotError::RequestConflict);
    }
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
    if let Some(category) = bot.category_id {
        let available: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM bot_categories WHERE id = ?1 AND NOT deleting)",
        )
        .bind(category)
        .fetch_one(&mut *tx)
        .await?;
        if !available {
            return Err(BotError::CategoryNotFound);
        }
    }
    insert_bot(&mut tx, bot_id, bot.name, bot.role).await?;
    if let Some((provider, profile)) = bot.profile {
        super::bot_profiles::put_in(&mut tx, bot_id, provider, profile)
            .await
            .map_err(|err| match err {
                super::bot_profiles::BotProfileError::Db(err) => BotError::Db(err),
                super::bot_profiles::BotProfileError::NotFound => BotError::NotFound,
            })?;
    }
    if let Some(category) = bot.category_id {
        sqlx::query("UPDATE bots SET category_id = ?2 WHERE id = ?1")
            .bind(bot_id)
            .bind(category)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO request_receipts (request_id, operation, bot_id, payload_hash)
         VALUES (?1, 'create_bot', ?2, ?3)",
    )
    .bind(request_id)
    .bind(bot_id)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    // Wake `bots` channel subscribers once this commits.
    super::events::notify(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(Created::New(
        find(pool, bot_id).await?.ok_or(BotError::NotFound)?,
    ))
}

pub async fn find(pool: &SqlitePool, bot_id: Uuid) -> Result<Option<BotRecord>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    find_in(&mut conn, bot_id).await
}

/// Look up a bot inside an existing transaction or connection.
pub async fn find_in(
    conn: &mut sqlx::SqliteConnection,
    bot_id: Uuid,
) -> Result<Option<BotRecord>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT_BOT} WHERE b.id = ?1")))
        .bind(bot_id)
        .fetch_optional(conn)
        .await
}

pub struct Listing {
    pub default_bot_id: Option<Uuid>,
    /// Sidebar categories (POC-030), in the same snapshot as the bots.
    pub categories: Vec<super::categories::Category>,
    pub bots: Vec<BotRecord>,
}

/// Active bots, plus tombstones whose shutdown is still pending when
/// requested. Completed tombstones never appear. Ordered oldest first.
pub async fn list(pool: &SqlitePool, include_deleting: bool) -> Result<Listing, sqlx::Error> {
    // One snapshot for both reads, so a concurrent deletion cannot pair a
    // cleared default with the bot it just deleted.
    let mut tx = pool.begin().await?;
    let bots = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT_BOT} WHERE b.status = 'active' OR (?1 AND b.status = 'deleting')
         ORDER BY b.created_at, b.id"
    )))
    .bind(include_deleting)
    .fetch_all(&mut *tx)
    .await?;
    let default_bot_id = sqlx::query_scalar("SELECT default_bot_id FROM installation")
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
    let categories = sqlx::query_as(super::categories::LIST)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Listing {
        default_bot_id,
        categories,
        bots,
    })
}

/// Result of a delete request.
pub struct Deleted {
    pub bot: BotRecord,
    /// The active turn to interrupt, when work was still running.
    pub interrupt: Option<Uuid>,
}

/// Commit the tombstone and halt all accepted work before returning. The
/// main bot (the installation's default) is refused: it is the owner's
/// entry point and is never deleted. With work still running the bot stays `deleting`
/// until its provider stops; otherwise it is `deleted` at once. Repeating it
/// returns the existing tombstone unchanged. History is retained.
pub async fn delete(pool: &SqlitePool, bot_id: Uuid) -> Result<Deleted, BotError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // Read under a share lock so the main bot cannot change meanwhile.
    let main: Option<Uuid> = sqlx::query_scalar("SELECT default_bot_id FROM installation")
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
    if main == Some(bot_id) {
        return Err(BotError::MainBot);
    }
    let status: Option<BotStatus> = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
    let mut interrupt = None;
    match status {
        None => return Err(BotError::NotFound),
        Some(BotStatus::Active) => {
            sqlx::query("UPDATE bots SET status = 'deleting', deleted_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1")
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
            interrupt =
                super::stop::halt_accepted(&mut tx, bot_id, i64::MAX, "The bot was deleted.")
                    .await?;
            super::stop::settle(&mut tx, bot_id).await?;
            // Wake this bot's streams so they close promptly.
            super::events::notify(&mut tx, bot_id).await?;
        }
        Some(BotStatus::Deleting | BotStatus::Deleted) => {}
    }
    tx.commit().await?;
    let bot = find(pool, bot_id).await?.ok_or(BotError::NotFound)?;
    Ok(Deleted { bot, interrupt })
}
