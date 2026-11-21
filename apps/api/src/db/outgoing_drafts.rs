//! Outgoing drafts: a bot proposes an email, message, or post, the owner
//! edits and approves or discards it, and the bot sends it once and reports
//! the result. Every channel
//! shares this lifecycle; only `Content` differs.
//!
//! A pending draft parks the attempt that proposed it, like an owner
//! question. The Approve button approves the exact version the owner saw
//! and continues that turn. An owner message sent while a draft is open
//! carries the owner's edits atomically and binds that version to the new
//! request; the bot there may revise it (a new revision supersedes the old
//! one) or, when the message approves it, confirm that exact version. If
//! that request ends having done neither, the reply moved on and the draft
//! closes as discarded (a trigger). An
//! approval lets one call of the send request send it: an external computer
//! tool call naming the draft, or `start_draft_send` before a host send. No
//! second approval card is shown for that call. Only the bot's report for
//! that claim marks it sent; an unreported claim becomes uncertain when the
//! request ends (a trigger).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use super::turns;
use crate::app::outgoing::{self, ApprovalSource, Content, Delivery, DraftKind, Edits, Status};
use crate::db::in_ids;
use crate::grants::policy::GrantReach;

mod send;

pub use send::{
    Claim, Claimed, MailAttempt, MailSend, Payload, SentCheck, after_mail_send, after_social_send,
    claim, claim_connected, fail_unsent, mail_attempts, next_mail_send, record_sent_check, report,
};

/// Drafts per owner request (turn).
pub const MAX_PER_TURN: i64 = 8;

/// A draft revision as shown to the owner and recorded in events.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct OutgoingDraft {
    pub id: Uuid,
    /// Shared by every revision of the same email, message, or post.
    pub lineage_id: Uuid,
    pub revision: i64,
    pub bot_id: Uuid,
    /// The turn the card is shown under.
    pub turn_id: Uuid,
    pub content: Json<Content>,
    /// Bumped by each owner edit; approvals and messages name it.
    pub version: i64,
    pub status: Status,
    /// Pending, and its turn is still live, so the Approve button works.
    pub can_approve: bool,
    /// Pending and handed to the live request of the owner message it was
    /// sent with: that request revises, confirms, or closes it.
    pub in_reply: bool,
    pub approval_source: Option<ApprovalSource>,
    pub approved_version: Option<i64>,
    /// The owner message that approved it, for a chat approval.
    pub approved_item_id: Option<Uuid>,
    pub approved_at: Option<DateTime<Utc>>,
    pub delivery: Option<Json<Delivery>>,
    pub result_detail: Option<String>,
    pub superseded_by: Option<Uuid>,
    /// What "Approve always" on this card would cover; none for an email
    /// or an X reply.
    pub kind: Option<Json<DraftKind>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
    pub send_turn_id: Option<Uuid>,
    pub context_turn_id: Option<Uuid>,
    pub context_version: Option<i64>,
    /// The action of the connected send this approval claimed. Settling the
    /// draft keeps it, while `delivery` may come to name an earlier send.
    pub claim_action_id: Option<Uuid>,
}

impl From<&OutgoingDraft> for else_wire::outgoing::OutgoingDraft {
    fn from(draft: &OutgoingDraft) -> Self {
        Self {
            id: draft.id,
            lineage_id: draft.lineage_id,
            revision: draft.revision,
            bot_id: Some(draft.bot_id),
            turn_id: Some(draft.turn_id),
            content: (&draft.content.0).into(),
            version: draft.version,
            status: draft.status.into(),
            can_approve: draft.can_approve,
            in_reply: draft.in_reply,
            approval_source: draft.approval_source.map(Into::into),
            approved_version: draft.approved_version,
            approved_item_id: draft.approved_item_id,
            approved_at: draft.approved_at,
            delivery: draft.delivery.as_deref().map(Into::into),
            result_detail: draft.result_detail.clone(),
            superseded_by: draft.superseded_by,
            kind: draft.kind.as_deref().cloned().map(Into::into),
            created_at: Some(draft.created_at),
            updated_at: draft.updated_at,
            settled_at: draft.settled_at,
        }
    }
}

/// The [`DraftKind`] of draft `d` as JSON, or NULL when it has none: the one
/// definition both granting and matching use. A platform that sends as the
/// owner ([`Platform::sends_as_owner`]) never has a task kind, only its
/// destination's, and none without the account it sends from.
///
/// [`Platform::sends_as_owner`]: crate::app::outgoing::Platform::sends_as_owner
macro_rules! kind {
    () => {
        "CASE WHEN json_extract(d.content, '$.platform') = 'whatsapp'
            AND json_extract(d.content, '$.account_id') IS NULL
            AND json_extract(d.content, '$.account') IS NULL THEN NULL
        WHEN json_extract(d.content, '$.channel') = 'message'
            AND json_extract(d.content, '$.task') IS NOT NULL
            AND json_extract(d.content, '$.platform') <> 'whatsapp'
        THEN json_object('platform', json_extract(d.content, '$.platform'),
            'account_id', json_extract(d.content, '$.account_id'),
            'account', lower(json_extract(d.content, '$.account')),
            'destination', NULL,
            'task', lower(json_extract(d.content, '$.task')))
        WHEN json_extract(d.content, '$.channel') = 'message'
            AND NOT (json_extract(d.content, '$.platform') = 'x'
                     AND (json_extract(d.content, '$.destination') IS NOT NULL
                          OR json_extract(d.content, '$.reply_to') IS NOT NULL))
        THEN json_object('platform', json_extract(d.content, '$.platform'),
            'account_id', json_extract(d.content, '$.account_id'),
            'account', lower(json_extract(d.content, '$.account')),
            'destination', json_extract(d.content, '$.destination')) END"
    };
}

const SELECT: &str = concat!(
    "SELECT d.id, d.lineage_id, d.revision, d.bot_id, d.turn_id, d.content,
        d.version, d.status,
        d.status = 'pending' AND t.status IN ('queued', 'preparing', 'sending', 'running', 'waiting')
            AS can_approve,
        d.status = 'pending' AND EXISTS (SELECT 1 FROM turns c WHERE c.id = d.context_turn_id
            AND c.status IN ('queued', 'preparing', 'sending', 'running', 'waiting')) AS in_reply,
        d.approval_source, d.approved_version, d.approved_item_id, d.approved_at, d.delivery,
        d.result_detail, d.superseded_by, d.created_at, d.updated_at, d.settled_at,
        d.send_turn_id, d.context_turn_id, d.context_version, d.claim_action_id, ",
    kind!(),
    " AS kind
    FROM outgoing_drafts d JOIN turns t ON t.id = d.turn_id"
);

#[derive(Debug, thiserror::Error)]
pub enum DraftError {
    #[error("not found")]
    NotFound,
    #[error("the draft changed since version {0}; reload it")]
    Changed(i64),
    #[error("the draft is {}", .0.as_str())]
    Settled(Status),
    #[error("the request that drafted it has ended; reply in chat to send it")]
    Inactive,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("only a message or a new post can be approved always")]
    NoKind,
    #[error("the draft has {0} attachments the approval did not name")]
    FilesUnseen(usize),
    #[error(
        "only the owner's own conversation or a task delegated for an owner request can draft for approval"
    )]
    NotAllowed,
    #[error("a revision keeps its draft's channel ({0})")]
    ChannelChanged(&'static str),
    #[error("this attempt already asked or delegated; end it and wait for the answer")]
    AttemptParked,
    #[error("this request already made {MAX_PER_TURN} drafts")]
    BudgetExhausted,
    #[error("the request is no longer running")]
    NotRunning,
    #[error(
        "the owner's message for this request was not sent with this draft open at its current version"
    )]
    NotInContext,
    #[error("always needs the owner's exact words from their current message")]
    NotQuoted,
    #[error(
        "this draft was approved once, without always, or the owner has since stopped approving it always"
    )]
    NotAlways,
    #[error("this draft was approved for another request")]
    OtherRequest,
    #[error("an earlier request still waits on the owner for this draft; make a new draft instead")]
    Held,
    #[error(
        "this email goes from its connected account: send it with send_draft, which records its receipt"
    )]
    Connected,
    #[error("this draft has no connected account: send it with a computer tool or your host tools")]
    NotConnected,
    #[error("native sending is unavailable for this task and account")]
    NoTransport,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl DraftError {
    /// The stable code tools and routes report.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "NOT_FOUND",
            Self::Changed(_) => "DRAFT_CHANGED",
            Self::Settled(_) => "DRAFT_SETTLED",
            Self::Inactive => "DRAFT_INACTIVE",
            Self::Invalid(_) => "INVALID_DRAFT",
            Self::NoKind => "NO_KIND",
            Self::FilesUnseen(_) => "ATTACHMENTS_UNSEEN",
            Self::NotAllowed => "NOT_ALLOWED",
            Self::ChannelChanged(_) => "CHANNEL_CHANGED",
            Self::AttemptParked => "ATTEMPT_PARKED",
            Self::BudgetExhausted => "BUDGET_EXHAUSTED",
            Self::NotRunning => "NOT_RUNNING",
            Self::NotInContext => "NOT_IN_CONTEXT",
            Self::NotQuoted => "NOT_QUOTED",
            Self::NotAlways => "NOT_ALWAYS",
            Self::OtherRequest => "OTHER_REQUEST",
            Self::Held => "DRAFT_HELD",
            Self::Connected => "CONNECTED_ACCOUNT",
            Self::NotConnected => "NOT_CONNECTED",
            Self::NoTransport => "NATIVE_UNAVAILABLE",
            Self::Db(_) => "UNAVAILABLE",
        }
    }
}

async fn load(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    draft_id: Uuid,
) -> Result<OutgoingDraft, DraftError> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} JOIN bots b ON b.id = d.bot_id
         WHERE d.id = ?1 AND d.bot_id = ?2 AND b.status = 'active'"
    )))
    .bind(draft_id)
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(DraftError::NotFound)
}

async fn reload(conn: &mut SqliteConnection, draft_id: Uuid) -> Result<OutgoingDraft, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE d.id = ?1")))
        .bind(draft_id)
        .fetch_one(&mut *conn)
        .await
}

/// Every draft revision shown under these turns, oldest first per turn.
pub async fn for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<OutgoingDraft>>, sqlx::Error> {
    let rows: Vec<OutgoingDraft> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE {}
         ORDER BY d.created_at, d.id",
        in_ids("d.turn_id", "?1")
    )))
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    Ok(super::holds::by_turn(rows, |row| row.turn_id))
}

async fn caller(conn: &mut SqliteConnection, turn_id: Uuid) -> Result<turns::Caller, DraftError> {
    let caller = turns::asking_caller(conn, turn_id)
        .await?
        .ok_or(DraftError::NotRunning)?;
    if !turns::may_ask_owner(conn, turn_id, &caller).await? {
        return Err(DraftError::NotAllowed);
    }
    Ok(caller)
}

/// Record a draft from the provider attempt of `turn_id`, parking it, or
/// a revision of the bot's pending or failed draft `replaces`, which it
/// supersedes. A draft of a kind the owner approves always is approved
/// for this request instead, when the turn may use saved grants (not a
/// webhook wake or an event), and the attempt goes on to send it; a draft
/// with attachments always waits for its own approval. A revision keeps the
/// channel (email, or the same platform).
/// Proposing the same content again in this turn returns the earlier
/// revision, so a retried tool call never makes a second card.
pub async fn propose(
    pool: &SqlitePool,
    turn_id: Uuid,
    content: &Content,
    replaces: Option<Uuid>,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let same: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM outgoing_drafts
         WHERE turn_id = ?1 AND status <> 'superseded' AND content = ?2 AND version = 1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(turn_id)
    .bind(Json(content))
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = same {
        return Ok(reload(&mut tx, id).await?);
    }
    if turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
        return Err(DraftError::AttemptParked);
    }
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM outgoing_drafts WHERE turn_id = ?1")
        .bind(turn_id)
        .fetch_one(&mut *tx)
        .await?;
    if total >= MAX_PER_TURN {
        return Err(DraftError::BudgetExhausted);
    }
    let id = Uuid::new_v4();
    let (lineage_id, revision, old_turn) = match replaces {
        None => (Uuid::new_v4(), 1, None),
        Some(old) => {
            let old = load(&mut tx, caller.bot_id, old).await?;
            if !old.content.same_channel(content) {
                return Err(DraftError::ChannelChanged(old.content.channel()));
            }
            if !matches!(old.status, Status::Pending | Status::Failed) {
                return Err(DraftError::Settled(old.status));
            }
            // Superseding it would leave that request waiting on a card
            // only this turn's revision can settle.
            let held: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT EXISTS (SELECT 1 FROM turns WHERE id = ?1 AND id <> ?2
                                  AND status IN {})",
                turns::UNFINISHED_STATUSES
            )))
            .bind(old.turn_id)
            .bind(turn_id)
            .fetch_one(&mut *tx)
            .await?;
            if held {
                return Err(DraftError::Held);
            }
            sqlx::query(
                "UPDATE outgoing_drafts SET status = 'superseded', superseded_by = ?2,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1",
            )
            .bind(old.id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            (old.lineage_id, old.revision + 1, Some(old.turn_id))
        }
    };
    sqlx::query(
        "INSERT INTO outgoing_drafts (id, lineage_id, revision, bot_id, turn_id, attempt, content)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(id)
    .bind(lineage_id)
    .bind(revision)
    .bind(caller.bot_id)
    .bind(turn_id)
    .bind(caller.attempt)
    .bind(Json(content))
    .execute(&mut *tx)
    .await?;
    if super::owner_questions::grant_reach(&mut tx, turn_id).await? != GrantReach::Nothing {
        sqlx::query(concat!(
            "UPDATE outgoing_drafts AS d SET status = 'approved', approval_source = 'standing',
                 approved_version = version,
                 approved_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 send_turn_id = turn_id, delivered_attempt = attempt
             WHERE d.id = ?1 AND json_array_length(d.content, '$.attachments') IS NULL
               AND EXISTS (SELECT 1 FROM draft_grants g
                 WHERE g.bot_id = d.bot_id AND g.kind = ",
            kind!(),
            ")"
        ))
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(old_turn) = old_turn.filter(|t| *t != turn_id) {
        events::turn_changed(&mut tx, caller.bot_id, old_turn).await?;
    }
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    let row = reload(&mut tx, id).await?;
    tx.commit().await?;
    Ok(row)
}

fn is_button_approval_replay(draft: &OutgoingDraft, version: i64, edits: Edits<'_>) -> bool {
    let same_approval = draft.approval_source == Some(ApprovalSource::Button)
        && !matches!(draft.status, Status::Superseded | Status::Discarded);
    let original_or_approved_version = draft
        .approved_version
        .is_some_and(|approved| version == approved || version.checked_add(1) == Some(approved));
    let same_content = draft
        .content
        .edited(edits)
        .is_ok_and(|content| content == draft.content.0);
    same_approval && original_or_approved_version && same_content
}

async fn apply_edits(
    conn: &mut SqliteConnection,
    draft: &OutgoingDraft,
    version: i64,
    edits: Edits<'_>,
) -> Result<bool, DraftError> {
    if draft.version != version {
        return Err(DraftError::Changed(version));
    }
    let content = draft.content.edited(edits).map_err(DraftError::Invalid)?;
    if content == draft.content.0 {
        return Ok(false);
    }
    if draft.status != Status::Pending {
        return Err(DraftError::Settled(draft.status));
    }
    sqlx::query(
        "UPDATE outgoing_drafts SET content = ?2, version = version + 1,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .bind(Json(&content))
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// The result of an owner action on a draft. `changed` is false for a
/// retry of an action already recorded.
#[derive(Debug, Clone)]
pub struct Settled {
    pub draft: OutgoingDraft,
    pub changed: bool,
    /// The waiting turn was queued again as a continuation.
    pub requeued: bool,
}

/// Save the owner's edits to a pending draft they saw at `version`.
pub async fn edit(
    pool: &SqlitePool,
    bot_id: Uuid,
    draft_id: Uuid,
    version: i64,
    edits: Edits<'_>,
) -> Result<Settled, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = load(&mut tx, bot_id, draft_id).await?;
    if draft.status != Status::Pending {
        return Err(DraftError::Settled(draft.status));
    }
    let changed = apply_edits(&mut tx, &draft, version, edits).await?;
    if changed {
        events::turn_changed(&mut tx, bot_id, draft.turn_id).await?;
    }
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(Settled {
        draft,
        changed,
        requeued: false,
    })
}

/// The owner approves the pending draft they saw at `version`, with their
/// final edits, atomically, and its turn continues to send it. `always`
/// also approves every later draft of its kind as the bot proposes it. A
/// retry of the same approval changes nothing.
pub async fn approve(
    pool: &SqlitePool,
    bot_id: Uuid,
    draft_id: Uuid,
    version: i64,
    edits: Edits<'_>,
    always: bool,
) -> Result<Settled, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = load(&mut tx, bot_id, draft_id).await?;
    if always && draft.kind.is_none() {
        return Err(DraftError::NoKind);
    }
    let files = draft.content.files().len();
    if files > 0 && edits.attachments.is_none() {
        return Err(DraftError::FilesUnseen(files));
    }
    if draft.status != Status::Pending {
        if is_button_approval_replay(&draft, version, edits)
            && (!always || approved_always(&mut tx, &draft).await?)
        {
            return Ok(Settled {
                draft,
                changed: false,
                requeued: false,
            });
        }
        return Err(DraftError::Settled(draft.status));
    }
    if !draft.can_approve {
        return Err(DraftError::Inactive);
    }
    apply_edits(&mut tx, &draft, version, edits).await?;
    if always {
        grant_always(&mut tx, &draft).await?;
    }
    sqlx::query(
        "UPDATE outgoing_drafts SET status = 'approved', approval_source = 'button',
             approved_version = version, approved_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             send_turn_id = turn_id, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .execute(&mut *tx)
    .await?;
    let requeued = turns::lifecycle::continue_waiting(&mut tx, draft.turn_id).await?;
    events::turn_changed(&mut tx, bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(Settled {
        draft,
        changed: true,
        requeued,
    })
}

/// The owner stops approving `draft_id`'s kind always. Later drafts of it
/// wait for approval again; drafts already approved keep their approval.
/// Stopping a kind not approved always changes nothing.
pub async fn stop_always(
    pool: &SqlitePool,
    bot_id: Uuid,
    draft_id: Uuid,
) -> Result<Settled, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = load(&mut tx, bot_id, draft_id).await?;
    let changed = sqlx::query(concat!(
        "DELETE FROM draft_grants WHERE (bot_id, kind) IN (SELECT d.bot_id, ",
        kind!(),
        " FROM outgoing_drafts d WHERE d.id = ?1)"
    ))
    .bind(draft.id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if changed {
        events::bot_updated(&mut tx, bot_id).await?;
    }
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(Settled {
        draft,
        changed,
        requeued: false,
    })
}

/// Approve every later draft of `draft`'s kind as the bot proposes it.
async fn grant_always(
    conn: &mut SqliteConnection,
    draft: &OutgoingDraft,
) -> Result<(), sqlx::Error> {
    sqlx::query(concat!(
        "INSERT INTO draft_grants (bot_id, kind, draft_id) SELECT d.bot_id, ",
        kind!(),
        ", d.id FROM outgoing_drafts d WHERE d.id = ?1
         ON CONFLICT (bot_id, kind) DO UPDATE SET draft_id = excluded.draft_id,
             granted_at = excluded.granted_at"
    ))
    .bind(draft.id)
    .execute(&mut *conn)
    .await?;
    events::bot_updated(conn, draft.bot_id).await
}

async fn approved_always(
    conn: &mut SqliteConnection,
    draft: &OutgoingDraft,
) -> Result<bool, sqlx::Error> {
    let Some(kind) = &draft.kind else {
        return Ok(false);
    };
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM draft_grants WHERE bot_id = ?1 AND kind = ?2)")
        .bind(draft.bot_id)
        .bind(kind)
        .fetch_one(&mut *conn)
        .await
}

/// The kinds the owner approves always for `bot_id`.
pub async fn always_approved(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<Vec<DraftKind>, sqlx::Error> {
    let rows: Vec<Json<DraftKind>> =
        sqlx::query_scalar("SELECT kind FROM draft_grants WHERE bot_id = ?1 ORDER BY granted_at")
            .bind(bot_id)
            .fetch_all(&mut *conn)
            .await?;
    Ok(rows.into_iter().map(|row| row.0).collect())
}

/// The owner discards a pending or failed draft. Its turn continues if it
/// waited on it. A retry changes nothing.
pub async fn discard(
    pool: &SqlitePool,
    bot_id: Uuid,
    draft_id: Uuid,
) -> Result<Settled, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = load(&mut tx, bot_id, draft_id).await?;
    match draft.status {
        Status::Discarded => {
            return Ok(Settled {
                draft,
                changed: false,
                requeued: false,
            });
        }
        Status::Pending | Status::Failed => {}
        other => return Err(DraftError::Settled(other)),
    }
    sqlx::query(
        "UPDATE outgoing_drafts SET status = 'discarded',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .execute(&mut *tx)
    .await?;
    let requeued = turns::lifecycle::continue_waiting(&mut tx, draft.turn_id).await?;
    events::turn_changed(&mut tx, bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(Settled {
        draft,
        changed: true,
        requeued,
    })
}

/// The draft open in the app when the owner sent a message: its id, the
/// version shown, and the owner's unsaved edits. `subject` is refused for
/// a draft that is not an email.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: Uuid,
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The draft's files the owner keeps, by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Uuid>>,
}

impl Attachment {
    pub fn edits(&self) -> Edits<'_> {
        Edits {
            subject: self.subject.as_deref(),
            body: self.body.as_deref(),
            attachments: self.attachments.as_deref(),
        }
    }
}

/// Within the acceptance of the owner message that starts `turn_id`: save
/// the owner's edits to the open draft and bind its resulting version to
/// that request, so the bot sees the owner's text and can confirm exactly
/// it. A stale version refuses the whole message.
pub async fn attach_to_message(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    turn_id: Uuid,
    attachment: &Attachment,
) -> Result<(), DraftError> {
    let draft = load(conn, bot_id, attachment.id).await?;
    if !draft.status.approvable_in_chat() {
        return Err(DraftError::Settled(draft.status));
    }
    let files = draft.content.files().len();
    if files > 0 && attachment.attachments.is_none() {
        return Err(DraftError::FilesUnseen(files));
    }
    apply_edits(conn, &draft, attachment.version, attachment.edits()).await?;
    sqlx::query(
        "UPDATE outgoing_drafts SET context_turn_id = ?2, context_version = version,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    events::turn_changed(conn, bot_id, draft.turn_id).await?;
    Ok(())
}

/// The bot read the owner message of its request as approval of the draft
/// that message was sent with. Only that request, only the version the
/// owner saw (unchanged since), and only a draft still open to sending.
/// The card moves under this request, which may now send it once.
/// `always` quotes the owner's words in that message asking to approve
/// drafts like it always; it also grants what the card's Approve always
/// would. Once this message has approved the draft, even if its send then
/// failed, `always` only reads the grant back, so it never undoes the
/// owner's Stop approving always.
pub async fn confirm(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
    always: Option<&str>,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let draft = load(&mut tx, caller.bot_id, draft_id).await?;
    if draft.status == Status::Approved
        && draft.send_turn_id == Some(turn_id)
        && draft.approval_source == Some(ApprovalSource::Message)
    {
        if always.is_some() && !approved_always(&mut tx, &draft).await? {
            return Err(DraftError::NotAlways);
        }
        return Ok(draft);
    }
    if !draft.status.approvable_in_chat() {
        return Err(DraftError::Settled(draft.status));
    }
    if always.is_some() && draft.kind.is_none() {
        return Err(DraftError::NoKind);
    }
    if let Some(quote) = always.map(str::trim)
        && (quote.is_empty()
            || !super::owner_questions::owner_message_quotes(&mut tx, turn_id, quote).await?)
    {
        return Err(DraftError::NotQuoted);
    }
    // The owner message that saw this draft: the request's own, or one the
    // reply has already been given while it worked.
    let source: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT i.id FROM turns t JOIN items i ON i.id = t.owner_item_id
         WHERE t.id = ?2 AND i.kind = 'user'
           AND (t.id = ?1 OR t.id IN ({}))",
        super::steers::delivered_into("?1")
    )))
    .bind(turn_id)
    .bind(draft.context_turn_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(source) = source else {
        return Err(DraftError::NotInContext);
    };
    if draft.context_version != Some(draft.version) {
        return Err(DraftError::NotInContext);
    }
    // This message approved it before and the send may have gone out: only
    // a later owner message may send it again.
    if draft.status == Status::Uncertain && draft.approved_item_id == Some(source) {
        return Err(DraftError::Settled(draft.status));
    }
    // This message approved it before and its send failed: it granted
    // always then or never, and the owner may have stopped it since.
    if always.is_some()
        && draft.approved_item_id == Some(source)
        && !approved_always(&mut tx, &draft).await?
    {
        return Err(DraftError::NotAlways);
    }
    sqlx::query(
        "UPDATE outgoing_drafts SET status = 'approved', approval_source = 'message',
             approved_version = version, approved_item_id = ?3,
             approved_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), send_turn_id = ?2,
             turn_id = ?2, attempt = ?4, delivery = NULL, claim_action_id = NULL,
             result_detail = NULL, settled_at = NULL,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .bind(turn_id)
    .bind(source)
    .bind(caller.attempt)
    .execute(&mut *tx)
    .await?;
    if always.is_some() {
        grant_always(&mut tx, &draft).await?;
    }
    if draft.turn_id != turn_id {
        events::turn_changed(&mut tx, caller.bot_id, draft.turn_id).await?;
    }
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(draft)
}

/// What a turn must know about drafts: owner decisions on its cards not yet
/// given to an attempt, and the draft open when one of its owner messages
/// was sent.
pub async fn notes(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<outgoing::Note>, sqlx::Error> {
    let joined = super::steers::joined_into("?1");
    let rows: Vec<OutgoingDraft> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE (d.turn_id = ?1 AND d.delivered_attempt IS NULL
                          AND d.status IN ('approved', 'discarded')
                          AND d.approval_source IS NOT 'message')
                     OR ((d.context_turn_id = ?1 OR d.context_turn_id IN ({joined}))
                         AND d.status <> 'superseded')
         ORDER BY d.created_at, d.id"
    )))
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    let mut context: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(joined))
        .bind(turn_id)
        .fetch_all(&mut *conn)
        .await?;
    context.push(turn_id);
    Ok(rows
        .into_iter()
        .map(|row| outgoing::Note {
            id: row.id,
            lineage_id: row.lineage_id,
            revision: row.revision,
            content: row.content.0.clone(),
            status: row.status,
            version: row.version,
            in_context: row.context_turn_id.is_some_and(|c| context.contains(&c))
                && row.context_version == Some(row.version),
            decided: row.turn_id == turn_id
                && row.approval_source != Some(ApprovalSource::Message)
                && matches!(row.status, Status::Approved | Status::Discarded),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_grant_kind_reads_existing_json() {
        for (json, task, destination) in [
            (
                r#"{"platform":"telegram","account_id":null,"account":"helper","destination":null,"task":"daily brief"}"#,
                Some("daily brief"),
                None,
            ),
            (
                r#"{"platform":"telegram","account_id":null,"account":"helper","destination":"family"}"#,
                None,
                Some("family"),
            ),
        ] {
            let stored: DraftKind = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&stored).unwrap(), json);
            let wire: else_wire::outgoing::DraftKind = stored.into();
            assert_eq!(
                wire,
                else_wire::outgoing::DraftKind {
                    platform: else_wire::outgoing::DraftPlatform::Telegram,
                    account_id: None,
                    account: Some("helper".into()),
                    destination: destination.map(str::to_owned),
                    task: task.map(str::to_owned),
                }
            );
        }
    }

    #[test]
    fn outgoing_draft_content_reads_a_stored_attachment() {
        let json = r#"{"channel":"email","from":"bot@example.org","to":["owner@example.org"],"cc":[],"bcc":[],"subject":"Brief","body":"Here it is.","attachments":[{"id":"cebfa944-17e7-4469-808d-2d4eb9aaafec","name":"brief.txt","media_type":"text/plain","size":5,"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"}]}"#;
        let content: Content = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&content).unwrap(), json);

        let wire: else_wire::outgoing::DraftContent = (&content).into();
        let else_wire::outgoing::DraftContent::Email { attachments, .. } = wire else {
            panic!("expected an email");
        };
        assert_eq!(
            attachments,
            [else_wire::outgoing::DraftFile {
                id: "cebfa944-17e7-4469-808d-2d4eb9aaafec".parse().unwrap(),
                name: "brief.txt".into(),
                media_type: "text/plain".into(),
                size: 5,
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into(),
            }]
        );
    }
}
