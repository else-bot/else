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
use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use super::turns;
use crate::app::outgoing::{
    self, ApprovalSource, Content, Delivery, DraftKind, Edits, Outcome, Status,
};
use crate::app::questions::GrantReach;
use crate::db::in_ids;

/// Drafts per owner request (turn).
pub const MAX_PER_TURN: i64 = 8;

/// A draft revision as shown to the owner and recorded in events.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
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
    #[serde(skip)]
    pub send_turn_id: Option<Uuid>,
    #[serde(skip)]
    pub context_turn_id: Option<Uuid>,
    #[serde(skip)]
    pub context_version: Option<i64>,
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
        d.send_turn_id, d.context_turn_id, d.context_version, ",
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

/// One of an active bot's drafts.
pub async fn load_for(
    pool: &SqlitePool,
    bot_id: Uuid,
    draft_id: Uuid,
) -> Result<OutgoingDraft, DraftError> {
    load(&mut *pool.acquire().await?, bot_id, draft_id).await
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
    let mut by_turn: HashMap<Uuid, Vec<OutgoingDraft>> = HashMap::new();
    for row in rows {
        by_turn.entry(row.turn_id).or_default().push(row);
    }
    Ok(by_turn)
}

async fn caller(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<turns::AskingCaller, DraftError> {
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
             turn_id = ?2, attempt = ?4, delivery = NULL, result_detail = NULL, settled_at = NULL,
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

/// Claim an approved draft for the one call that sends it, atomically, in
/// the request allowed to send it. A second claim is refused: sending again
/// needs a new approval. An email bound to a connected account is claimed
/// only for that account's mail action. A repeated mail claim in the same
/// request returns the first, so its retry reuses the same action ID and
/// the connector's receipt.
pub async fn claim(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
    delivery: &Delivery,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let draft = load(&mut tx, caller.bot_id, draft_id).await?;
    match (draft.content.account(), delivery) {
        (Some(bound), Delivery::Mail { account_id, .. })
            if bound == *account_id && matches!(draft.content.0, Content::Email(_)) => {}
        (Some(bound), Delivery::Social { account_id, .. })
            if bound == *account_id && matches!(draft.content.0, Content::Message(_)) => {}
        (None, Delivery::Tool { .. } | Delivery::Host) => {}
        (Some(_), _) => return Err(DraftError::Connected),
        (None, Delivery::Mail { .. } | Delivery::Social { .. }) => {
            return Err(DraftError::NotConnected);
        }
    }
    if draft.status == Status::Sending
        && draft.send_turn_id == Some(turn_id)
        && matches!(
            draft.delivery.as_deref(),
            Some(Delivery::Mail { .. } | Delivery::Social { .. })
        )
    {
        return Ok(draft);
    }
    if draft.status != Status::Approved {
        return Err(DraftError::Settled(draft.status));
    }
    if draft.send_turn_id != Some(turn_id) {
        return Err(DraftError::OtherRequest);
    }
    sqlx::query(
        "UPDATE outgoing_drafts SET status = 'sending', delivery = ?2,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'approved'",
    )
    .bind(draft.id)
    .bind(Json(delivery))
    .execute(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, caller.bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(draft)
}

/// A send this request just claimed stopped before any external effect,
/// such as an approved file whose bytes changed: it fails, and only a new
/// approval sends it.
pub async fn fail_unsent(
    pool: &SqlitePool,
    draft_id: Uuid,
    detail: &str,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let failed = sqlx::query(
        "UPDATE outgoing_drafts SET status = 'failed', result_detail = ?2,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'sending'",
    )
    .bind(draft_id)
    .bind(detail)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let draft = reload(&mut tx, draft_id).await?;
    if failed == 0 {
        return Err(DraftError::Settled(draft.status));
    }
    events::turn_changed(&mut tx, draft.bot_id, draft.turn_id).await?;
    tx.commit().await?;
    Ok(draft)
}

/// The bot reports how its claimed send ended. Only the send request's own
/// claim can be reported; `failed` may also release an approval never
/// claimed (nothing was sent). The same report again changes nothing.
pub async fn report(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
    outcome: Outcome,
    detail: Option<&str>,
) -> Result<(OutgoingDraft, bool), DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let draft = load(&mut tx, caller.bot_id, draft_id).await?;
    if draft.send_turn_id != Some(turn_id) {
        return Err(DraftError::OtherRequest);
    }
    let connected = draft.content.account().is_some();
    match (draft.status, outcome) {
        (status, outcome) if status == outcome.status() => return Ok((draft, false)),
        (Status::Sending, _) if connected => return Err(DraftError::Connected),
        (Status::Sending, _) | (Status::Approved, Outcome::Failed) => {}
        (status, _) => return Err(DraftError::Settled(status)),
    }
    sqlx::query(
        "UPDATE outgoing_drafts SET status = ?2, result_detail = ?3,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .bind(outcome.status().as_str())
    .bind(detail)
    .execute(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, caller.bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok((draft, true))
}

/// After a connector send returned: a send the mail connector refused
/// before recording its action (`refused`) never reached the mail server,
/// so it fails; otherwise the action receipt already settled it, or will.
/// Announces the draft's current state.
pub async fn after_mail_send(
    pool: &SqlitePool,
    draft_id: Uuid,
    refused: Option<&str>,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = reload(&mut tx, draft_id).await?;
    if let (
        Some(code),
        Status::Sending,
        Some(Delivery::Mail {
            account_id,
            action_id,
        }),
    ) = (refused, draft.status, draft.delivery.as_deref())
    {
        sqlx::query(
            "UPDATE outgoing_drafts SET status = 'failed', result_detail = ?2,
                 settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status = 'sending' AND NOT EXISTS (
                 SELECT 1 FROM connector_actions WHERE account_id = ?3 AND action_id = ?4)",
        )
        .bind(draft.id)
        .bind(format!(
            "The mail connector refused it ({code}). Nothing was sent."
        ))
        .bind(account_id)
        .bind(action_id)
        .execute(&mut *tx)
        .await?;
    }
    events::turn_changed(&mut tx, draft.bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(draft)
}

/// What a claimed connected email does next.
#[derive(Debug, Clone)]
pub enum MailSend {
    /// Send it, with this Message-ID. The attempt is recorded first.
    Dispatch { message_id: String },
    /// An earlier send of it already succeeded: the claim is settled as
    /// sent, naming that send, and nothing is sent.
    Sent(Box<OutgoingDraft>),
    /// Earlier sends that may have gone out (`earlier`, newest first) block
    /// it: search Sent for `message_id`, then [`record_sent_check`]. `None`
    /// when an earlier send carried no stable Message-ID to search for.
    CheckSent {
        message_id: Option<String>,
        earlier: Vec<Uuid>,
    },
}

/// What a search of the Sent folder found for an earlier, uncertain send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, sqlx::Type)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum SentCheck {
    Found,
    Absent,
}

/// One mail action that sent, or tried to send, a draft, oldest first.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct MailAttempt {
    pub action_id: Uuid,
    /// The action's status; none when the connector refused it before
    /// recording it, so nothing was sent.
    pub status: Option<String>,
    pub phase: Option<String>,
    pub reason: Option<String>,
    pub message_id: Option<String>,
    pub sent_check: Option<SentCheck>,
    pub created_at: DateTime<Utc>,
}

/// The mail action this request claimed for a connected email.
fn claimed_mail(draft: &OutgoingDraft) -> Option<(Uuid, Uuid)> {
    match (draft.status, draft.delivery.as_deref()) {
        (
            Status::Sending,
            Some(Delivery::Mail {
                account_id,
                action_id,
            }),
        ) => Some((*account_id, *action_id)),
        _ => None,
    }
}

/// Decide whether a claimed connected email may be sent. An earlier send
/// that may have gone out, and that Sent was not searched for, blocks it.
/// The search moves the draft out of `sending`, so the next claim needs a
/// new approval. Otherwise its attempt is recorded, once, before the mail
/// connector sends it.
pub async fn next_mail_send(
    pool: &SqlitePool,
    draft_id: Uuid,
    message_id: &str,
) -> Result<MailSend, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = reload(&mut tx, draft_id).await?;
    let Some((account_id, action_id)) = claimed_mail(&draft) else {
        return Err(DraftError::Settled(draft.status));
    };
    let recorded: Option<Option<String>> = sqlx::query_scalar(
        "SELECT message_id FROM draft_mail_sends WHERE account_id = ?1 AND action_id = ?2",
    )
    .bind(account_id)
    .bind(action_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(recorded) = recorded {
        return Ok(MailSend::Dispatch {
            message_id: recorded.unwrap_or_else(|| message_id.to_owned()),
        });
    }
    let succeeded: Option<Uuid> = sqlx::query_scalar(
        "SELECT s.action_id FROM draft_mail_sends s JOIN connector_actions a
           ON a.account_id = s.account_id AND a.action_id = s.action_id
         WHERE s.draft_id = ?1 AND a.status = 'succeeded'
         ORDER BY s.created_at DESC, s.rowid DESC LIMIT 1",
    )
    .bind(draft_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(earlier) = succeeded {
        let settled = sqlx::query(
            "UPDATE outgoing_drafts SET status = 'sent', delivery = ?2,
                 result_detail = 'An earlier send of it went out. It was not sent again.',
                 settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status = 'sending'",
        )
        .bind(draft_id)
        .bind(Json(Delivery::Mail {
            account_id,
            action_id: earlier,
        }))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if settled == 0 {
            return Err(DraftError::Settled(draft.status));
        }
        events::turn_changed(&mut tx, draft.bot_id, draft.turn_id).await?;
        let draft = reload(&mut tx, draft_id).await?;
        tx.commit().await?;
        return Ok(MailSend::Sent(Box::new(draft)));
    }
    let unchecked: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT s.action_id, s.message_id
         FROM draft_mail_sends s JOIN connector_actions a
           ON a.account_id = s.account_id AND a.action_id = s.action_id
         WHERE s.draft_id = ?1 AND s.sent_check IS NULL
           AND a.status IN ('uncertain', 'dispatching')
         ORDER BY s.created_at DESC, s.rowid DESC",
    )
    .bind(draft_id)
    .fetch_all(&mut *tx)
    .await?;
    if !unchecked.is_empty() {
        let first = unchecked[0].1.clone();
        let searchable = first.is_some() && unchecked.iter().all(|(_, id)| *id == first);
        return Ok(MailSend::CheckSent {
            message_id: first.filter(|_| searchable),
            earlier: unchecked.into_iter().map(|(id, _)| id).collect(),
        });
    }
    sqlx::query(
        "INSERT INTO draft_mail_sends (account_id, action_id, draft_id, message_id)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(account_id)
    .bind(action_id)
    .bind(draft_id)
    .bind(message_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(MailSend::Dispatch {
        message_id: message_id.to_owned(),
    })
}

/// Settle the claim `claimed` of a connected email after searching Sent for
/// the `earlier` sends [`next_mail_send`] named, without sending it.
/// `Ok(check)` is what the search found; `Err(reason)` says why there was
/// no search.
///
/// Found: the newest earlier action succeeded and the draft is sent,
/// naming it. Absent: the draft is uncertain again, naming it, and the next
/// approval sends. No search: the draft is uncertain and nothing is
/// recorded, so the next approval must search again. A draft no longer in
/// that claim, or an earlier send already searched for, changes nothing.
pub async fn record_sent_check(
    pool: &SqlitePool,
    draft_id: Uuid,
    claimed: Uuid,
    earlier: &[Uuid],
    check: Result<SentCheck, String>,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = reload(&mut tx, draft_id).await?;
    let (Some((account_id, _)), Some(&newest)) = (claimed_mail(&draft), earlier.first()) else {
        return Ok(draft);
    };
    let (status, detail) = match &check {
        Ok(SentCheck::Found) => (
            Status::Sent,
            "Its Sent folder already holds it, so the earlier send went out. It was not sent again."
                .to_owned(),
        ),
        Ok(SentCheck::Absent) => (
            Status::Uncertain,
            "Its Sent folder does not hold it, which does not prove the earlier send failed. \
             Nothing was sent again; it sends only if the owner approves it again."
                .to_owned(),
        ),
        Err(reason) => (
            Status::Uncertain,
            format!(
                "Its Sent folder was not searched: {reason}. Nothing was sent again."
            ),
        ),
    };
    let moved = sqlx::query(
        "UPDATE outgoing_drafts SET status = ?2, delivery = ?3, result_detail = ?4,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'sending' AND json_extract(delivery, '$.via') = 'mail'
           AND json_extract(delivery, '$.action_id') = ?5",
    )
    .bind(draft_id)
    .bind(status.as_str())
    .bind(Json(Delivery::Mail {
        account_id,
        action_id: newest,
    }))
    .bind(
        detail
            .chars()
            .take(outgoing::MAX_DETAIL)
            .collect::<String>(),
    )
    .bind(claimed.to_string())
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if moved == 0 {
        return Ok(draft);
    }
    if let Ok(check) = check {
        let marked = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE draft_mail_sends SET sent_check = ?2,
                 checked_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE draft_id = ?1 AND sent_check IS NULL
               AND {}",
            in_ids("action_id", "?3")
        )))
        .bind(draft_id)
        .bind(check)
        .bind(Json(earlier))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // Another search already settled one of them, or the newest send
        // failed after all: keep the claim as it was.
        let settled = marked != earlier.len() as u64
            || (check == SentCheck::Found
                && !crate::connectors::store::found_in_sent(&mut tx, account_id, newest).await?);
        if settled {
            tx.rollback().await?;
            return Ok(reload(&mut *pool.acquire().await?, draft_id).await?);
        }
    }
    events::turn_changed(&mut tx, draft.bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(draft)
}

/// Every mail action that sent, or tried to send, `draft_id`.
pub async fn mail_attempts(
    pool: &SqlitePool,
    draft_id: Uuid,
) -> Result<Vec<MailAttempt>, sqlx::Error> {
    sqlx::query_as(
        "SELECT s.action_id, a.status, json_extract(a.result, '$.phase') AS phase,
                json_extract(a.result, '$.reason') AS reason, s.message_id, s.sent_check,
                s.created_at
         FROM draft_mail_sends s LEFT JOIN connector_actions a
           ON a.account_id = s.account_id AND a.action_id = s.action_id
         WHERE s.draft_id = ?1 ORDER BY s.created_at, s.rowid",
    )
    .bind(draft_id)
    .fetch_all(pool)
    .await
}

pub async fn after_social_send(
    pool: &SqlitePool,
    id: Uuid,
    refused: Option<&str>,
) -> Result<OutgoingDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = reload(&mut tx, id).await?;
    if let (
        Some(reason),
        Some(Delivery::Social {
            account_id,
            action_id,
        }),
    ) = (refused, draft.delivery.as_deref())
    {
        sqlx::query("UPDATE outgoing_drafts SET status='failed',result_detail=?2,updated_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now'),settled_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE id=?1 AND status='sending' AND NOT EXISTS(SELECT 1 FROM social_actions WHERE connection_id=?3 AND action_id=?4)").bind(id).bind(reason).bind(account_id).bind(action_id).execute(&mut *tx).await?;
    }
    events::turn_changed(&mut tx, draft.bot_id, draft.turn_id).await?;
    let updated = reload(&mut tx, id).await?;
    tx.commit().await?;
    Ok(updated)
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

/// Record that `attempt` received these owner decisions.
pub async fn mark_delivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
    ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE outgoing_drafts SET delivered_attempt = ?3
         WHERE turn_id = ?1 AND delivered_attempt IS NULL
           AND {}",
        in_ids("id", "?2")
    )))
    .bind(turn_id)
    .bind(Json(ids))
    .bind(attempt)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
