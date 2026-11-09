//! Durable owner messages and turn attempts.
//!
//! Acceptance writes the owner item and its queued turn in one transaction
//! inside the immediate transaction. The scheduler claims turns with short
//! transactions; provider progress is persisted through [`super::turn_sink::SqliteSink`], whose
//! writes are fenced on the turn still being active and the bot still active.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::app::messages::{MessagePart, Reaction};
use crate::app::providers::{HistoryEntry, Profile, SessionState, TurnInput};

/// Turn statuses past the queue that hold the bot's single active slot.
pub const ACTIVE_STATUSES: &str = "('preparing', 'sending', 'running', 'cancelling')";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TurnRecord {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub request_id: Uuid,
    pub turn_no: i64,
    /// The item the turn answers: the owner's message, or the assistant's
    /// own greeting for a turn that started the conversation.
    pub source_item_id: Uuid,
    pub provider_id: String,
    pub profile_revision: Option<i64>,
    pub profile: Option<Json<Profile>>,
    pub status: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub effect_uncertain: bool,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub reaction: Option<Json<Reaction>>,
    pub attempt: i32,
    /// The bot's requests for secrets during this turn (metadata only).
    #[sqlx(skip)]
    pub secret_requests: Vec<crate::db::secret_requests::RequestInfo>,
    #[sqlx(skip)]
    pub social_setups: Vec<crate::socials::store::Setup>,
    /// The bot's questions and approval requests to the owner during this turn.
    #[sqlx(skip)]
    pub questions: Vec<crate::db::owner_questions::QuestionInfo>,
    pub routine: Option<Json<crate::app::providers::RoutineRun>>,
    /// The outgoing draft revisions shown under this turn.
    #[sqlx(skip)]
    pub outgoing_drafts: Vec<crate::db::outgoing_drafts::OutgoingDraft>,
    /// The tasks this turn handed to other bots.
    #[sqlx(skip)]
    pub delegations: Vec<crate::db::delegations::Handoff>,
    /// The MCP jobs this turn waits on.
    #[sqlx(skip)]
    pub mcp_waits: Vec<crate::db::mcp_waits::Pending>,
}

pub const TURN_COLUMNS: &str = "t.id, t.bot_id, t.conversation_id, t.request_id, t.turn_no,
    t.owner_item_id AS source_item_id, t.provider_id, t.profile_revision, t.profile, t.status,
    t.error_code, t.error_message, t.effect_uncertain, t.created_at, t.started_at, t.finished_at, t.attempt,
    (SELECT i.reaction FROM items i WHERE i.id = t.owner_item_id) AS reaction,
    (SELECT json_object('routine_id', lower(hex(r.routine_id)), 'name', r.name, 'due_at', r.due_at)
     FROM routine_runs r WHERE r.turn_id = t.id) AS routine";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BotTask {
    pub sender_bot_id: Uuid,
    pub sender_name: String,
    pub prompt: String,
}

/// A delegated bot's settled reply, shown in the delegating bot's
/// conversation. The item's ID is the delegation's.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BotReply {
    pub sender_bot_id: Uuid,
    pub sender_name: String,
    pub prompt: String,
    pub status: String,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ItemRecord {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub seq: i64,
    pub turn_id: Uuid,
    pub kind: String,
    pub hidden: bool,
    pub text: String,
    pub provider_id: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Set for the owner's message: the request that created it.
    pub request_id: Option<Uuid>,
    /// Set for an owner's message sent as a reaction to an assistant message.
    pub reaction: Option<Json<Reaction>>,
    pub bot_task: Option<Json<BotTask>>,
    pub bot_reply: Option<Json<BotReply>>,
    pub routine_update: Option<Json<RoutineUpdate>>,
    pub bot_created: Option<Json<BotCreated>>,
    pub rules_update: Option<Json<crate::app::rules::RulesUpdate>>,
    /// Ordered text and file parts; `None` for plain text.
    pub parts: Option<Json<Vec<MessagePart>>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoutineUpdate {
    pub routine_id: Uuid,
    pub name: String,
}

/// The main bot's notice that `creator_bot_id` created `bot_id`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BotCreated {
    pub bot_id: Uuid,
    pub name: String,
    pub creator_bot_id: Uuid,
    pub creator_name: String,
    pub purpose: String,
}

pub const ITEM_COLUMNS: &str = "i.id, i.conversation_id, i.seq, i.turn_id, i.kind, i.text,
    i.provider_id, i.created_at, i.hidden,
    CASE WHEN i.kind = 'user' THEN t.request_id END AS request_id, i.reaction,
    CASE WHEN i.kind = 'status' AND i.routine_update IS NULL AND i.conversation_id <> t.conversation_id THEN
        (SELECT json_object('sender_bot_id', lower(hex(d.parent_bot_id)), 'sender_name', b.name, 'prompt', d.task)
         FROM delegations d JOIN bots b ON b.id = d.parent_bot_id WHERE d.child_turn_id = i.turn_id)
    END AS bot_task,
    CASE WHEN i.kind = 'status' AND i.conversation_id = t.conversation_id THEN
        (SELECT json_object('sender_bot_id', lower(hex(d.target_bot_id)), 'sender_name', b.name, 'prompt', d.task, 'status', d.status)
         FROM delegations d JOIN bots b ON b.id = d.target_bot_id WHERE d.id = i.id)
    END AS bot_reply, coalesce(i.parts, i.assistant_parts) AS parts, i.routine_update,
    i.bot_created, i.rules_update";

#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    #[error("bot not found")]
    NotFound,
    #[error("bot deleted")]
    Deleted,
    #[error("request ID reused with different input")]
    RequestConflict,
    #[error("reaction target is not an assistant message of this conversation")]
    InvalidReaction,
    #[error("a file part is not one of this bot's attachments as stored")]
    InvalidAttachment,
    #[error("work is still stopping")]
    CancellationPending,
    #[error("the bot has unfinished work")]
    Busy,
    #[error("no saved profile for provider {0}")]
    ProfileMissing(String),
    #[error("the open draft: {0}")]
    OpenDraft(crate::db::outgoing_drafts::DraftError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub struct Accepted {
    /// False when this was a matching retry.
    pub created: bool,
    pub turn: TurnRecord,
    pub item: ItemRecord,
    /// Active turns this message preempted: the conversation's own, and
    /// delegated tasks it took over before they parked. Each is
    /// `cancelling` and must be interrupted.
    pub preempted: Vec<Uuid>,
}

pub struct SendRequest<'a> {
    pub bot_id: Uuid,
    pub request_id: Uuid,
    pub text: &'a str,
    pub hidden: bool,
    /// Canonical parts (see [`crate::app::messages::canonical_parts`]);
    /// empty for plain text.
    pub parts: &'a [MessagePart],
    pub reaction: Option<Reaction>,
    /// The provider named by the request, as sent (part of the request hash).
    pub requested_provider: Option<&'a str>,
    /// The provider the turn is bound to.
    pub provider_id: &'a str,
    pub uses_profile: bool,
    /// The outgoing draft open in the app when the owner sent this
    /// message, with their unsaved edits (part of the request hash).
    pub open_draft: Option<&'a crate::db::outgoing_drafts::Attachment>,
}

/// Text holds no NUL, so it is always the tail after the last separator.
/// A reaction gets its own prefix, so text-only hashes are unchanged.
/// Parts come first under their own prefix (their JSON holds no raw NUL),
/// so messages without files keep the hashes saved before parts existed.
pub fn request_hash(req: &SendRequest<'_>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    if req.hidden {
        hasher.update(b"hidden-owner-setup\0");
    }
    if let Some(draft) = req.open_draft {
        hasher.update(b"open-draft\0");
        hasher.update(serde_json::to_vec(draft).expect("draft attachment serializes"));
        hasher.update(b"\0");
    }
    if !req.parts.is_empty() {
        hasher.update(b"parts\0");
        hasher.update(serde_json::to_vec(req.parts).expect("parts serialize"));
        hasher.update(b"\0");
    }
    match req.reaction {
        None => hasher.update(b"turn\0"),
        Some(reaction) => {
            hasher.update(b"reaction-turn\0");
            hasher.update(reaction.item_id.hyphenated().to_string().as_bytes());
            hasher.update(b"\0");
            hasher.update(reaction.kind.as_str().as_bytes());
            hasher.update(b"\0");
        }
    }
    hasher.update(req.requested_provider.unwrap_or("").as_bytes());
    hasher.update(b"\0");
    hasher.update(req.text.as_bytes());
    hasher.finalize().to_vec()
}

pub async fn accept(pool: &SqlitePool, req: SendRequest<'_>) -> Result<Accepted, AcceptError> {
    let hash = request_hash(&req);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot: Option<(String, Uuid, i64, i64, bool)> = sqlx::query_as(
        "SELECT b.status, c.id, c.last_turn_no, c.last_item_seq, b.cancel_pending
         FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
         WHERE b.id = ?1",
    )
    .bind(req.bot_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, conversation_id, last_turn_no, last_item_seq, cancel_pending)) = bot else {
        return Err(AcceptError::NotFound);
    };
    if status != "active" {
        return Err(AcceptError::Deleted);
    }
    let existing: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT id, request_hash FROM turns WHERE conversation_id = ?1 AND request_id = ?2",
    )
    .bind(conversation_id)
    .bind(req.request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((turn_id, saved)) = existing {
        if saved != hash {
            return Err(AcceptError::RequestConflict);
        }
        let (turn, item) = load_turn_and_item(&mut tx, turn_id).await?;
        tx.commit().await?;
        return Ok(Accepted {
            created: false,
            turn,
            item,
            preempted: Vec::new(),
        });
    }
    if let Some(reaction) = req.reaction {
        let target: Option<String> =
            sqlx::query_scalar("SELECT kind FROM items WHERE id = ?1 AND conversation_id = ?2")
                .bind(reaction.item_id)
                .bind(conversation_id)
                .fetch_optional(&mut *tx)
                .await?;
        if target.as_deref() != Some("assistant") {
            return Err(AcceptError::InvalidReaction);
        }
    }
    let files: Vec<_> = req
        .parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::File { file } => Some(file),
            MessagePart::Text { .. } => None,
        })
        .collect();
    if !crate::db::attachments::all_match(&mut tx, req.bot_id, &files).await? {
        return Err(AcceptError::InvalidAttachment);
    }
    if cancel_pending {
        return Err(AcceptError::CancellationPending);
    }
    if req.hidden {
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE bot_id = ?1
             AND status IN ('queued', 'preparing', 'sending', 'running', 'cancelling', 'waiting'))",
        )
        .bind(req.bot_id)
        .fetch_one(&mut *tx)
        .await?;
        if busy {
            return Err(AcceptError::Busy);
        }
    }
    let profile: Option<(i64, Json<Profile>)> = if req.uses_profile {
        Some(
            crate::db::bot_profiles::effective(&mut tx, req.bot_id, req.provider_id)
                .await?
                .ok_or_else(|| AcceptError::ProfileMissing(req.provider_id.to_owned()))?,
        )
    } else {
        None
    };

    let turn_id = Uuid::new_v4();
    let item_id = Uuid::new_v4();
    let turn_no = last_turn_no + 1;
    let seq = last_item_seq + 1;
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, profile_revision, profile)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(turn_id)
    .bind(req.bot_id)
    .bind(conversation_id)
    .bind(req.request_id)
    .bind(&hash)
    .bind(turn_no)
    .bind(item_id)
    .bind(req.provider_id)
    .bind(profile.as_ref().map(|(revision, _)| *revision))
    .bind(profile.as_ref().map(|(_, profile)| profile))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text, reaction, parts, hidden)
         VALUES (?1, ?2, ?3, ?4, 'user', ?5, ?6, ?7, ?8)",
    )
    .bind(item_id)
    .bind(conversation_id)
    .bind(seq)
    .bind(turn_id)
    .bind(req.text)
    .bind(req.reaction.map(Json))
    .bind((!req.parts.is_empty()).then_some(Json(req.parts)))
    .bind(req.hidden)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET last_turn_no = ?2, last_item_seq = ?3 WHERE id = ?1")
        .bind(conversation_id)
        .bind(turn_no)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE bots SET selected_provider_id = ?2 WHERE id = ?1")
        .bind(req.bot_id)
        .bind(req.provider_id)
        .execute(&mut *tx)
        .await?;
    if let Some(draft) = req.open_draft {
        crate::db::outgoing_drafts::attach_to_message(&mut tx, req.bot_id, turn_id, draft)
            .await
            .map_err(|err| match err {
                crate::db::outgoing_drafts::DraftError::Db(err) => AcceptError::Db(err),
                err => AcceptError::OpenDraft(err),
            })?;
    }
    crate::db::events::turn_accepted(&mut tx, req.bot_id, turn_id).await?;
    crate::db::delegations::lock_targets_of_bot(&mut tx, req.bot_id).await?;
    crate::db::delegations::take_over(&mut tx, req.bot_id, conversation_id, turn_no, turn_id)
        .await?;
    // A delegated task shows its cards here through its receipt item; its
    // turn lives in a task conversation. One this message took over ends
    // below; any other continues with the skip.
    let skipped: Vec<(Uuid, bool)> = sqlx::query_as(
        "UPDATE owner_questions SET status = 'skipped',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status = 'pending'
           AND (turn_id IN (SELECT id FROM turns WHERE conversation_id = ?1 AND turn_no < ?2)
                OR turn_id IN (SELECT d.child_turn_id FROM delegations d
                               JOIN items i ON i.turn_id = d.child_turn_id
                               WHERE i.conversation_id = ?1))
         RETURNING turn_id, turn_id IN (SELECT child_turn_id FROM delegations
                                        WHERE takeover_turn_id IS NULL) AS delegated",
    )
    .bind(conversation_id)
    .bind(turn_no)
    .fetch_all(&mut *tx)
    .await?;
    for task in skipped
        .into_iter()
        .filter_map(|(turn, delegated)| delegated.then_some(turn))
    {
        continue_waiting(&mut tx, task).await?;
        crate::db::events::turn_changed(&mut tx, req.bot_id, task).await?;
    }
    crate::db::delegations::carry_forward(&mut tx, conversation_id, turn_no, turn_id).await?;
    let superseded: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE turns SET status = 'interrupted', error_code = 'superseded',
             error_message = 'A newer owner message continued this conversation before this one finished.',
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status IN ('queued', 'waiting')
           AND ((conversation_id = ?1 AND turn_no < ?2)
                OR id IN (SELECT child_turn_id FROM delegations WHERE takeover_turn_id = ?3))
         RETURNING id",
    )
    .bind(conversation_id)
    .bind(turn_no)
    .bind(turn_id)
    .fetch_all(&mut *tx)
    .await?;
    for superseded in superseded {
        crate::db::events::turn_changed(&mut tx, req.bot_id, superseded).await?;
        crate::db::delegations::cancel_for_root(&mut tx, superseded).await?;
    }
    let preempted: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE turns SET status = 'cancelling', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status IN ('preparing', 'sending', 'running')
           AND ((conversation_id = ?1 AND turn_no < ?2)
                OR id IN (SELECT child_turn_id FROM delegations WHERE takeover_turn_id = ?3))
         RETURNING id",
    )
    .bind(conversation_id)
    .bind(turn_no)
    .bind(turn_id)
    .fetch_all(&mut *tx)
    .await?;
    for &active in &preempted {
        crate::db::events::turn_changed(&mut tx, req.bot_id, active).await?;
    }
    // Show the tasks carried from the turns this message ended.
    crate::db::events::turn_changed(&mut tx, req.bot_id, turn_id).await?;
    crate::db::memory::log_item(&mut tx, item_id).await?;
    let (turn, item) = load_turn_and_item(&mut tx, turn_id).await?;
    tx.commit().await?;
    Ok(Accepted {
        created: true,
        turn,
        item,
        preempted,
    })
}

pub async fn load_turn(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<TurnRecord, sqlx::Error> {
    let mut turn: TurnRecord = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {TURN_COLUMNS} FROM turns t WHERE t.id = ?1"
    )))
    .bind(turn_id)
    .fetch_one(&mut *conn)
    .await?;
    attach_owner_requests(conn, std::slice::from_mut(&mut turn)).await?;
    Ok(turn)
}

/// Fill in each turn's secret requests, owner questions, drafts, social
/// setups, delegations, and pending MCP waits.
pub async fn attach_owner_requests(
    conn: &mut SqliteConnection,
    turns: &mut [TurnRecord],
) -> Result<(), sqlx::Error> {
    let ids: Vec<Uuid> = turns.iter().map(|t| t.id).collect();
    let mut socials = crate::socials::store::setups(conn, &ids).await?;
    let mut secrets = crate::db::secret_requests::for_turns(conn, &ids).await?;
    let mut questions = crate::db::owner_questions::for_turns(conn, &ids).await?;
    let mut drafts = crate::db::outgoing_drafts::for_turns(conn, &ids).await?;
    let mut delegations = crate::db::delegations::for_turns(conn, &ids).await?;
    let mut mcp_waits = crate::db::mcp_waits::pending_for_turns(conn, &ids).await?;
    for turn in turns {
        turn.social_setups = socials.remove(&turn.id).unwrap_or_default();
        turn.secret_requests = secrets.remove(&turn.id).unwrap_or_default();
        turn.questions = questions.remove(&turn.id).unwrap_or_default();
        turn.outgoing_drafts = drafts.remove(&turn.id).unwrap_or_default();
        turn.delegations = delegations.remove(&turn.id).unwrap_or_default();
        turn.mcp_waits = mcp_waits.remove(&turn.id).unwrap_or_default();
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
pub struct AskingCaller {
    pub bot_id: Uuid,
    pub attempt: i32,
    pub owner_conversation: bool,
    pub main_bot: bool,
}

pub async fn asking_caller(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Option<AskingCaller>, sqlx::Error> {
    sqlx::query_as(
        "SELECT t.bot_id, t.attempt, c.kind = 'owner' AS owner_conversation,
             EXISTS (SELECT 1 FROM installation WHERE default_bot_id = t.bot_id) AS main_bot
         FROM turns t JOIN conversations c ON c.id = t.conversation_id
         JOIN bots b ON b.id = t.bot_id
         WHERE t.id = ?1 AND t.status IN ('sending', 'running')
           AND b.status = 'active' AND NOT b.cancel_pending",
    )
    .bind(turn_id)
    .fetch_optional(conn)
    .await
}

/// Whether `caller` may show the owner a card: from the owner's own chat, or
/// from a task another bot delegated for an owner request, whose cards
/// appear in its own bot's chat.
pub async fn may_ask_owner(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    caller: &AskingCaller,
) -> Result<bool, sqlx::Error> {
    Ok(caller.owner_conversation
        || super::delegations::origin(conn, turn_id).await?
            == crate::app::providers::Origin::OwnerRequest)
}

/// Whether the current `attempt` of `turn_id` already delegated, asked for
/// a secret, or asked the owner: it must end, and asks nothing more.
pub async fn attempt_parked(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM turn_holds WHERE turn_id = ?1 AND attempt = ?2)",
    )
    .bind(turn_id)
    .bind(attempt)
    .fetch_one(&mut *conn)
    .await
}

/// Queue a waiting turn again as a continuation once nothing but its
/// delegated tasks holds it, and either none of those is pending or a batch
/// of them has settled. True when it was.
pub async fn continue_waiting(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let requeued = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'waiting'
           AND (SELECT coalesce(sum(pending), 0) FROM turn_holds WHERE turn_id = ?1)
               = (SELECT count(*) FROM delegations
                  WHERE root_turn_id = ?1 AND status IN ('queued', 'running'))
           AND (NOT EXISTS (SELECT 1 FROM delegations
                            WHERE root_turn_id = ?1 AND status IN ('queued', 'running'))
                OR EXISTS (SELECT 1 FROM delegations d
                           WHERE d.root_turn_id = ?1 AND {}))",
        crate::db::delegations::READY
    )))
    .bind(turn_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(requeued == 1)
}

pub async fn load_item(
    conn: &mut SqliteConnection,
    item_id: Uuid,
) -> Result<ItemRecord, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN turns t ON t.id = i.turn_id WHERE i.id = ?1"
    )))
    .bind(item_id)
    .fetch_one(conn)
    .await
}

async fn load_turn_and_item(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<(TurnRecord, ItemRecord), sqlx::Error> {
    let turn = load_turn(conn, turn_id).await?;
    let item = load_item(conn, turn.source_item_id).await?;
    Ok((turn, item))
}

/// A previously accepted send with this request ID, if any. `Err` when the
/// ID was used with a different payload. Lets a matching retry succeed even
/// if its provider has since become unavailable.
pub async fn existing_send(
    pool: &SqlitePool,
    req: &SendRequest<'_>,
) -> Result<Option<Accepted>, AcceptError> {
    let hash = request_hash(req);
    let mut conn = pool.acquire().await?;
    let row: Option<(Uuid, Vec<u8>, String)> = sqlx::query_as(
        "SELECT t.id, t.request_hash, b.status FROM turns t JOIN bots b ON b.id = t.bot_id
         WHERE t.bot_id = ?1 AND t.request_id = ?2",
    )
    .bind(req.bot_id)
    .bind(req.request_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        None => Ok(None),
        Some((_, _, status)) if status != "active" => Err(AcceptError::Deleted),
        Some((_, saved, _)) if saved != hash => Err(AcceptError::RequestConflict),
        Some((turn_id, _, _)) => {
            let (turn, item) = load_turn_and_item(&mut conn, turn_id).await?;
            Ok(Some(Accepted {
                created: false,
                turn,
                item,
                preempted: Vec::new(),
            }))
        }
    }
}

/// Resolve a lost send response by its request ID.
pub async fn find_by_request(
    pool: &SqlitePool,
    bot_id: Uuid,
    request_id: Uuid,
) -> Result<Option<(TurnRecord, ItemRecord)>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let turn_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM turns WHERE bot_id = ?1 AND request_id = ?2")
            .bind(bot_id)
            .bind(request_id)
            .fetch_optional(&mut *conn)
            .await?;
    match turn_id {
        Some(id) => Ok(Some(load_turn_and_item(&mut conn, id).await?)),
        None => Ok(None),
    }
}

const TASK_OR_NO_WAITING_OWNER: &str = "(EXISTS (SELECT 1 FROM conversations k
                                 WHERE k.id = t.conversation_id AND k.kind = 'task')
          OR NOT EXISTS (SELECT 1 FROM turns w
                         WHERE w.conversation_id = t.conversation_id AND w.status = 'waiting'))";

/// Claim the oldest queued turn of the next eligible bot, oldest first
/// across bots. A bot's owner message that took over its delegated work goes
/// ahead of the bot's other queued turns, so the steer is not left behind. A bot is eligible when it is active, not stopping work, and
/// has no turn in an active status. A waiting owner turn holds only owner
/// work, and a waiting task holds nothing. The claimed turn moves to
/// `preparing` and is returned; the caller must start it before claiming
/// again, so a later failure never strands an already committed claim. A
/// turn in `busy` still has a task finishing its previous attempt, whose
/// cleanup would end a new attempt's grants; it and the rest of its bot's
/// queue wait for that task to wake the scheduler again.
pub async fn claim_next(pool: &SqlitePool, busy: &[Uuid]) -> Result<Option<Uuid>, sqlx::Error> {
    let candidates: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, bot_id FROM (
            SELECT t.id, t.bot_id, t.created_at,
                   row_number() OVER (
                       PARTITION BY t.bot_id
                       ORDER BY EXISTS (SELECT 1 FROM delegations d
                                        WHERE d.takeover_turn_id = t.id
                                          AND d.status IN ('queued', 'running')) DESC,
                                t.turn_no) AS queue_rank
            FROM turns t JOIN bots b ON b.id = t.bot_id
            WHERE t.status = 'queued' AND b.status = 'active' AND NOT b.cancel_pending
              AND NOT EXISTS (SELECT 1 FROM turns a
                              WHERE a.bot_id = t.bot_id AND a.status IN {ACTIVE_STATUSES})
              AND {TASK_OR_NO_WAITING_OWNER}
         ) oldest WHERE queue_rank = 1
           AND NOT (lower(hex(id)) IN (SELECT replace(value, '-', '') FROM json_each(?1)))
         ORDER BY created_at, id LIMIT 8"
    )))
    .bind(sqlx::types::Json(busy))
    .fetch_all(pool)
    .await?;
    for (turn_id, bot_id) in candidates {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let eligible: Option<bool> = sqlx::query_scalar(
            "SELECT status = 'active' AND NOT cancel_pending FROM bots WHERE id = ?1",
        )
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
        if eligible != Some(true) {
            continue;
        }
        let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE turns SET status = 'preparing', started_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 attempt = attempt + 1
             WHERE id = ?1 AND status = 'queued'
               AND NOT EXISTS (SELECT 1 FROM turns a
                               WHERE a.bot_id = ?2 AND a.status IN {ACTIVE_STATUSES})
               AND (SELECT {TASK_OR_NO_WAITING_OWNER} FROM turns t WHERE t.id = ?1)"
        )))
        .bind(turn_id)
        .bind(bot_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 1 {
            crate::db::events::turn_changed(&mut tx, bot_id, turn_id).await?;
            tx.commit().await?;
            return Ok(Some(turn_id));
        }
    }
    Ok(None)
}

/// Fail a claimed turn that could not be started (for example, its input
/// could not be loaded). Nothing was sent to a provider.
pub async fn fail_unstarted(
    pool: &SqlitePool,
    turn_id: Uuid,
    message: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT b.id FROM bots b JOIN turns t ON t.bot_id = b.id WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(bot_id) = bot_id else {
        return Ok(());
    };
    let updated = sqlx::query(
        "UPDATE turns SET status = 'failed', error_code = 'start_failed', error_message = ?2,
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'preparing'",
    )
    .bind(turn_id)
    .bind(message)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 1 {
        crate::db::events::turn_changed(&mut tx, bot_id, turn_id).await?;
    }
    tx.commit().await
}

/// Build the adapter input for a claimed turn.
pub async fn input(pool: &SqlitePool, turn_id: Uuid) -> Result<TurnInput, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let turn = load_turn(&mut conn, turn_id).await?;
    let bot_name: String = sqlx::query_scalar("SELECT name FROM bots WHERE id = ?1")
        .bind(turn.bot_id)
        .fetch_one(&mut *conn)
        .await?;
    let (text, Json(parts)): (String, Json<Vec<MessagePart>>) = sqlx::query_as(
        "SELECT text, coalesce(parts, '[]') FROM items WHERE id = ?1 AND kind IN ('user', 'routine', 'connector')",
    )
    .bind(turn.source_item_id)
    .fetch_one(&mut *conn)
    .await?;
    let history: Vec<HistoryEntry> = sqlx::query_as::<
        _,
        (
            i64,
            String,
            String,
            String,
            Option<String>,
            Json<Vec<MessagePart>>,
        ),
    >(
        "SELECT t.turn_no, i.kind, i.text, t.status,
                CASE WHEN t.status IN ('failed', 'uncertain') THEN t.error_message END,
                coalesce(i.parts, i.assistant_parts, '[]')
         FROM items i JOIN turns t ON t.id = i.turn_id
         WHERE i.conversation_id = ?1 AND t.turn_no < ?2
           AND i.kind IN ('user', 'routine', 'connector', 'status', 'assistant')
           AND i.routine_update IS NULL
           -- A child's reply reached this bot as its delegation outcome.
           AND NOT EXISTS (SELECT 1 FROM delegations d WHERE d.id = i.id)
         ORDER BY t.turn_no, i.seq",
    )
    .bind(turn.conversation_id)
    .bind(turn.turn_no)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(
        |(turn_no, kind, text, status, error, Json(parts))| HistoryEntry {
            turn_no,
            kind,
            text,
            status,
            error,
            parts,
        },
    )
    .collect();
    type Receipts = Json<std::collections::BTreeMap<String, String>>;
    let session = sqlx::query_as::<_, (String, String, i64, Option<i64>, Receipts, Receipts)>(
        "SELECT native_session_id, fingerprint, considered_through_turn, memory_revision, default_skills,
                context_receipts
         FROM provider_sessions s
         WHERE conversation_id = ?1 AND provider_id = ?2 AND status = 'active'
           AND lane = (SELECT session_lane FROM turns WHERE id = ?3)
           -- A session that may hold an uncertain delivery is never resumed.
           AND NOT EXISTS (SELECT 1 FROM turns u
                           WHERE u.provider_session_id = s.id AND u.status = 'uncertain')",
    )
    .bind(turn.conversation_id)
    .bind(&turn.provider_id)
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?
    .map(
        |(native_id, fingerprint, considered_through_turn, memory_revision, Json(default_skills), Json(context))| {
            SessionState {
                native_id,
                fingerprint,
                considered_through_turn,
                memory_revision,
                default_skills,
                context,
            }
        },
    );
    let considered = session.as_ref().map_or(0, |s| s.considered_through_turn);
    let files = input_files(&mut conn, turn.bot_id, &parts, &history, considered).await?;
    let mut input = TurnInput {
        bot_id: turn.bot_id,
        conversation_id: turn.conversation_id,
        turn_id,
        turn_no: turn.turn_no,
        bot_name,
        text,
        parts,
        files,
        profile: turn.profile.map(|Json(profile)| profile),
        history,
        session,
        tool_grant: None,
        memory: None,
        attempt: turn.attempt,
        delegation: crate::db::delegations::context(&mut conn, turn_id).await?,
        secrets: crate::db::secret_requests::undelivered(&mut conn, turn_id).await?,
        questions: crate::db::owner_questions::undelivered(&mut conn, turn_id).await?,
        standing_permissions: Vec::new(),
        outgoing_drafts: crate::db::outgoing_drafts::notes(&mut conn, turn_id).await?,
        social_setups: crate::socials::store::setup_outcomes(&mut conn, turn_id).await?,
        mcp_waits: crate::db::mcp_waits::undelivered(&mut conn, turn_id).await?,
        mcps: None,
        handoff: None,
        jobs: crate::db::handoffs::job_receipts(&mut conn, turn.bot_id).await?,
        run: match crate::db::routines::run_of(&mut conn, turn_id).await? {
            Some(run) => Some(crate::app::providers::SavedRun::Routine(run)),
            None => crate::db::webhooks::run_of(&mut conn, turn_id)
                .await?
                .map(crate::app::providers::SavedRun::Webhook),
        },
        rules: crate::db::rules::current(&mut conn, turn.bot_id)
            .await?
            .map(|(_, markdown)| markdown),
    };
    drop(conn);
    let (memory, permissions) = tokio::join!(
        crate::db::memory_tree::session_context(pool, input.bot_id),
        crate::db::owner_questions::permissions(pool, input.bot_id),
    );
    input.memory = memory?;
    let permissions = permissions?;
    let reach = input.grant_reach();
    input.standing_permissions = permissions
        .into_iter()
        .filter_map(|p| p.target)
        .filter(|t| reach.covers(t))
        .collect();
    Ok(input)
}

const HISTORY_FILE_BUDGET: usize = 32 * 1024 * 1024;

async fn input_files(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    parts: &[MessagePart],
    history: &[HistoryEntry],
    considered: i64,
) -> Result<Vec<crate::app::providers::InputFile>, sqlx::Error> {
    let file_ids = |parts: &[MessagePart]| -> Vec<(Uuid, u64)> {
        parts
            .iter()
            .filter_map(|part| match part {
                MessagePart::File { file } => Some((file.id, file.size)),
                MessagePart::Text { .. } => None,
            })
            .collect()
    };
    let mut ids: Vec<Uuid> = file_ids(parts).into_iter().map(|(id, _)| id).collect();
    let mut used = 0;
    for entry in history.iter().rev().filter(|e| e.turn_no > considered) {
        for (id, size) in file_ids(&entry.parts).into_iter().rev() {
            if ids.contains(&id) || used + size as usize > HISTORY_FILE_BUDGET {
                continue;
            }
            used += size as usize;
            ids.push(id);
        }
    }
    crate::db::attachments::load(conn, bot_id, &ids).await
}

/// Startup reconciliation, run before the scheduler admits work. A turn
/// claimed but never delivered returns to the queue. A turn that may have
/// reached its provider is never replayed: it becomes `uncertain`, keeping
/// any partial text. Returns (requeued, uncertain).
pub async fn recover_after_restart(pool: &SqlitePool) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let requeued: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'queued', started_at = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status = 'preparing'
           AND bot_id IN (SELECT id FROM bots WHERE status = 'active' AND NOT cancel_pending)
         RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let uncertain: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'uncertain', effect_uncertain = true,
             error_code = 'api_restarted',
             error_message = 'The API restarted while this message was being processed. \
It was not resent; send a follow-up to continue.',
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status IN ('sending', 'running', 'cancelling') RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut uncertain = uncertain;
    let halted: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'interrupted', error_code = 'cancelled',
             error_message = 'Stopped before it was sent.', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status = 'preparing' RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    uncertain.extend(halted);
    for (turn_id, bot_id) in requeued.iter().chain(&uncertain) {
        crate::db::events::turn_changed(&mut tx, *bot_id, *turn_id).await?;
    }
    crate::db::handoffs::settle_ended(&mut tx, None).await?;
    for (turn_id, _) in &uncertain {
        crate::db::memory::log_turn_end(&mut tx, *turn_id).await?;
        crate::db::delegations::cancel_for_root(&mut tx, *turn_id).await?;
    }
    tx.commit().await?;
    crate::db::stop::settle_all(pool).await?;
    Ok((requeued.len() as u64, uncertain.len() as u64))
}

/// Turns in an active status that no provider task in this process owns.
/// The singleton lock means no other process runs them, so they are settled:
/// one never started fails, one that may have reached its provider becomes
/// `uncertain` (never resent). Pending stops and deletions then complete.
pub async fn settle_orphans(pool: &SqlitePool, live: &[Uuid]) -> Result<u64, sqlx::Error> {
    let orphans: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, bot_id FROM turns WHERE status IN {ACTIVE_STATUSES} AND NOT (lower(hex(id)) IN (SELECT replace(value, '-', '') FROM json_each(?1)))"
    )))
    .bind(sqlx::types::Json(live))
    .fetch_all(pool)
    .await?;
    let mut settled = 0;
    for (turn_id, bot_id) in orphans {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("SELECT 1 FROM bots WHERE id = ?1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        crate::db::delegations::lock_targets_of_root(&mut tx, turn_id).await?;
        let updated = sqlx::query(
            "UPDATE turns SET
                 status = CASE WHEN status = 'preparing' THEN 'failed' ELSE 'uncertain' END,
                 effect_uncertain = effect_uncertain OR status <> 'preparing',
                 error_code = CASE WHEN status = 'preparing' THEN 'start_failed' ELSE 'orphaned' END,
                 error_message = CASE WHEN status = 'preparing'
                     THEN 'The message could not be started. Send it again.'
                     ELSE 'Processing stopped unexpectedly. It was not resent; send a follow-up to continue.'
                 END,
                 finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status IN ('preparing', 'sending', 'running', 'cancelling')",
        )
        .bind(turn_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 1 {
            crate::db::events::turn_changed(&mut tx, bot_id, turn_id).await?;
            crate::db::memory::log_turn_end(&mut tx, turn_id).await?;
            crate::db::delegations::cancel_for_root(&mut tx, turn_id).await?;
            crate::db::handoffs::settle_ended(&mut tx, Some(turn_id)).await?;
            crate::db::stop::settle(&mut tx, bot_id).await?;
            settled += 1;
        }
        tx.commit().await?;
    }
    Ok(settled)
}

/// IDs of turns being stopped.
pub async fn cancelling(pool: &SqlitePool) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM turns WHERE status = 'cancelling'")
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::messages::ReactionKind;

    fn request(reaction: Option<Reaction>) -> SendRequest<'static> {
        SendRequest {
            bot_id: Uuid::nil(),
            request_id: Uuid::nil(),
            text: "Hello there",
            hidden: false,
            parts: &[],
            reaction,
            requested_provider: Some("synthetic"),
            provider_id: "synthetic",
            uses_profile: false,
            open_draft: None,
        }
    }

    #[test]
    fn an_open_draft_is_part_of_the_request_hash() {
        use crate::db::outgoing_drafts::Attachment;
        let draft = |version, body: Option<&str>| Attachment {
            id: Uuid::nil(),
            version,
            subject: None,
            body: body.map(str::to_owned),
        };
        let hash = |draft: &Attachment| {
            request_hash(&SendRequest {
                open_draft: Some(draft),
                ..request(None)
            })
        };
        let plain = request_hash(&request(None));
        assert_ne!(hash(&draft(1, None)), plain);
        assert_eq!(hash(&draft(1, None)), hash(&draft(1, None)));
        assert_ne!(hash(&draft(1, None)), hash(&draft(2, None)));
        assert_ne!(hash(&draft(1, None)), hash(&draft(1, Some("Hi"))));
    }

    #[test]
    fn a_text_only_send_keeps_its_saved_request_hash() {
        assert_eq!(
            hex::encode(request_hash(&request(None))),
            "15fac7d6c0ee166df9bf70a936445c1584de89d06083cbc846c1a9162c0e8352"
        );
    }

    #[test]
    fn file_parts_are_part_of_the_request_hash() {
        use crate::app::messages::Attachment;
        let file = |id| MessagePart::File {
            file: Attachment {
                id,
                name: "a.png".into(),
                media_type: "image/png".into(),
                size: 3,
            },
        };
        let text = |t: &str| MessagePart::Text { text: t.into() };
        let hash = |parts: &[MessagePart]| {
            request_hash(&SendRequest {
                text: "Hello [File: a.png]",
                parts,
                ..request(None)
            })
        };
        let one = [text("Hello "), file(Uuid::nil())];
        let plain = hash(&[]);
        assert_ne!(hash(&one), plain);
        assert_eq!(hash(&one), hash(&one.clone()));
        assert_ne!(hash(&one), hash(&[text("Hello "), file(Uuid::max())]));
        assert_ne!(hash(&one), hash(&[text("Hello  "), file(Uuid::nil())]));
    }

    #[test]
    fn a_reaction_is_part_of_the_request_hash() {
        let reaction = |item_id, kind| Some(Reaction { item_id, kind });
        let up = request_hash(&request(reaction(Uuid::nil(), ReactionKind::ThumbsUp)));
        assert_ne!(up, request_hash(&request(None)));
        assert_ne!(
            up,
            request_hash(&request(reaction(Uuid::nil(), ReactionKind::ThumbsDown)))
        );
        assert_ne!(
            up,
            request_hash(&request(reaction(Uuid::max(), ReactionKind::ThumbsUp)))
        );
    }
}
