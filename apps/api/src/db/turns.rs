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
use crate::db::bots::BotStatus;
use crate::db::in_ids;
pub use crate::db::steers::Placement;
use crate::db::steers::STEERED;

pub mod lifecycle;

/// Where a turn is in its life, as stored in `turns.status`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    sqlx::Type,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum TurnStatus {
    Queued,
    Preparing,
    Sending,
    Running,
    Cancelling,
    Waiting,
    Completed,
    Interrupted,
    Failed,
    Uncertain,
}

/// The part of a turn's life a [`TurnStatus`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Queued,
    /// Holds the bot's single active slot.
    Active,
    /// Parked until its holds settle.
    Waiting,
    Ended,
}

impl TurnStatus {
    pub const ALL: [Self; 10] = [
        Self::Queued,
        Self::Preparing,
        Self::Sending,
        Self::Running,
        Self::Cancelling,
        Self::Waiting,
        Self::Completed,
        Self::Interrupted,
        Self::Failed,
        Self::Uncertain,
    ];

    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn phase(self) -> Phase {
        match self {
            Self::Queued => Phase::Queued,
            Self::Preparing | Self::Sending | Self::Running | Self::Cancelling => Phase::Active,
            Self::Waiting => Phase::Waiting,
            Self::Completed | Self::Interrupted | Self::Failed | Self::Uncertain => Phase::Ended,
        }
    }

    pub fn is_active(self) -> bool {
        self.phase() == Phase::Active
    }

    pub fn is_ended(self) -> bool {
        self.phase() == Phase::Ended
    }

    /// The provider has the turn and its tool calls are honored.
    pub fn serves_tools(self) -> bool {
        matches!(self, Self::Sending | Self::Running)
    }
}

/// [`TurnStatus::is_active`] as an SQL list.
pub const ACTIVE_STATUSES: &str = "('preparing', 'sending', 'running', 'cancelling')";
/// [`TurnStatus::is_ended`] as an SQL list.
pub const ENDED_STATUSES: &str = "('completed', 'interrupted', 'failed', 'uncertain')";
/// The statuses that are not [`TurnStatus::is_ended`], as an SQL list.
pub const UNFINISHED_STATUSES: &str =
    "('queued', 'preparing', 'sending', 'running', 'cancelling', 'waiting')";

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RoutineRun {
    routine_id: Uuid,
    name: String,
    due_at: DateTime<Utc>,
    reminder: bool,
    quiet: bool,
    no_news: bool,
}

impl From<&RoutineRun> for else_wire::routines::RoutineRun {
    fn from(value: &RoutineRun) -> Self {
        Self {
            routine_id: value.routine_id,
            name: value.name.clone(),
            due_at: value.due_at,
            reminder: value.reminder,
            quiet: value.quiet,
            no_news: value.no_news,
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TurnMail {
    account_id: Uuid,
    message: crate::connectors::types::MessageRef,
    subject: Option<String>,
    sender: Option<String>,
    location: MailLocation,
}

impl From<&TurnMail> for else_wire::email::TurnMail {
    fn from(mail: &TurnMail) -> Self {
        Self {
            account_id: mail.account_id,
            message: (&mail.message).into(),
            subject: mail.subject.clone(),
            sender: mail.sender.clone(),
            location: (&mail.location).into(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum MailLocation {
    Arrived,
    Archived(MailMove),
    Trashed(MailMove),
    Moved(MailMove),
    Deleted(MailChange),
}

#[derive(Debug, Clone, serde::Deserialize)]
struct MailMove {
    folder: String,
    #[serde(flatten)]
    change: MailChange,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct MailChange {
    by_bot_id: Option<Uuid>,
    by_turn_id: Option<Uuid>,
    at: String,
}

impl From<&MailLocation> for else_wire::email::MailLocation {
    fn from(location: &MailLocation) -> Self {
        match location {
            MailLocation::Arrived => Self::Arrived,
            MailLocation::Archived(moved) => Self::Archived(moved.into()),
            MailLocation::Trashed(moved) => Self::Trashed(moved.into()),
            MailLocation::Moved(moved) => Self::Moved(moved.into()),
            MailLocation::Deleted(change) => Self::Deleted(else_wire::email::MailDeletion {
                by_bot_id: change.by_bot_id,
                by_turn_id: change.by_turn_id,
                at: change.at.clone(),
            }),
        }
    }
}

impl From<&MailMove> for else_wire::email::MailMove {
    fn from(moved: &MailMove) -> Self {
        Self {
            folder: moved.folder.clone(),
            by_bot_id: moved.change.by_bot_id,
            by_turn_id: moved.change.by_turn_id,
            at: moved.change.at.clone(),
        }
    }
}

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
    pub status: TurnStatus,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub effect_uncertain: bool,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub reaction: Option<Json<Reaction>>,
    pub attempt: i32,
    /// Attempts the attempt limit cut that the turn continued after.
    pub limit_continuations: i64,
    /// The running reply this queued message joined
    /// ([`crate::db::steers`]).
    pub steered_into: Option<Uuid>,
    /// How far this message got into [`Self::steered_into`].
    pub steer_status: Option<crate::db::steers::Status>,
    /// The bot's requests for secrets during this turn (metadata only).
    #[sqlx(skip)]
    pub secret_requests: Vec<crate::db::secret_requests::RequestInfo>,
    #[sqlx(skip)]
    pub social_setups: Vec<crate::socials::store::Setup>,
    /// The bot's skill install cards during this turn (no file contents).
    #[sqlx(skip)]
    pub skill_installs: Vec<crate::db::skills::InstallInfo>,
    /// The bot's questions and approval requests to the owner during this turn.
    #[sqlx(skip)]
    pub questions: Vec<crate::db::owner_questions::QuestionInfo>,
    pub routine: Option<Json<RoutineRun>>,
    /// The emails whose arrival started this turn, oldest first.
    pub mail: Json<Vec<TurnMail>>,
    /// The outgoing draft revisions shown under this turn.
    #[sqlx(skip)]
    pub outgoing_drafts: Vec<crate::db::outgoing_drafts::OutgoingDraft>,
    /// The tasks this turn handed to other bots.
    #[sqlx(skip)]
    pub delegations: Vec<crate::db::delegations::Handoff>,
    /// The MCP jobs this turn waits on.
    #[sqlx(skip)]
    pub mcp_waits: Vec<crate::db::mcp_waits::Pending>,
    /// The background computer jobs this turn waits on.
    #[sqlx(skip)]
    pub computer_job_waits: Vec<crate::db::computer_job_waits::Pending>,
    /// The bot's requests that the owner take over its computer.
    #[sqlx(skip)]
    pub computer_handoffs: Vec<crate::db::computer_handoffs::Handoff>,
    /// The bot's notes that this delegated task ends needing the owner.
    #[sqlx(skip)]
    pub owner_notices: Vec<crate::db::owner_notices::Notice>,
    /// It answers a visible owner message in the bot's chat, joined no
    /// other reply, and no owner message came after it. Only such a turn
    /// can be retried, so a retry never reorders the chat.
    pub retry_shape: bool,
}

impl TurnRecord {
    /// Whether the owner may retry it ([`lifecycle::retry`]): it has the
    /// [`Self::retry_shape`], failed for a reason that may pass, and nothing it
    /// did is in doubt.
    pub fn retryable(&self) -> bool {
        use crate::app::providers::FailureCode;
        use crate::app::routine_failures::Cause;
        self.status == TurnStatus::Failed
            && self.retry_shape
            && !self.effect_uncertain
            && self
                .error_code
                .as_deref()
                .map(FailureCode::decode)
                .is_some_and(|code| {
                    !matches!(code, FailureCode::Other(_)) && code.cause() == Cause::Transient
                })
    }
}

pub const TURN_COLUMNS: &str = "t.id, t.bot_id, t.conversation_id, t.request_id, t.turn_no,
    t.owner_item_id AS source_item_id, t.provider_id, t.profile_revision, t.profile, t.status,
    t.error_code, t.error_message, t.effect_uncertain, t.created_at, t.started_at, t.finished_at, t.attempt,
    (SELECT i.reaction FROM items i WHERE i.id = t.owner_item_id) AS reaction,
    (SELECT count(*) FROM turn_limit_cuts l WHERE l.turn_id = t.id) AS limit_continuations,
    (SELECT s.into_turn_id FROM turn_steers s WHERE s.turn_id = t.id) AS steered_into,
    (SELECT s.status FROM turn_steers s WHERE s.turn_id = t.id) AS steer_status,
    (SELECT json_object('routine_id', lower(hex(r.routine_id)), 'name', r.name, 'due_at', r.due_at,
                        'reminder', json(iif(o.run_at IS NULL, 'false', 'true')),
                        'quiet', json(iif(o.quiet, 'true', 'false')),
                        'no_news', json(iif(r.report = 'no_news' AND t.status = 'completed', 'true', 'false')))
     FROM routine_runs r JOIN routines o ON o.id = r.routine_id WHERE r.turn_id = t.id) AS routine,
    (SELECT json_group_array(json_object('account_id', m.account_id, 'message',
                json_object('folder', m.folder, 'uid_validity', m.uid_validity, 'uid', m.uid),
                'subject', m.subject, 'sender', m.sender,
                'location', json(CASE m.location
                    WHEN 'arrived' THEN json_object('state', 'arrived')
                    WHEN 'deleted' THEN json_object('state', 'deleted', 'by_bot_id', m.by_bot_id,
                        'by_turn_id', m.by_turn_id, 'at', m.located_at)
                    ELSE json_object('state', m.location, 'folder', m.moved_folder,
                        'by_bot_id', m.by_bot_id, 'by_turn_id', m.by_turn_id, 'at', m.located_at)
                    END)))
     FROM (SELECT lower(hex(d.account_id)) AS account_id, e.folder, e.uid_validity, e.uid,
                  e.subject, e.sender, e.location, e.moved_folder, e.located_at,
                  iif(e.located_turn_id IS NULL, NULL, lower(hex(e.located_turn_id))) AS by_turn_id,
                  (SELECT lower(hex(l.bot_id)) FROM turns l WHERE l.id = e.located_turn_id) AS by_bot_id
           FROM connector_deliveries d JOIN connector_events e ON e.id = d.event_id
           WHERE d.turn_id = t.id ORDER BY e.created_at, e.uid, e.folder) m) AS mail,
    (EXISTS (SELECT 1 FROM conversations k JOIN items i ON i.id = t.owner_item_id
             WHERE k.id = t.conversation_id AND k.kind = 'owner' AND i.kind = 'user' AND NOT i.hidden)
     AND NOT EXISTS (SELECT 1 FROM turn_steers s WHERE s.turn_id = t.id)
     AND NOT EXISTS (SELECT 1 FROM turns l JOIN items li ON li.id = l.owner_item_id
                     WHERE l.conversation_id = t.conversation_id AND l.turn_no > t.turn_no
                       AND li.kind = 'user' AND NOT li.hidden)) AS retry_shape";

/// Whose conversation it is, as stored in `conversations.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum ConversationKind {
    /// The bot's chat with the owner.
    Owner,
    /// A delegated task's conversation.
    Task,
}

impl From<TurnStatus> for else_wire::turns::TurnStatus {
    fn from(status: TurnStatus) -> Self {
        match status {
            TurnStatus::Queued => Self::Queued,
            TurnStatus::Preparing => Self::Preparing,
            TurnStatus::Sending => Self::Sending,
            TurnStatus::Running => Self::Running,
            TurnStatus::Cancelling => Self::Cancelling,
            TurnStatus::Waiting => Self::Waiting,
            TurnStatus::Completed => Self::Completed,
            TurnStatus::Interrupted => Self::Interrupted,
            TurnStatus::Failed => Self::Failed,
            TurnStatus::Uncertain => Self::Uncertain,
        }
    }
}

impl From<ItemKind> for else_wire::items::ItemKind {
    fn from(kind: ItemKind) -> Self {
        match kind {
            ItemKind::User => Self::User,
            ItemKind::Assistant => Self::Assistant,
            ItemKind::Status => Self::Status,
            ItemKind::Routine => Self::Routine,
            ItemKind::Connector => Self::Connector,
        }
    }
}

/// What a conversation item is, as stored in `items.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Serialize)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    /// The owner's message.
    User,
    Assistant,
    /// A delegated task's request, or a receipt or rule update else.bot
    /// wrote.
    Status,
    Routine,
    Connector,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ItemRecord {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub seq: i64,
    pub turn_id: Uuid,
    pub kind: ItemKind,
    pub hidden: bool,
    pub text: String,
    pub provider_id: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Set for the owner's message: the request that created it.
    pub request_id: Option<Uuid>,
    /// Set for an owner's message sent as a reaction to an assistant message.
    pub reaction: Option<Json<Reaction>>,
    pub bot_task: Option<Json<else_wire::items::BotTask>>,
    pub bot_reply: Option<Json<else_wire::items::BotReply>>,
    pub routine_update: Option<Json<super::routines::StoredRoutineUpdate>>,
    pub bot_created: Option<Json<super::bots::StoredBotCreated>>,
    pub rules_update: Option<Json<super::rules::StoredRulesUpdate>>,
    /// Ordered text and file parts; `None` for plain text.
    pub parts: Option<Json<Vec<MessagePart>>>,
    /// The provider attempt that wrote an assistant item.
    pub attempt: Option<i64>,
}

pub const ITEM_COLUMNS: &str = "i.id, i.conversation_id, i.seq, i.turn_id, i.kind, i.text,
    i.provider_id, i.created_at, i.hidden,
    CASE WHEN i.kind = 'user' THEN t.request_id END AS request_id, i.reaction,
    CASE WHEN i.kind = 'status' AND i.routine_update IS NULL AND i.conversation_id <> t.conversation_id THEN
        (SELECT json_object('sender_bot_id', lower(hex(d.parent_bot_id)), 'sender_name', b.name,
                'prompt', CASE WHEN t.request_id = d.id THEN d.task
                               ELSE (SELECT s.text FROM items s WHERE s.id = t.owner_item_id) END,
                'title', d.title, 'owner_need', d.owner_need, 'owner_line', d.owner_line)
         FROM delegations d JOIN bots b ON b.id = d.parent_bot_id
         -- A task's first turn carries the delegation's ID as its request
         -- (see delegations::control::update_request_id).
         WHERE d.id = (SELECT f.request_id FROM turns f WHERE f.conversation_id = t.conversation_id
                       ORDER BY f.turn_no LIMIT 1))
    END AS bot_task,
    CASE WHEN i.kind = 'status' AND i.conversation_id = t.conversation_id THEN
        (SELECT json_object('sender_bot_id', lower(hex(d.target_bot_id)), 'sender_name', b.name, 'prompt', d.task, 'status', d.status,
                'reason', CASE WHEN d.status = 'failed' THEN
                    (SELECT nullif(c.error_message, '') FROM turns c
                     WHERE c.id = coalesce(d.takeover_turn_id, d.child_turn_id)) END,
                'title', d.title, 'owner_need', d.owner_need, 'owner_line', d.owner_line,
                -- Notices from every attempt of the task (an update starts a
                -- new turn in its conversation), then from tasks it delegated.
                'asks', json((SELECT json_group_array(json(a.ask)) FROM (
                    SELECT json_object('id', lower(hex(n.id)), 'bot_id', lower(hex(n.bot_id)),
                            'bot_name', nb.name, 'turn_id', lower(hex(n.turn_id)), 'kind', n.kind,
                            'summary', n.summary, 'tried', n.tried, 'status', n.status,
                            'created_at', n.created_at, 'settled_at', n.settled_at) AS ask
                    FROM owner_notices n JOIN bots nb ON nb.id = n.bot_id
                    WHERE n.status <> 'superseded'
                      AND n.turn_id IN (
                          SELECT t.id FROM turns t WHERE t.conversation_id = d.conversation_id
                          UNION SELECT d.takeover_turn_id
                          UNION SELECT gt.id FROM delegations g
                              JOIN turns gt ON gt.conversation_id = g.conversation_id
                          WHERE g.root_turn_id = d.takeover_turn_id
                             OR g.root_turn_id IN (SELECT t.id FROM turns t
                                                   WHERE t.conversation_id = d.conversation_id))
                    ORDER BY n.created_at, n.id) AS a)))
         FROM delegations d JOIN bots b ON b.id = d.target_bot_id WHERE d.id = i.id)
    END AS bot_reply, coalesce(i.parts, i.assistant_parts) AS parts, i.routine_update,
    i.bot_created, i.rules_update, i.attempt";

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
    #[error("the answered notice is not one of this bot's")]
    InvalidNotice,
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
    /// Where the message went; `None` for a retry, or while admission is
    /// deferred.
    pub placement: Option<crate::db::steers::Placement>,
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
    /// The owner ask this message replies to (part of the request hash).
    pub answers_notice: Option<Uuid>,
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
    if let Some(notice) = req.answers_notice {
        hasher.update(b"answers-notice\0");
        hasher.update(notice.hyphenated().to_string().as_bytes());
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

/// Whether a new owner message is placed now ([`crate::db::steers::place`])
/// or waits for the scheduler's gate to reopen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// Place it now (the usual case).
    Open,
    /// Leave it queued and unplaced, as while a backup waits for running
    /// turns to end; [`settle_deferred`] places it afterwards.
    Deferred,
}

pub async fn accept(
    pool: &SqlitePool,
    req: SendRequest<'_>,
    admission: Admission,
) -> Result<Accepted, AcceptError> {
    let hash = request_hash(&req);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot: Option<(BotStatus, Uuid, i64, i64, bool)> = sqlx::query_as(
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
    if !status.is_active() {
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
            placement: None,
        });
    }
    if let Some(reaction) = req.reaction {
        let target: Option<ItemKind> =
            sqlx::query_scalar("SELECT kind FROM items WHERE id = ?1 AND conversation_id = ?2")
                .bind(reaction.item_id)
                .bind(conversation_id)
                .fetch_optional(&mut *tx)
                .await?;
        if target != Some(ItemKind::Assistant) {
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
        let busy: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE bot_id = ?1
             AND status IN {UNFINISHED_STATUSES})"
        )))
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
             owner_item_id, provider_id, profile_revision, profile, deferred)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
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
    .bind(admission == Admission::Deferred)
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
    if !req.hidden {
        use crate::db::owner_notices::{Answer, AnswerError, answer};
        let target = req
            .answers_notice
            .map_or(Answer::Untargeted, Answer::Notice);
        answer(&mut tx, req.bot_id, target)
            .await
            .map_err(|err| match err {
                AnswerError::NotThisBots => AcceptError::InvalidNotice,
                AnswerError::Db(err) => AcceptError::Db(err),
            })?;
    }
    let placement = match admission {
        // A setup message runs only on an idle bot.
        Admission::Open if req.hidden => Some(crate::db::steers::Placement::Queued),
        Admission::Open => Some(
            crate::db::steers::place(
                &mut tx,
                req.bot_id,
                conversation_id,
                turn_no,
                turn_id,
                req.provider_id,
                req.reaction.is_none() && req.open_draft.is_none() && req.answers_notice.is_none(),
            )
            .await?,
        ),
        // Running turns finish first; settle_deferred places the message
        // once the pause ends.
        Admission::Deferred => None,
    };
    crate::db::events::turn_changed(&mut tx, req.bot_id, turn_id).await?;
    crate::db::memory::log_item(&mut tx, item_id).await?;
    let (turn, item) = load_turn_and_item(&mut tx, turn_id).await?;
    tx.commit().await?;
    Ok(Accepted {
        created: true,
        turn,
        item,
        placement,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    #[error("turn not found")]
    NotFound,
    #[error("bot deleted")]
    Deleted,
    #[error("work is still stopping")]
    CancellationPending,
    #[error("the turn cannot be retried")]
    NotRetryable,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub struct Retried {
    /// False when this attempt was already retried.
    pub created: bool,
    pub turn: TurnRecord,
}

/// The provider of failed turn `turn_id` of `bot_id`, which a retry checks
/// before it queues the turn.
pub async fn failed_provider(
    pool: &SqlitePool,
    bot_id: Uuid,
    turn_id: Uuid,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT provider_id FROM turns WHERE id = ?1 AND bot_id = ?2 AND status = 'failed'",
    )
    .bind(turn_id)
    .bind(bot_id)
    .fetch_optional(pool)
    .await
}

/// The owner's retry of failed turn `turn_id` of `bot_id`, whose attempt
/// `attempt` they saw fail ([`lifecycle::retry`]).
pub async fn retry(
    pool: &SqlitePool,
    bot_id: Uuid,
    turn_id: Uuid,
    attempt: i32,
) -> Result<Retried, RetryError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot: Option<(BotStatus, bool)> =
        sqlx::query_as("SELECT status, cancel_pending FROM bots WHERE id = ?1")
            .bind(bot_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((status, cancel_pending)) = bot else {
        return Err(RetryError::NotFound);
    };
    if !status.is_active() {
        return Err(RetryError::Deleted);
    }
    if cancel_pending {
        return Err(RetryError::CancellationPending);
    }
    let owned: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM turns WHERE id = ?1 AND bot_id = ?2)")
            .bind(turn_id)
            .bind(bot_id)
            .fetch_one(&mut *tx)
            .await?;
    if !owned {
        return Err(RetryError::NotFound);
    }
    let turn = load_turn(&mut tx, turn_id).await?;
    let created = match lifecycle::retry(&mut tx, &turn, attempt).await? {
        lifecycle::Retry::Repeated => false,
        lifecycle::Retry::Refused => return Err(RetryError::NotRetryable),
        lifecycle::Retry::Queued => true,
    };
    let turn = load_turn(&mut tx, turn_id).await?;
    tx.commit().await?;
    Ok(Retried { created, turn })
}

/// Place the messages sent during a pause ([`crate::db::steers::place`]),
/// now that the turns they queued behind have finished or parked. Run while
/// the gate is open, before claiming. Returns how many it settled, and the
/// running replies that must be woken to take the messages that joined them.
pub async fn settle_deferred(pool: &SqlitePool) -> Result<(usize, Vec<Uuid>), sqlx::Error> {
    let deferred: Vec<Uuid> = sqlx::query_scalar(
        "SELECT t.id FROM turns t JOIN bots b ON b.id = t.bot_id
         WHERE t.deferred = 1 AND t.status = 'queued'
           AND b.status = 'active' AND NOT b.cancel_pending
         ORDER BY t.turn_no",
    )
    .fetch_all(pool)
    .await?;
    let mut settled = 0;
    let mut woken = Vec::new();
    for id in deferred {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let turn: Option<(Uuid, Uuid, i64, String)> = sqlx::query_as(
            "UPDATE turns SET deferred = 0
             WHERE id = ?1 AND deferred = 1 AND status = 'queued'
               AND bot_id IN (SELECT id FROM bots WHERE status = 'active' AND NOT cancel_pending)
             RETURNING bot_id, conversation_id, turn_no, provider_id",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((bot_id, conversation_id, turn_no, provider_id)) = turn else {
            continue;
        };
        // Whether a deferred message targeted a notice or a draft is not
        // kept, so it answers no card by its words.
        let placement = crate::db::steers::place(
            &mut tx,
            bot_id,
            conversation_id,
            turn_no,
            id,
            &provider_id,
            false,
        )
        .await?;
        woken.extend(placement.wakes());
        crate::db::events::turn_changed(&mut tx, bot_id, id).await?;
        tx.commit().await?;
        settled += 1;
    }
    Ok((settled, woken))
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
/// setups, delegations, pending MCP waits, computer handoffs, and owner
/// notices.
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
    let mut job_waits = crate::db::computer_job_waits::pending_for_turns(conn, &ids).await?;
    let mut skills = crate::db::skills::for_turns(conn, &ids).await?;
    let mut handoffs = crate::db::computer_handoffs::for_turns(conn, &ids).await?;
    let mut notices = crate::db::owner_notices::for_turns(conn, &ids).await?;
    for turn in turns {
        turn.skill_installs = skills.remove(&turn.id).unwrap_or_default();
        turn.social_setups = socials.remove(&turn.id).unwrap_or_default();
        turn.secret_requests = secrets.remove(&turn.id).unwrap_or_default();
        turn.questions = questions.remove(&turn.id).unwrap_or_default();
        turn.outgoing_drafts = drafts.remove(&turn.id).unwrap_or_default();
        turn.delegations = delegations.remove(&turn.id).unwrap_or_default();
        turn.mcp_waits = mcp_waits.remove(&turn.id).unwrap_or_default();
        turn.computer_job_waits = job_waits.remove(&turn.id).unwrap_or_default();
        turn.computer_handoffs = handoffs.remove(&turn.id).unwrap_or_default();
        turn.owner_notices = notices.remove(&turn.id).unwrap_or_default();
    }
    Ok(())
}

/// The turn behind a tool call, as [`caller`] reads it.
#[derive(sqlx::FromRow)]
pub struct Caller {
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub attempt: i32,
    pub owner_conversation: bool,
    pub main_bot: bool,
    /// The turn is sending or running and its bot is active with no Stop
    /// pending, so it may still call tools.
    pub running: bool,
}

/// The turn `turn_id` whether or not it still runs, so a gate can check
/// ownership before it reports that the turn ended.
pub async fn caller(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Option<Caller>, sqlx::Error> {
    sqlx::query_as(
        "SELECT t.bot_id, t.conversation_id, t.attempt, c.kind = 'owner' AS owner_conversation,
             EXISTS (SELECT 1 FROM installation WHERE default_bot_id = t.bot_id) AS main_bot,
             t.status IN ('sending', 'running') AND b.status = 'active' AND NOT b.cancel_pending
                 AS running
         FROM turns t JOIN conversations c ON c.id = t.conversation_id AND c.bot_id = t.bot_id
         JOIN bots b ON b.id = t.bot_id
         WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(conn)
    .await
}

/// [`asking_caller`] on a pooled connection, for a tool that reads the
/// attempt before a slow call so the call cannot park a later attempt.
pub async fn running_caller(
    pool: &sqlx::SqlitePool,
    turn_id: Uuid,
) -> Result<Option<Caller>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    asking_caller(&mut conn, turn_id).await
}

/// The [`caller`] of `turn_id` only while it is running.
pub async fn asking_caller(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Option<Caller>, sqlx::Error> {
    Ok(caller(conn, turn_id).await?.filter(|caller| caller.running))
}

/// Whether `caller` may show the owner a card: from the owner's own chat, or
/// from a task another bot delegated for an owner request, a routine run, or
/// a webhook wake, whose cards appear in its own bot's chat.
pub async fn may_ask_owner(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    caller: &Caller,
) -> Result<bool, sqlx::Error> {
    use crate::app::providers::Origin;
    Ok(caller.owner_conversation
        || matches!(
            super::delegations::origin(conn, turn_id).await?,
            Origin::OwnerRequest | Origin::RoutineRun | Origin::WebhookWake
        ))
}

/// SQL condition: attempt `attempt` of turn `turn` (SQL expressions)
/// delegated, waits on a job, asked for a secret, or asked the owner. It
/// must end and asks nothing more.
pub fn parked(turn: &str, attempt: &str) -> String {
    format!("EXISTS (SELECT 1 FROM turn_holds WHERE turn_id = {turn} AND attempt = {attempt})")
}

/// Whether the current `attempt` of `turn_id` is [`parked`].
pub async fn attempt_parked(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {}",
        parked("?1", "?2")
    )))
    .bind(turn_id)
    .bind(attempt)
    .fetch_one(&mut *conn)
    .await
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
    let row: Option<(Uuid, Vec<u8>, BotStatus)> = sqlx::query_as(
        "SELECT t.id, t.request_hash, b.status FROM turns t JOIN bots b ON b.id = t.bot_id
         WHERE t.bot_id = ?1 AND t.request_id = ?2",
    )
    .bind(req.bot_id)
    .bind(req.request_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        None => Ok(None),
        Some((_, _, status)) if !status.is_active() => Err(AcceptError::Deleted),
        Some((_, saved, _)) if saved != hash => Err(AcceptError::RequestConflict),
        Some((turn_id, _, _)) => {
            let (turn, item) = load_turn_and_item(&mut conn, turn_id).await?;
            Ok(Some(Accepted {
                created: false,
                turn,
                item,
                placement: None,
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

/// Whether turn `t` is background work: a routine run or connector event, or
/// a task delegated, directly or through another task, from one. Matches
/// `delegations::origin`: the source item of the owner conversation turn at
/// the top of the chain, followed by task conversation up to
/// `delegations::MAX_DEPTH` hops.
const BACKGROUND: &str = "coalesce((SELECT CASE WHEN c0.kind = 'owner' THEN i0.kind
                                 WHEN c1.kind = 'owner' THEN i1.kind
                                 WHEN c2.kind = 'owner' THEN i2.kind END
          FROM conversations c0 JOIN items i0 ON i0.id = t.owner_item_id
          LEFT JOIN delegations d1 ON d1.conversation_id = c0.id
          LEFT JOIN turns r1 ON r1.id = d1.root_turn_id
          LEFT JOIN conversations c1 ON c1.id = r1.conversation_id
          LEFT JOIN items i1 ON i1.id = r1.owner_item_id
          LEFT JOIN delegations d2 ON d2.conversation_id = c1.id
          LEFT JOIN turns r2 ON r2.id = d2.root_turn_id
          LEFT JOIN conversations c2 ON c2.id = r2.conversation_id
          LEFT JOIN items i2 ON i2.id = r2.owner_item_id
          WHERE c0.id = t.conversation_id), '') IN ('routine', 'connector')";

#[cfg(test)]
mod background_tests {
    /// `BACKGROUND` unrolls the chain one join group per hop.
    #[test]
    fn background_follows_every_hop_a_chain_can_have() {
        assert_eq!(
            crate::db::delegations::MAX_DEPTH,
            2,
            "extend BACKGROUND first"
        );
    }
}

pub struct Claimed {
    pub id: Uuid,
    pub background: bool,
}

/// Claim the oldest queued turn of the next eligible bot, oldest first
/// across bots. A bot's owner message that took over its delegated work, and
/// a delegating bot's update to a task, go ahead of the bot's other queued
/// turns, so the steer is not left behind. The owner's other messages come
/// next, ahead of queued tasks, so an aside waits only for the running one.
/// A bot is eligible when it is active, not stopping work, and has no turn
/// in an active status. A `waiting` turn holds nothing, whatever it waits
/// on (a task, an MCP or computer job, a card or draft): the bot is idle, so
/// its routine runs, connector events and other continuations go ahead, and
/// the waiting turn's continuation queues behind them by turn number. Its
/// holds stay on it. An owner message joins it and continues it at once
/// ([`crate::db::steers::place`]), so no owner request waits for its
/// holds. A turn behind a routine run that ended unreviewed waits for its
/// review, so it reads the run's failure notice. The claimed turn moves to
/// `preparing` and is returned; the caller must start it before claiming
/// again, so a later failure never strands an already committed claim. A
/// turn in `busy` still has a task finishing its previous attempt, whose
/// cleanup would end a new attempt's grants; it and the rest of its bot's
/// queue wait for that task to wake the scheduler again. Without
/// `background`, only owner work is claimed: it goes ahead of its bot's
/// background work in other conversations, never earlier turns in its own.
pub async fn claim_next(
    pool: &SqlitePool,
    busy: &[Uuid],
    background: bool,
) -> Result<Option<Claimed>, sqlx::Error> {
    let candidates: Vec<(Uuid, Uuid, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, bot_id, background FROM (
            SELECT t.id, t.bot_id, t.created_at, {BACKGROUND} AS background,
                   row_number() OVER (
                       PARTITION BY t.bot_id
                       -- Takeovers and update turns (control::update_request_id)
                       -- first, then the owner's messages ahead of queued tasks.
                       ORDER BY EXISTS (SELECT 1 FROM delegations d
                                        WHERE (d.takeover_turn_id = t.id
                                               OR (d.child_turn_id = t.id AND d.id <> t.request_id))
                                          AND d.status IN ('queued', 'running')) DESC,
                                EXISTS (SELECT 1 FROM conversations k JOIN items i ON i.id = t.owner_item_id
                                        WHERE k.id = t.conversation_id AND k.kind = 'owner'
                                          AND i.kind = 'user') DESC,
                                t.turn_no) AS queue_rank
            FROM turns t JOIN bots b ON b.id = t.bot_id
            WHERE t.status = 'queued' AND NOT t.deferred AND NOT {STEERED} AND NOT {behind}
              AND b.status = 'active' AND NOT b.cancel_pending
              AND NOT EXISTS (SELECT 1 FROM turns a
                              WHERE a.bot_id = t.bot_id AND a.status IN {ACTIVE_STATUSES})
              AND (?2 OR (NOT {BACKGROUND}
                   AND NOT EXISTS (SELECT 1 FROM turns q
                                   WHERE q.conversation_id = t.conversation_id
                                     AND q.status = 'queued' AND q.turn_no < t.turn_no
                                     AND NOT EXISTS (SELECT 1 FROM turn_steers s
                                                     WHERE s.turn_id = q.id))
                   AND NOT EXISTS (SELECT 1 FROM turns q
                                   WHERE q.bot_id = t.bot_id
                                     AND {})))
         ) oldest WHERE queue_rank = 1
           AND NOT ({})
         ORDER BY created_at, id LIMIT 8",
        in_ids("q.id", "?1"),
        in_ids("id", "?1"),
        behind = crate::db::routines::behind_unreviewed_run()
    )))
    .bind(sqlx::types::Json(busy))
    .bind(background)
    .fetch_all(pool)
    .await?;
    for (turn_id, bot_id, background) in candidates {
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
        if lifecycle::claim(&mut tx, turn_id, bot_id).await? {
            tx.commit().await?;
            return Ok(Some(Claimed {
                id: turn_id,
                background,
            }));
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
    lifecycle::fail_unstarted(&mut tx, bot_id, turn_id, message).await?;
    tx.commit().await
}

/// Bytes of earlier replies a fresh continuation receives, newest kept.
const EARLIER_REPLIES_BUDGET: usize = 8_000;

/// This turn's replies from its earlier attempts, oldest first, keeping the
/// newest within [`EARLIER_REPLIES_BUDGET`].
async fn earlier_replies(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    let texts: Vec<String> = sqlx::query_scalar(
        "SELECT text FROM items WHERE turn_id = ?1 AND kind = 'assistant' AND text != ''
         ORDER BY seq DESC",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(within_budget(texts, EARLIER_REPLIES_BUDGET))
}

/// Replies given newest first, returned oldest first within `budget`
/// bytes. The reply that crosses the budget keeps its end, where a reply
/// states what is open and what the owner must do, cut at a character
/// boundary, so even one long reply still arrives.
fn within_budget(newest_first: Vec<String>, budget: usize) -> Vec<String> {
    let mut left = budget;
    let mut kept = Vec::new();
    for text in newest_first {
        if text.len() <= left {
            left -= text.len();
            kept.push(text);
            continue;
        }
        let mut start = text.len() - left;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        if start < text.len() {
            kept.push(format!("…{}", &text[start..]));
        }
        break;
    }
    kept.reverse();
    kept
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
    let steered = crate::db::steers::for_request(&mut conn, turn_id).await?;
    let history: Vec<HistoryEntry> = sqlx::query_as::<
        _,
        (
            i64,
            ItemKind,
            String,
            TurnStatus,
            Option<String>,
            Json<Vec<MessagePart>>,
        ),
    >(
        "SELECT coalesce((SELECT a.turn_no FROM turn_steers s JOIN turns a ON a.id = s.into_turn_id
                          WHERE s.turn_id = t.id AND s.status = 'delivered'),
                         t.turn_no) AS history_turn,
                i.kind, i.text, t.status,
                CASE WHEN t.status IN ('failed', 'uncertain') THEN t.error_message END,
                coalesce(i.parts, i.assistant_parts, '[]')
         FROM items i JOIN turns t ON t.id = i.turn_id
              JOIN (SELECT session_lane AS name FROM turns WHERE id = ?3) lane
         WHERE t.turn_no < ?2
           -- A task's replies stream into its bot's chat; a later turn of
           -- the task, after an update, still reads them.
           AND (i.conversation_id = ?1
                OR (i.kind = 'assistant'
                    AND i.turn_id IN (SELECT id FROM turns WHERE conversation_id = ?1)))
           AND i.kind IN ('user', 'routine', 'connector', 'status', 'assistant')
           AND i.routine_update IS NULL
           -- A child's reply reached this bot as its delegation outcome.
           AND NOT EXISTS (SELECT 1 FROM delegations d WHERE d.id = i.id)
           -- A routine run starts a fresh session in its own lane; it catches
           -- up on the lane's latest report alone, never the bot's chat.
           AND (lane.name = '' OR (i.kind = 'assistant' AND t.turn_no = (
                SELECT max(u.turn_no) FROM turns u JOIN items j ON j.turn_id = u.id
                WHERE u.conversation_id = ?1 AND u.turn_no < ?2
                  AND u.session_lane = lane.name AND j.kind = 'assistant')))
         ORDER BY history_turn, i.seq",
    )
    .bind(turn.conversation_id)
    .bind(turn.turn_no)
    .bind(turn_id)
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
           -- A routine run resumes only the session it started itself.
           AND (lane = '' OR s.id = (SELECT provider_session_id FROM turns WHERE id = ?3))
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
    let mut files = input_files(&mut conn, turn.bot_id, &parts, &history, considered).await?;
    let mut delegation = crate::db::delegations::context(&mut conn, turn_id).await?;
    fit_delegation_files(&mut delegation.outcomes, HISTORY_FILE_BUDGET);
    // A chat turn that took tasks over answers them, so it needs their files.
    let taken_over: Vec<Json<Vec<MessagePart>>> = sqlx::query_scalar(
        "SELECT i.parts FROM delegations d JOIN turns c ON c.conversation_id = d.conversation_id
         JOIN items i ON i.id = c.owner_item_id
         WHERE d.takeover_turn_id = ?1 AND i.parts IS NOT NULL",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    let extra: Vec<Uuid> = taken_over
        .iter()
        .flat_map(|Json(parts)| parts)
        .filter_map(|part| match part {
            MessagePart::File { file } => Some(file.id),
            MessagePart::Text { .. } => None,
        })
        .chain(
            delegation
                .outcomes
                .iter()
                .flat_map(|outcome| outcome.files.iter().chain(&outcome.messages))
                .map(|file| file.id),
        )
        .chain(
            steered
                .iter()
                .flat_map(|steered| crate::db::steers::file_ids(&steered.message.parts)),
        )
        .filter(|id| !files.iter().any(|staged| staged.attachment.id == *id))
        .collect();
    files.extend(crate::db::attachments::load(&mut conn, turn.bot_id, &extra).await?);
    let mut input = TurnInput {
        bot_id: turn.bot_id,
        conversation_id: turn.conversation_id,
        turn_id,
        turn_no: turn.turn_no,
        bot_name,
        text,
        steered,
        parts,
        files,
        profile: turn.profile.map(|Json(profile)| profile),
        history,
        session,
        tool_grant: None,
        memory: None,
        attempt: turn.attempt,
        delegation,
        secrets: crate::db::secret_requests::undelivered(&mut conn, turn_id).await?,
        questions: crate::db::owner_questions::undelivered(&mut conn, turn_id).await?,
        standing_permissions: Vec::new(),
        draft_tasks: Vec::new(),
        outgoing_drafts: crate::db::outgoing_drafts::notes(&mut conn, turn_id).await?,
        social_setups: crate::socials::store::setup_outcomes(&mut conn, turn_id).await?,
        mcp_waits: crate::db::mcp_waits::undelivered(&mut conn, turn_id).await?,
        mcp_waits_in_progress: crate::db::mcp_waits::in_progress(&mut conn, turn_id).await?,
        computer_job_waits: crate::db::computer_job_waits::undelivered(&mut conn, turn_id).await?,
        skill_installs: crate::db::skills::undelivered(&mut conn, turn_id).await?,
        computer_handoffs: crate::db::computer_handoffs::undelivered(&mut conn, turn_id).await?,
        computer_control: crate::db::computer_handoffs::latest_taken(&mut conn, turn.bot_id)
            .await?,
        mcps: None,
        skills: None,
        handoff: None,
        jobs: crate::db::handoffs::job_receipts(&mut conn, turn.bot_id).await?,
        run: match crate::db::routines::run_of(&mut conn, turn_id).await? {
            Some(run) => Some(crate::app::providers::SavedRun::Routine(run)),
            None => match crate::db::webhooks::run_of(&mut conn, turn_id).await? {
                Some(run) => Some(crate::app::providers::SavedRun::Webhook(run)),
                None => crate::db::watchers::run_of(&mut conn, turn_id)
                    .await?
                    .map(crate::app::providers::SavedRun::Watcher),
            },
        },
        rules: crate::db::rules::current(&mut conn, turn.bot_id)
            .await?
            .map(|(_, markdown)| markdown),
        ran_actions: crate::db::owner_questions::ran_before(
            &mut conn,
            turn.conversation_id,
            turn.turn_no,
        )
        .await?,
        earlier_replies: earlier_replies(&mut conn, turn_id).await?,
        limit_continuation: crate::db::limit_cuts::undelivered(&mut conn, turn_id).await?,
    };
    let draft_tasks = crate::db::outgoing_drafts::always_approved(&mut conn, turn.bot_id).await?;
    drop(conn);
    let email_task = input.delegation.email_task();
    if email_task {
        input.jobs.clear();
        input.computer_control = None;
    }
    let (memory, permissions) = tokio::join!(
        async {
            if email_task {
                return Ok(None);
            }
            crate::db::memory_tree::session_context(pool, input.bot_id).await
        },
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
    if reach != crate::grants::policy::GrantReach::Nothing {
        input.draft_tasks = draft_tasks
            .into_iter()
            .filter(|kind| kind.task.is_some())
            .collect();
    }
    Ok(input)
}

const HISTORY_FILE_BUDGET: usize = 32 * 1024 * 1024;

/// Keeps the outcomes' files that fit `budget`. A task's messages may hold
/// requests its final message leaves out, so they fit before returned files.
fn fit_delegation_files(outcomes: &mut [crate::app::providers::DelegationOutcome], budget: usize) {
    let mut budget = budget;
    let mut fits = |file: &crate::app::messages::Attachment| {
        let fits = file.size as usize <= budget;
        budget -= if fits { file.size as usize } else { 0 };
        fits
    };
    for outcome in outcomes.iter_mut() {
        outcome.messages = outcome.messages.take().filter(&mut fits);
    }
    for outcome in outcomes.iter_mut() {
        outcome.files.retain(&mut fits);
    }
}

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

/// IDs of turns being stopped.
pub async fn cancelling(pool: &SqlitePool) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM turns WHERE status = 'cancelling'")
        .fetch_all(pool)
        .await
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_long_newest_reply_is_cut_not_dropped() {
        let kept = super::within_budget(vec!["é".repeat(10), "old".into()], 5);
        assert_eq!(
            kept,
            ["…éé"],
            "the newest reply arrives, cut at a character"
        );
        let kept = super::within_budget(vec!["new".into(), "older".into(), "oldest".into()], 9);
        assert_eq!(
            kept,
            ["…t", "older", "new"],
            "oldest first, each cut keeps its end"
        );
    }

    use super::*;
    use crate::app::messages::ReactionKind;

    #[test]
    fn a_tasks_messages_fit_before_files_other_tasks_returned() {
        let file = |name: &str, size: u64| crate::app::messages::Attachment {
            id: Uuid::new_v4(),
            name: name.into(),
            media_type: "text/plain".into(),
            size,
        };
        let outcome = |files: Vec<crate::app::messages::Attachment>, messages| {
            crate::app::providers::DelegationOutcome {
                id: Uuid::new_v4(),
                target_bot_id: Uuid::new_v4(),
                target_name: "Scout".into(),
                task: "Scout the leads.".into(),
                status: crate::db::delegations::Status::Completed,
                result: Some("Done.".into()),
                error: None,
                files,
                messages,
                earlier_request: false,
                ran: Vec::new(),
                owner: None,
                choices: Vec::new(),
            }
        };
        let mut outcomes = vec![
            outcome(vec![file("a.csv", 6), file("b.csv", 4)], None),
            outcome(Vec::new(), Some(file("messages.md", 3))),
        ];
        fit_delegation_files(&mut outcomes, 10);
        assert_eq!(
            outcomes[1].messages.as_ref().map(|f| f.name.as_str()),
            Some("messages.md")
        );
        let kept: Vec<_> = outcomes[0].files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(kept, ["a.csv"], "b.csv no longer fits");
    }

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
            answers_notice: None,
        }
    }

    #[test]
    fn the_answered_notice_is_part_of_the_request_hash() {
        let hash = |notice| {
            request_hash(&SendRequest {
                answers_notice: Some(notice),
                ..request(None)
            })
        };
        assert_ne!(hash(Uuid::nil()), request_hash(&request(None)));
        assert_ne!(hash(Uuid::nil()), hash(Uuid::max()));
    }

    #[test]
    fn an_open_draft_is_part_of_the_request_hash() {
        use crate::db::outgoing_drafts::Attachment;
        let draft = |version, body: Option<&str>| Attachment {
            id: Uuid::nil(),
            version,
            subject: None,
            body: body.map(str::to_owned),
            attachments: None,
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
