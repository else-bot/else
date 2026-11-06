//! Bot creation, listing, lookup, and soft deletion.
//!
//! Creation is idempotent under a stable request ID: a matching retry returns
//! the same bot and a changed payload conflicts. The owner supplies that ID;
//! bots' creations derive it from the owner turn and the name.
//! Deletion commits a tombstone and clears the saved default in one
//! transaction before returning; history and conversations are retained.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

use super::installation::insert_bot;
use crate::app::bots::{Avatar, AvatarColor, AvatarShape, BotName, BotPurpose};
use crate::app::providers::Profile;

mod sidebar;

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
    pub conversation_id: Uuid,
    pub status: BotStatus,
    pub created_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub selected_provider_id: Option<String>,
    pub cancel_pending: bool,
    pub active_turn_id: Option<Uuid>,
    pub active_turn_status: Option<String>,
    pub queued_count: i64,
    /// Its sidebar category, if any.
    pub category_id: Option<Uuid>,
    /// When the newest owner or bot message in its owner conversation was
    /// created; `None` before the first message.
    pub last_message_at: Option<DateTime<Utc>>,
    pub metadata_revision: i64,
    avatar_shape: Option<AvatarShape>,
    avatar_color: Option<AvatarColor>,
    #[sqlx(flatten)]
    pub read: super::reads::ReadRow,
    #[sqlx(flatten)]
    pub sidebar: sidebar::SidebarRow,
}

impl BotRecord {
    /// The chosen avatar, or the one derived from the ID for bots saved
    /// before avatars existed.
    pub fn avatar(&self) -> Avatar {
        match (self.avatar_shape, self.avatar_color) {
            (Some(shape), Some(color)) => Avatar { shape, color },
            _ => Avatar::derived(self.id),
        }
    }
}

const SELECT_BOT: &str = "SELECT b.id, b.name, c.id AS conversation_id, b.status,
        b.created_at, b.deleted_at, b.selected_provider_id, b.cancel_pending, b.category_id,
        b.avatar_shape, b.avatar_color,
        c.read_item_seq, c.read_text_bytes, c.marked_unread,
        h.id AS head_item_id, h.turn_id AS head_turn_id, h.seq AS head_seq,
        length(CAST(h.text AS BLOB)) AS head_bytes, substr(h.text, 1, 240) AS head_preview,
        a.id AS active_turn_id, a.status AS active_turn_status,
        question.kind AS question_kind, question.title AS question_title,
        r.reason AS credential_reason,
        (SELECT t.status FROM turns t WHERE t.conversation_id = c.id
            ORDER BY t.turn_no DESC LIMIT 1) AS latest_turn_status,
        (SELECT count(*) FROM turns q WHERE q.bot_id = b.id AND q.status = 'queued') AS queued_count,
        (SELECT m.created_at FROM items m WHERE m.conversation_id = c.id
            AND m.kind IN ('user', 'assistant') AND NOT m.hidden ORDER BY m.seq DESC LIMIT 1) AS last_message_at,
        coalesce((SELECT e.seq FROM bot_events e WHERE e.bot_id = b.id
            AND e.kind = 'bot.updated' ORDER BY e.seq DESC LIMIT 1), 0) AS metadata_revision
    FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
    LEFT JOIN items h ON h.id = (SELECT x.id FROM items x WHERE x.conversation_id = c.id
                                 AND x.kind = 'assistant' ORDER BY x.seq DESC LIMIT 1)
    LEFT JOIN owner_questions question ON question.id = (
        SELECT p.id FROM owner_questions p JOIN turns t ON t.id = p.turn_id
        WHERE p.bot_id = b.id AND p.status = 'pending' AND t.conversation_id = c.id
        ORDER BY p.created_at, p.id LIMIT 1)
    LEFT JOIN secret_requests r ON r.id = (
        SELECT p.id FROM secret_requests p JOIN turns t ON t.id = p.turn_id
        WHERE p.bot_id = b.id AND p.status = 'pending' AND t.conversation_id = c.id
        ORDER BY p.created_at, p.id LIMIT 1)
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
    #[error("the bot was deleted")]
    Deleted,
    #[error(transparent)]
    Refused(#[from] Refusal),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Why a turn may not create or rename a bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("bots can be created only in an owner conversation for an owner request")]
    NotOwnerConversation,
    #[error("bots can be created or renamed only for work rooted in an owner request")]
    NotOwnerRequest,
    #[error("a purpose is required when another bot creates a bot")]
    PurposeRequired,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("this attempt already delegated; end it and wait for the result")]
    AttemptDelegated,
}

pub struct NewBot<'a> {
    pub name: &'a BotName,
    pub profile: Option<(&'a str, &'a Profile)>,
    pub category_id: Option<Uuid>,
    pub avatar: Option<Avatar>,
}

fn create_hash(bot: &NewBot<'_>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"create_bot\0");
    hasher.update(bot.name.as_str().as_bytes());
    if bot.profile.is_some() || bot.category_id.is_some() {
        hasher.update(b"\0options\0");
        hasher.update(
            serde_json::to_vec(&(bot.profile, bot.category_id)).expect("serializable options"),
        );
    }
    if let Some(avatar) = bot.avatar {
        hasher.update(b"\0avatar\0");
        hasher.update(serde_json::to_vec(&avatar).expect("serializable avatar"));
    }
    hasher.finalize().to_vec()
}

pub async fn create(
    pool: &SqlitePool,
    request_id: Uuid,
    bot: NewBot<'_>,
) -> Result<Created, BotError> {
    create_as(pool, request_id, bot, None).await
}

/// Create a bot while an owner-request turn is running. Retries of the
/// same name, including continuation attempts, return the same bot even if
/// it was deleted. The bot uses installation defaults.
pub async fn create_for_turn(
    pool: &SqlitePool,
    turn_id: Uuid,
    name: &BotName,
    purpose: Option<&BotPurpose>,
    category_id: Option<Uuid>,
) -> Result<Created, BotError> {
    let mut hasher = Sha256::new();
    hasher.update(b"else.bot turn create_bot\0");
    hasher.update(turn_id.as_bytes());
    hasher.update(name.as_str().as_bytes());
    let digest = hasher.finalize();
    let request_id =
        uuid::Builder::from_custom_bytes(digest[..16].try_into().expect("16 bytes")).into_uuid();
    let bot = NewBot {
        name,
        profile: None,
        category_id,
        avatar: None,
    };
    create_as(pool, request_id, bot, Some((turn_id, purpose))).await
}

async fn authorize(
    conn: &mut sqlx::SqliteConnection,
    turn_id: Uuid,
) -> Result<super::turns::AskingCaller, BotError> {
    let caller = super::turns::asking_caller(conn, turn_id)
        .await?
        .ok_or(Refusal::NotRunning)?;
    if !caller.owner_conversation {
        return Err(Refusal::NotOwnerConversation.into());
    }
    if super::delegations::origin(conn, turn_id).await?
        != crate::app::providers::Origin::OwnerRequest
    {
        return Err(Refusal::NotOwnerRequest.into());
    }
    if super::turns::attempt_parked(conn, turn_id, caller.attempt).await? {
        return Err(Refusal::AttemptDelegated.into());
    }
    Ok(caller)
}

async fn create_as(
    pool: &SqlitePool,
    request_id: Uuid,
    bot: NewBot<'_>,
    creation: Option<(Uuid, Option<&BotPurpose>)>,
) -> Result<Created, BotError> {
    let mut hash = create_hash(&bot);
    if let Some((_, Some(purpose))) = creation {
        hash = Sha256::digest([hash.as_slice(), purpose.as_str().as_bytes()].concat()).to_vec();
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some((turn_id, purpose)) = creation {
        let caller = authorize(&mut tx, turn_id).await?;
        if !caller.main_bot && purpose.is_none() {
            return Err(Refusal::PurposeRequired.into());
        }
    }
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
    insert_bot(
        &mut tx,
        bot_id,
        bot.name,
        bot.avatar.unwrap_or_else(|| Avatar::derived(bot_id)),
    )
    .await?;
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
    if let Some((turn_id, Some(purpose))) = creation {
        creation_receipt(&mut tx, turn_id, bot_id, bot.name, purpose, &hash).await?;
    }
    super::events::notify(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(Created::New(
        find(pool, bot_id).await?.ok_or(BotError::NotFound)?,
    ))
}

async fn creation_receipt(
    conn: &mut sqlx::SqliteConnection,
    source_turn: Uuid,
    bot_id: Uuid,
    name: &BotName,
    purpose: &BotPurpose,
    hash: &[u8],
) -> Result<(), BotError> {
    let (creator, creator_name): (Uuid, String) = sqlx::query_as(
        "SELECT b.id, b.name FROM turns t JOIN bots b ON b.id = t.bot_id WHERE t.id = ?1",
    )
    .bind(source_turn)
    .fetch_one(&mut *conn)
    .await?;
    let (main, conversation): (Uuid, Uuid) = sqlx::query_as(
        "SELECT b.id, c.id FROM installation i JOIN bots b ON b.id = i.default_bot_id
         JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner' WHERE b.status = 'active'",
    )
    .fetch_one(&mut *conn)
    .await?;
    let text = format!(
        "{creator_name} (bot_id {creator}) created {} (bot_id {bot_id}).\nPurpose: {}",
        name.as_str(),
        purpose.as_str()
    );
    for target in [main, bot_id] {
        super::memory::append(
            conn,
            target,
            &super::memory::NewEntry {
                request_key: &format!("bot-created:{bot_id}"),
                kind: super::memory::Kind::ToolActivity,
                body: &text,
                source: serde_json::json!({"tool": "create_bot", "creator_bot_id": creator,
                "source_turn_id": source_turn, "bot_id": bot_id}),
                supersedes_seq: None,
            },
        )
        .await
        .map_err(|err| match err {
            super::memory::AppendError::Db(err) => err,
            err => sqlx::Error::Protocol(err.to_string()),
        })?;
    }
    if creator == main {
        return Ok(());
    }
    let (turn_no, seq): (i64, i64) = sqlx::query_as(
        "UPDATE conversations SET last_turn_no = last_turn_no + 1, last_item_seq = last_item_seq + 1
         WHERE id = ?1 RETURNING last_turn_no, last_item_seq",
    ).bind(conversation).fetch_one(&mut *conn).await?;
    let receipt_turn = Uuid::new_v4();
    let item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
         owner_item_id, provider_id, status, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'system', 'completed', strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))",
    ).bind(receipt_turn).bind(main).bind(conversation).bind(bot_id).bind(hash)
        .bind(turn_no).bind(item).execute(&mut *conn).await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
        VALUES (?1, ?2, ?3, ?4, 'status', ?5)",
    )
    .bind(item)
    .bind(conversation)
    .bind(seq)
    .bind(receipt_turn)
    .bind(text)
    .execute(&mut *conn)
    .await?;
    super::events::item_upsert(conn, main, item).await?;
    Ok(())
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
    /// Sidebar categories, in the same snapshot as the bots.
    pub categories: Vec<super::categories::Category>,
    pub bots: Vec<BotRecord>,
}

/// Active bots, plus tombstones whose shutdown is still pending when
/// requested. Completed tombstones never appear. Ordered oldest first.
pub async fn list(pool: &SqlitePool, include_deleting: bool) -> Result<Listing, sqlx::Error> {
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

/// Owner edits to a bot's metadata. Absent fields keep their value.
pub struct Patch<'a> {
    pub name: Option<&'a BotName>,
    pub avatar: Option<Avatar>,
}

/// Change an active bot's name, avatar, or both at once. Saving the values
/// it already shows changes nothing and journals nothing; a change journals
/// one `bot.updated` in the same transaction. A deleted bot is refused and
/// never revived.
pub async fn update(
    pool: &SqlitePool,
    bot_id: Uuid,
    patch: Patch<'_>,
) -> Result<BotRecord, BotError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot = update_in(&mut tx, bot_id, patch).await?;
    tx.commit().await?;
    Ok(bot)
}

pub async fn rename_for_turn(
    pool: &SqlitePool,
    turn_id: Uuid,
    bot_id: Uuid,
    name: &BotName,
) -> Result<BotRecord, BotError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = super::turns::asking_caller(&mut tx, turn_id)
        .await?
        .ok_or(Refusal::NotRunning)?;
    if super::delegations::origin(&mut tx, turn_id).await?
        != crate::app::providers::Origin::OwnerRequest
    {
        return Err(Refusal::NotOwnerRequest.into());
    }
    if super::turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
        return Err(Refusal::AttemptDelegated.into());
    }
    let bot = update_in(
        &mut tx,
        bot_id,
        Patch {
            name: Some(name),
            avatar: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(bot)
}

async fn update_in(
    conn: &mut sqlx::SqliteConnection,
    bot_id: Uuid,
    patch: Patch<'_>,
) -> Result<BotRecord, BotError> {
    let current = find_in(conn, bot_id).await?.ok_or(BotError::NotFound)?;
    if current.status != BotStatus::Active {
        return Err(BotError::Deleted);
    }
    let name = patch.name.map_or(current.name.as_str(), BotName::as_str);
    let avatar = patch.avatar.unwrap_or(current.avatar());
    if name == current.name && avatar == current.avatar() {
        return Ok(current);
    }
    sqlx::query(
        "UPDATE bots SET name = ?2,
             avatar_shape = coalesce(?3, avatar_shape), avatar_color = coalesce(?4, avatar_color)
         WHERE id = ?1",
    )
    .bind(bot_id)
    .bind(name)
    .bind(patch.avatar.map(|a| a.shape))
    .bind(patch.avatar.map(|a| a.color))
    .execute(&mut *conn)
    .await?;
    super::events::bot_updated(conn, bot_id).await?;
    find_in(conn, bot_id).await?.ok_or(BotError::NotFound)
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
            super::events::notify(&mut tx, bot_id).await?;
        }
        Some(BotStatus::Deleting | BotStatus::Deleted) => {}
    }
    tx.commit().await?;
    let bot = find(pool, bot_id).await?.ok_or(BotError::NotFound)?;
    Ok(Deleted { bot, interrupt })
}
