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
use sqlx::types::Json;
use uuid::Uuid;

use super::installation::insert_bot;
use super::turns::TurnStatus;
use crate::app::bots::{Avatar, AvatarColor, AvatarShape, BotName, BotPurpose};
use crate::app::providers::Profile;

mod impact;
mod sidebar;

pub use impact::{Impact, impact};

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct StoredBotCreated {
    bot_id: Uuid,
    name: String,
    creator_bot_id: Uuid,
    creator_name: String,
    purpose: String,
}

impl From<&StoredBotCreated> for else_wire::items::BotCreated {
    fn from(value: &StoredBotCreated) -> Self {
        Self {
            bot_id: value.bot_id,
            name: value.name.clone(),
            creator_bot_id: value.creator_bot_id,
            creator_name: value.creator_name.clone(),
            purpose: value.purpose.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct WaitingBot {
    id: Uuid,
    name: String,
    tasks: u32,
}

impl From<&WaitingBot> for else_wire::bots::WaitingBot {
    fn from(value: &WaitingBot) -> Self {
        Self {
            id: value.id,
            name: value.name.clone(),
            tasks: value.tasks,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WaitingOn {
    since: DateTime<Utc>,
    bots: Vec<WaitingBot>,
    jobs: u32,
}

impl From<&WaitingOn> for else_wire::bots::WaitingOn {
    fn from(waiting: &WaitingOn) -> Self {
        Self {
            since: waiting.since,
            bots: waiting.bots.iter().map(Into::into).collect(),
            jobs: waiting.jobs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Serialize, serde::Deserialize)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum BotStatus {
    Active,
    /// Tombstoned; shutdown of its work is still pending.
    Deleting,
    /// Tombstoned and shut down. Data is retained.
    Deleted,
}

impl BotStatus {
    pub fn is_active(self) -> bool {
        self == Self::Active
    }
}

/// What a `request_receipts` row records, as stored in its `operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub(crate) enum ReceiptOperation {
    CreateBot,
    Cancel,
}

/// The bot's status, or `None` when no such bot exists.
pub async fn status<'e>(
    conn: impl sqlx::SqliteExecutor<'e>,
    bot_id: Uuid,
) -> Result<Option<BotStatus>, sqlx::Error> {
    sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_optional(conn)
        .await
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
    pub active_turn_status: Option<TurnStatus>,
    pub queued_count: i64,
    /// Its active watchers.
    pub watching: i64,
    /// Its sidebar category, if any.
    pub category_id: Option<Uuid>,
    /// The owner silenced it: no notifications or unread state while it
    /// waits on nothing from the owner.
    pub muted: bool,
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
    /// Set while its active turn is parked on tasks it handed to other bots
    /// or on its own MCP and computer jobs.
    pub waiting_on: Option<Json<WaitingOn>>,
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

/// SQL over a turn `t` of an owner conversation: it ended, or it is parked
/// on a request to the owner. Replies of a turn still working are progress
/// updates, so they are neither the latest reply nor unread.
macro_rules! handed_back {
    () => {
        "(t.status IN ('completed', 'interrupted', 'failed', 'uncertain')
            OR EXISTS (SELECT 1 FROM owner_questions p WHERE p.turn_id = t.id AND p.status = 'pending')
            OR EXISTS (SELECT 1 FROM computer_handoffs p WHERE p.turn_id = t.id AND p.status = 'pending')
            OR EXISTS (SELECT 1 FROM skill_installs p WHERE p.turn_id = t.id AND p.status = 'pending')
            OR EXISTS (SELECT 1 FROM secret_requests p WHERE p.turn_id = t.id AND p.status = 'pending')
            OR EXISTS (SELECT 1 FROM social_setups p WHERE p.turn_id = t.id AND p.status = 'pending')
            OR EXISTS (SELECT 1 FROM outgoing_drafts p WHERE p.turn_id = t.id AND p.status = 'pending'))"
    };
}

const SELECT_BOT: &str = concat!(
    "SELECT b.id, b.name, c.id AS conversation_id, b.status,
        b.created_at, b.deleted_at, b.selected_provider_id, b.cancel_pending, b.category_id, b.muted,
        b.avatar_shape, b.avatar_color,
        c.read_item_seq, c.read_text_bytes, c.marked_unread,
        h.id AS head_item_id, h.turn_id AS head_turn_id, h.seq AS head_seq,
        length(CAST(h.text AS BLOB)) AS head_bytes, h.text AS head_preview,
        a.id AS active_turn_id, a.status AS active_turn_status,
        question.kind AS question_kind, question.title AS question_title,
        question.created_at AS question_at,
        handoff.reason AS handoff_reason, handoff.created_at AS handoff_at,
        r.reason AS credential_reason, r.created_at AS credential_at,
        setup.provider AS setup_provider, setup.created_at AS setup_at,
        (SELECT min(k.taken_at) FROM computer_handoffs k JOIN turns t ON t.id = k.turn_id
            WHERE k.bot_id = b.id AND k.status = 'taken'
              AND t.status IN ('preparing', 'sending', 'running', 'cancelling', 'waiting'))
            AS computer_taken_at,
        'Install skill ' || install.name AS skill_install_title,
        install.created_at AS skill_install_at,
        draft.content AS draft_content, draft.created_at AS draft_at,
        notice.id AS notice_id, notice.summary AS notice_summary, notice.created_at AS notice_at,
        (SELECT t.status FROM turns t WHERE t.conversation_id = c.id
            ORDER BY t.turn_no DESC LIMIT 1) AS latest_turn_status,
        coalesce((SELECT (i.seq, length(CAST(i.text AS BLOB)))
                    <= (c.read_item_seq, c.read_text_bytes) AND NOT c.marked_unread
            FROM items i WHERE i.kind = 'assistant' AND i.turn_id = (
                SELECT t.id FROM turns t WHERE t.conversation_id = c.id
                ORDER BY t.turn_no DESC LIMIT 1)
            ORDER BY i.seq DESC LIMIT 1), false) AS latest_turn_read,
        (SELECT count(*) FROM items u JOIN turns t ON t.id = u.turn_id
            WHERE u.conversation_id = c.id AND u.kind = 'assistant' AND NOT u.silent AND ",
    handed_back!(),
    "
            AND NOT EXISTS (SELECT 1 FROM items v WHERE v.turn_id = u.turn_id
                            AND v.kind = 'assistant' AND v.seq > u.seq)
            AND u.seq >= c.read_item_seq
            AND (u.seq > c.read_item_seq OR length(CAST(u.text AS BLOB)) > c.read_text_bytes))
            AS unread_count,
        (SELECT count(*) FROM turns q WHERE q.bot_id = b.id AND q.status = 'queued'
            AND NOT EXISTS (SELECT 1 FROM turn_steers s
                            WHERE s.turn_id = q.id AND s.status <> 'pending')) AS queued_count,
        (SELECT count(*) FROM watchers wt WHERE wt.bot_id = b.id
            AND wt.status IN ('waiting', 'checking')) AS watching,
        (SELECT m.created_at FROM items m WHERE m.conversation_id = c.id
            AND m.kind IN ('user', 'assistant') AND NOT m.hidden AND NOT m.silent
            ORDER BY m.seq DESC LIMIT 1) AS last_message_at,
        (SELECT CASE WHEN count(g.id) > 0 OR j.jobs > 0 THEN json_object(
                'since', min(coalesce(min(g.since), j.since), coalesce(j.since, min(g.since))),
                'bots', CASE WHEN count(g.id) > 0 THEN json_group_array(
                    json_object('id', g.id, 'name', g.name, 'tasks', g.tasks)) ELSE json('[]') END,
                'jobs', j.jobs) END
            FROM (SELECT count(*) AS jobs, min(w.created_at) AS since
                  FROM (SELECT created_at FROM mcp_waits WHERE turn_id = a.id AND status = 'pending'
                        UNION ALL
                        SELECT created_at FROM computer_job_waits
                        WHERE turn_id = a.id AND status = 'pending') w) j
            LEFT JOIN (SELECT lower(hex(d.target_bot_id)) AS id, t.name, count(*) AS tasks,
                         min(d.created_at) AS since
                  FROM delegations d JOIN bots t ON t.id = d.target_bot_id
                  WHERE d.root_turn_id = a.id AND d.status IN ('queued', 'running')
                  GROUP BY d.target_bot_id ORDER BY min(d.created_at), d.target_bot_id) g ON true
            WHERE a.status = 'waiting') AS waiting_on,
        coalesce((SELECT e.seq FROM bot_events e WHERE e.bot_id = b.id
            AND e.kind = 'bot.updated' ORDER BY e.seq DESC LIMIT 1), 0) AS metadata_revision
    FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
    LEFT JOIN items h ON h.id = (SELECT x.id FROM items x JOIN turns t ON t.id = x.turn_id
                                 WHERE x.conversation_id = c.id AND x.kind = 'assistant'
                                 AND NOT x.silent AND ",
    handed_back!(),
    " ORDER BY x.seq DESC LIMIT 1)
    LEFT JOIN owner_questions question ON question.id = (
        SELECT p.id FROM owner_questions p
        WHERE p.bot_id = b.id AND p.status = 'pending'
        ORDER BY p.created_at, p.id LIMIT 1)
    LEFT JOIN computer_handoffs handoff ON handoff.bot_id = b.id AND handoff.status = 'pending'
    LEFT JOIN skill_installs install ON install.id = (
        SELECT p.id FROM skill_installs p
        WHERE p.bot_id = b.id AND p.status = 'pending'
        ORDER BY p.created_at, p.id LIMIT 1)
    LEFT JOIN secret_requests r ON r.id = (
        SELECT p.id FROM secret_requests p
        WHERE p.bot_id = b.id AND p.status = 'pending'
        ORDER BY p.created_at, p.id LIMIT 1)
    LEFT JOIN social_setups setup ON setup.id = (
        SELECT p.id FROM social_setups p
        WHERE p.bot_id = b.id AND p.status = 'pending'
        ORDER BY p.created_at, p.id LIMIT 1)
    LEFT JOIN outgoing_drafts draft ON draft.id = (
        SELECT d.id FROM outgoing_drafts d JOIN turns t ON t.id = d.turn_id
        WHERE d.bot_id = b.id AND d.status = 'pending'
            AND t.status IN ('queued', 'preparing', 'sending', 'running', 'waiting')
        ORDER BY d.created_at, d.id LIMIT 1)
    LEFT JOIN owner_notices notice ON notice.id = (
        SELECT n.id FROM owner_notices n
        WHERE n.bot_id = b.id AND n.status = 'pending'
        ORDER BY n.created_at, n.id LIMIT 1)
    LEFT JOIN turns a ON a.bot_id = b.id
        AND a.status IN ('preparing', 'sending', 'running', 'cancelling', 'waiting')
        AND a.id = (SELECT w.id FROM turns w WHERE w.bot_id = b.id
                    AND w.status IN ('preparing', 'sending', 'running', 'cancelling', 'waiting')
                    ORDER BY w.status = 'waiting', w.created_at LIMIT 1)"
);

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
    #[error("a bot cannot delete itself; the owner can delete it in the app")]
    OwnBot,
    #[error("a bot cannot delete a bot whose task it is working on")]
    Upstream,
}

pub struct NewBot<'a> {
    pub name: &'a BotName,
    pub profile: Option<(&'a str, &'a Profile)>,
    pub category_id: Option<Uuid>,
    pub avatar: Option<Avatar>,
}

/// A turn's creation passes no `profile` and hashes its model request as
/// sent instead, so a retry still matches after the defaults that filled in
/// the resolved profile change.
fn create_hash(bot: &NewBot<'_>, profile: Option<(&str, &Profile)>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"create_bot\0");
    hasher.update(bot.name.as_str().as_bytes());
    if profile.is_some() || bot.category_id.is_some() {
        hasher.update(b"\0options\0");
        hasher
            .update(serde_json::to_vec(&(profile, bot.category_id)).expect("serializable options"));
    }
    if let Some(avatar) = bot.avatar {
        hasher.update(b"\0avatar\0");
        hasher.update(serde_json::to_vec(&avatar).expect("serializable avatar"));
    }
    hasher.finalize().to_vec()
}

/// Create a bot for the owner, or return the one an earlier send of this
/// request made. `check` runs only for a new creation, inside the write
/// transaction, so a resend still finds its bot after the model catalog
/// changes, even while the first send is committing.
pub async fn create<E: From<BotError>>(
    pool: &SqlitePool,
    request_id: Uuid,
    bot: NewBot<'_>,
    check: impl FnOnce() -> Result<(), E>,
) -> Result<Created, E> {
    create_as(pool, request_id, bot, None, check).await?
}

/// Create a bot while an owner-request turn is running. Retries of the
/// same name, including continuation attempts, return the same bot even if
/// it was deleted. Without a model the bot uses installation defaults.
pub async fn create_for_turn(
    pool: &SqlitePool,
    turn_id: Uuid,
    name: &BotName,
    purpose: Option<&BotPurpose>,
    model: Option<TurnModel<'_>>,
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
        profile: model.as_ref().map(|m| (m.provider_id, m.profile)),
        category_id,
        avatar: None,
    };
    let creation = Creation {
        turn_id,
        purpose,
        model_request: model.map(|m| m.request),
    };
    create_as(pool, request_id, bot, Some(creation), || Ok(())).await?
}

/// A model a turn named for its new bot: the request as sent and the
/// profile it resolved to.
pub struct TurnModel<'a> {
    pub request: &'a str,
    pub provider_id: &'a str,
    pub profile: &'a Profile,
}

struct Creation<'a> {
    turn_id: Uuid,
    purpose: Option<&'a BotPurpose>,
    model_request: Option<&'a str>,
}

async fn authorize(
    conn: &mut sqlx::SqliteConnection,
    turn_id: Uuid,
) -> Result<super::turns::Caller, BotError> {
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

async fn create_as<E>(
    pool: &SqlitePool,
    request_id: Uuid,
    bot: NewBot<'_>,
    creation: Option<Creation<'_>>,
    check: impl FnOnce() -> Result<(), E>,
) -> Result<Result<Created, E>, BotError> {
    let mut hash = create_hash(&bot, creation.is_none().then_some(bot.profile).flatten());
    if let Some(Creation {
        purpose: Some(purpose),
        ..
    }) = &creation
    {
        hash = Sha256::digest([hash.as_slice(), purpose.as_str().as_bytes()].concat()).to_vec();
    }
    if let Some(Creation {
        model_request: Some(model),
        ..
    }) = &creation
    {
        hash = Sha256::digest([hash.as_slice(), b"\0model\0", model.as_bytes()].concat()).to_vec();
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if let Some(Creation {
        turn_id, purpose, ..
    }) = &creation
    {
        let caller = authorize(&mut tx, *turn_id).await?;
        if !caller.main_bot && purpose.is_none() {
            return Err(Refusal::PurposeRequired.into());
        }
    }
    let receipt: Option<(ReceiptOperation, Vec<u8>, Uuid)> = sqlx::query_as(
        "SELECT operation, payload_hash, bot_id FROM request_receipts WHERE request_id = ?1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if super::categories::used_by_category(&mut tx, request_id).await? {
        return Err(BotError::RequestConflict);
    }
    if let Some((operation, saved_hash, bot_id)) = receipt {
        if operation != ReceiptOperation::CreateBot || saved_hash != hash {
            return Err(BotError::RequestConflict);
        }
        tx.commit().await?;
        return Ok(Ok(Created::Existing(
            find(pool, bot_id).await?.ok_or(BotError::NotFound)?,
        )));
    }
    if let Err(err) = check() {
        return Ok(Err(err));
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
    if let Some(Creation {
        turn_id,
        purpose: Some(purpose),
        ..
    }) = creation
    {
        creation_receipt(&mut tx, turn_id, bot_id, bot.name, purpose, &hash).await?;
    }
    super::events::notify(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(Ok(Created::New(
        find(pool, bot_id).await?.ok_or(BotError::NotFound)?,
    )))
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
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text, bot_created)
        VALUES (?1, ?2, ?3, ?4, 'status', ?5, ?6)",
    )
    .bind(item)
    .bind(conversation)
    .bind(seq)
    .bind(receipt_turn)
    .bind(text)
    .bind(sqlx::types::Json(StoredBotCreated {
        bot_id,
        name: name.as_str().to_owned(),
        creator_bot_id: creator,
        creator_name,
        purpose: purpose.as_str().to_owned(),
    }))
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
    pub muted: Option<bool>,
}

/// Change an active bot's name, avatar, or mute in one save. Saving the values
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

/// Refuse unless `turn_id` is running, rooted in an owner request, and its
/// attempt has not handed off: the turns that may edit any bot.
async fn may_edit_bots(conn: &mut sqlx::SqliteConnection, turn_id: Uuid) -> Result<(), BotError> {
    let caller = super::turns::asking_caller(conn, turn_id)
        .await?
        .ok_or(Refusal::NotRunning)?;
    if super::delegations::origin(conn, turn_id).await?
        != crate::app::providers::Origin::OwnerRequest
    {
        return Err(Refusal::NotOwnerRequest.into());
    }
    if super::turns::attempt_parked(conn, turn_id, caller.attempt).await? {
        return Err(Refusal::AttemptDelegated.into());
    }
    Ok(())
}

pub async fn rename_for_turn(
    pool: &SqlitePool,
    turn_id: Uuid,
    bot_id: Uuid,
    name: &BotName,
) -> Result<BotRecord, BotError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    may_edit_bots(&mut tx, turn_id).await?;
    let bot = update_in(
        &mut tx,
        bot_id,
        Patch {
            name: Some(name),
            avatar: None,
            muted: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(bot)
}

/// What a deletion request from a turn finds: an active bot with what its
/// deletion would stop, or one already being deleted or gone.
pub enum Deletion {
    Active(Box<BotRecord>, Impact),
    Ended(BotStatus),
}

/// The bot `turn_id` may ask the owner to delete. Refuses the calling bot,
/// the bots it is doing a task for (deleting one cancels this turn), and
/// the main bot.
pub async fn deletion_for_turn(
    pool: &SqlitePool,
    turn_id: Uuid,
    bot_id: Uuid,
) -> Result<Deletion, BotError> {
    let mut conn = pool.acquire().await?;
    may_edit_bots(&mut conn, turn_id).await?;
    let caller = super::turns::asking_caller(&mut conn, turn_id)
        .await?
        .ok_or(Refusal::NotRunning)?;
    if caller.bot_id == bot_id {
        return Err(Refusal::OwnBot.into());
    }
    if super::delegations::chain(&mut conn, turn_id)
        .await?
        .upstream
        .contains(&bot_id)
    {
        return Err(Refusal::Upstream.into());
    }
    let main: Option<Uuid> = sqlx::query_scalar("SELECT default_bot_id FROM installation")
        .fetch_optional(&mut *conn)
        .await?
        .flatten();
    if main == Some(bot_id) {
        return Err(BotError::MainBot);
    }
    let bot = find_in(&mut conn, bot_id)
        .await?
        .ok_or(BotError::NotFound)?;
    if bot.status != BotStatus::Active {
        return Ok(Deletion::Ended(bot.status));
    }
    let impact = impact(&mut conn, bot_id).await?;
    Ok(Deletion::Active(Box::new(bot), impact))
}

/// Save an active bot's own model (already validated by its provider's
/// adapter) for a turn that may edit bots. Its next turn runs on it.
pub async fn set_model_for_turn(
    pool: &SqlitePool,
    turn_id: Uuid,
    bot_id: Uuid,
    provider_id: &str,
    profile: &Profile,
) -> Result<super::bot_profiles::BotProfile, BotError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    may_edit_bots(&mut tx, turn_id).await?;
    let current = find_in(&mut tx, bot_id).await?.ok_or(BotError::NotFound)?;
    if current.status != BotStatus::Active {
        return Err(BotError::Deleted);
    }
    let before = super::bot_profiles::own(&mut tx, bot_id).await?;
    let saved = super::bot_profiles::put_in(&mut tx, bot_id, provider_id, profile)
        .await
        .map_err(|err| match err {
            super::bot_profiles::BotProfileError::Db(err) => BotError::Db(err),
            super::bot_profiles::BotProfileError::NotFound => BotError::Deleted,
        })?;
    if before.as_ref() != Some(&saved) {
        super::events::bot_updated(&mut tx, bot_id).await?;
    }
    tx.commit().await?;
    Ok(saved)
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
    let muted = patch.muted.unwrap_or(current.muted);
    if name == current.name && avatar == current.avatar() && muted == current.muted {
        return Ok(current);
    }
    sqlx::query(
        "UPDATE bots SET name = ?2,
             avatar_shape = coalesce(?3, avatar_shape), avatar_color = coalesce(?4, avatar_color),
             muted = ?5
         WHERE id = ?1",
    )
    .bind(bot_id)
    .bind(name)
    .bind(patch.avatar.map(|a| a.shape))
    .bind(patch.avatar.map(|a| a.color))
    .bind(muted)
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
                super::stop::halt_accepted(&mut tx, bot_id, i64::MAX, super::stop::Halt::Delete)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_created_reads_existing_json() {
        let json = r#"{"bot_id":"56b67ab9-d0ef-4229-bd12-68e2b27d19ad","name":"Scout","creator_bot_id":"2a2b49c4-0905-4782-b685-c2d75db76d0e","creator_name":"Main","purpose":"Find open tasks."}"#;
        let stored: StoredBotCreated = serde_json::from_str(json).unwrap();
        let wire: else_wire::items::BotCreated = (&stored).into();
        assert_eq!(
            wire,
            else_wire::items::BotCreated {
                bot_id: "56b67ab9-d0ef-4229-bd12-68e2b27d19ad".parse().unwrap(),
                name: "Scout".into(),
                creator_bot_id: "2a2b49c4-0905-4782-b685-c2d75db76d0e".parse().unwrap(),
                creator_name: "Main".into(),
                purpose: "Find open tasks.".into(),
            }
        );
        assert_eq!(serde_json::to_string(&stored).unwrap(), json);
    }
}
