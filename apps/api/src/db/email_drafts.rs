//! Email drafts: a bot proposes an email, the owner edits and approves or
//! discards it, and the bot sends it once and reports the result (see
//! docs/decisions/0003-email-drafts.md).
//!
//! A pending draft parks the attempt that proposed it, like an owner
//! question. The Approve button approves the exact version the owner saw
//! and continues that turn. An owner message sent while a draft is open
//! carries the owner's edits atomically and binds that version to the new
//! request; the bot there may revise it (a new revision supersedes the old
//! one) or, when the message approves it, confirm that exact version. An
//! approval lets one call of the send request send it: an external computer
//! tool call naming the draft, or `start_email_send` before a host send. No
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
use crate::app::email::{self, ApprovalSource, Delivery, Draft, Outcome, Recipients, Status};

/// Drafts per owner request (turn).
pub const MAX_PER_TURN: i64 = 8;

/// A draft revision as shown to the owner and recorded in events.
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct EmailDraft {
    pub id: Uuid,
    pub email_id: Uuid,
    pub revision: i64,
    pub bot_id: Uuid,
    /// The turn the card is shown under.
    pub turn_id: Uuid,
    #[serde(rename = "from")]
    pub sender: String,
    #[serde(flatten)]
    pub recipients: Json<Recipients>,
    pub subject: String,
    pub body: String,
    /// Bumped by each owner edit; approvals and messages name it.
    pub version: i64,
    pub status: Status,
    /// Pending, and its turn is still live, so the Approve button works.
    pub can_approve: bool,
    pub approval_source: Option<ApprovalSource>,
    pub approved_version: Option<i64>,
    /// The owner message that approved it, for a chat approval.
    pub approved_item_id: Option<Uuid>,
    pub approved_at: Option<DateTime<Utc>>,
    pub delivery: Option<Json<Delivery>>,
    pub result_detail: Option<String>,
    pub superseded_by: Option<Uuid>,
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

impl EmailDraft {
    pub fn draft(&self) -> Draft {
        Draft {
            from: self.sender.clone(),
            recipients: self.recipients.0.clone(),
            subject: self.subject.clone(),
            body: self.body.clone(),
        }
    }
}

const SELECT: &str = "SELECT d.id, d.email_id, d.revision, d.bot_id, d.turn_id, d.sender,
        d.recipients, d.subject, d.body, d.version, d.status,
        d.status = 'pending' AND t.status IN ('queued', 'preparing', 'sending', 'running', 'waiting')
            AS can_approve,
        d.approval_source, d.approved_version, d.approved_item_id, d.approved_at, d.delivery,
        d.result_detail, d.superseded_by, d.created_at, d.updated_at, d.settled_at,
        d.send_turn_id, d.context_turn_id, d.context_version
    FROM email_drafts d JOIN turns t ON t.id = d.turn_id";

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
    #[error("only the owner's own conversation can draft email for approval")]
    NotAllowed,
    #[error("this attempt already asked or delegated; end it and wait for the answer")]
    AttemptParked,
    #[error("this request already drafted {MAX_PER_TURN} emails")]
    BudgetExhausted,
    #[error("the request is no longer running")]
    NotRunning,
    #[error(
        "the owner's message for this request was not sent with this draft open at its current version"
    )]
    NotInContext,
    #[error("this draft was approved for another request")]
    OtherRequest,
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
            Self::NotAllowed => "NOT_ALLOWED",
            Self::AttemptParked => "ATTEMPT_PARKED",
            Self::BudgetExhausted => "BUDGET_EXHAUSTED",
            Self::NotRunning => "NOT_RUNNING",
            Self::NotInContext => "NOT_IN_CONTEXT",
            Self::OtherRequest => "OTHER_REQUEST",
            Self::Db(_) => "UNAVAILABLE",
        }
    }
}

async fn load(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    draft_id: Uuid,
) -> Result<EmailDraft, DraftError> {
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

async fn reload(conn: &mut SqliteConnection, draft_id: Uuid) -> Result<EmailDraft, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE d.id = ?1")))
        .bind(draft_id)
        .fetch_one(&mut *conn)
        .await
}

/// Every draft revision shown under these turns, oldest first per turn.
pub async fn for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<EmailDraft>>, sqlx::Error> {
    let rows: Vec<EmailDraft> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE lower(hex(d.turn_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1))
         ORDER BY d.created_at, d.id"
    )))
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    let mut by_turn: HashMap<Uuid, Vec<EmailDraft>> = HashMap::new();
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
    if !caller.owner_conversation {
        return Err(DraftError::NotAllowed);
    }
    Ok(caller)
}

/// Record a draft from the provider attempt of `turn_id`, parking it, or
/// a revision of the bot's pending or failed draft `replaces`, which it
/// supersedes. Proposing the same email again in this turn returns the
/// earlier revision, so a retried tool call never makes a second card.
pub async fn propose(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft: &Draft,
    replaces: Option<Uuid>,
) -> Result<EmailDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let same: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM email_drafts
         WHERE turn_id = ?1 AND status <> 'superseded' AND sender = ?2 AND recipients = ?3
           AND subject = ?4 AND body = ?5 AND version = 1
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(turn_id)
    .bind(&draft.from)
    .bind(Json(&draft.recipients))
    .bind(&draft.subject)
    .bind(&draft.body)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = same {
        return Ok(reload(&mut tx, id).await?);
    }
    if turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
        return Err(DraftError::AttemptParked);
    }
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM email_drafts WHERE turn_id = ?1")
        .bind(turn_id)
        .fetch_one(&mut *tx)
        .await?;
    if total >= MAX_PER_TURN {
        return Err(DraftError::BudgetExhausted);
    }
    let id = Uuid::new_v4();
    let (email_id, revision, old_turn) = match replaces {
        None => (Uuid::new_v4(), 1, None),
        Some(old) => {
            let old = load(&mut tx, caller.bot_id, old).await?;
            if !matches!(old.status, Status::Pending | Status::Failed) {
                return Err(DraftError::Settled(old.status));
            }
            sqlx::query(
                "UPDATE email_drafts SET status = 'superseded', superseded_by = ?2,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1",
            )
            .bind(old.id)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            (old.email_id, old.revision + 1, Some(old.turn_id))
        }
    };
    sqlx::query(
        "INSERT INTO email_drafts (id, email_id, revision, bot_id, turn_id, attempt, sender,
             recipients, subject, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(id)
    .bind(email_id)
    .bind(revision)
    .bind(caller.bot_id)
    .bind(turn_id)
    .bind(caller.attempt)
    .bind(&draft.from)
    .bind(Json(&draft.recipients))
    .bind(&draft.subject)
    .bind(&draft.body)
    .execute(&mut *tx)
    .await?;
    if let Some(old_turn) = old_turn.filter(|t| *t != turn_id) {
        events::turn_changed(&mut tx, caller.bot_id, old_turn).await?;
    }
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    let row = reload(&mut tx, id).await?;
    tx.commit().await?;
    Ok(row)
}

/// The owner's subject and body for a draft, as last shown to them; an
/// omitted field keeps the saved text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Edits<'a> {
    pub subject: Option<&'a str>,
    pub body: Option<&'a str>,
}

fn is_button_approval_replay(draft: &EmailDraft, version: i64, edits: Edits<'_>) -> bool {
    let same_approval = draft.approval_source == Some(ApprovalSource::Button)
        && !matches!(draft.status, Status::Superseded | Status::Discarded);
    let original_or_approved_version = draft
        .approved_version
        .is_some_and(|approved| version == approved || version.checked_add(1) == Some(approved));
    let same_subject = edits
        .subject
        .is_none_or(|text| email::subject(text).is_ok_and(|subject| subject == draft.subject));
    let same_body = edits.body.is_none_or(|text| text == draft.body);
    same_approval && original_or_approved_version && same_subject && same_body
}

async fn apply_edits(
    conn: &mut SqliteConnection,
    draft: &EmailDraft,
    version: i64,
    edits: Edits<'_>,
) -> Result<bool, DraftError> {
    if draft.version != version {
        return Err(DraftError::Changed(version));
    }
    let subject = match edits.subject {
        Some(text) => email::subject(text).map_err(DraftError::Invalid)?,
        None => draft.subject.clone(),
    };
    let body = match edits.body {
        Some(text) => email::body(text).map_err(DraftError::Invalid)?,
        None => draft.body.clone(),
    };
    if subject == draft.subject && body == draft.body {
        return Ok(false);
    }
    if draft.status != Status::Pending {
        return Err(DraftError::Settled(draft.status));
    }
    sqlx::query(
        "UPDATE email_drafts SET subject = ?2, body = ?3, version = version + 1,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .bind(&subject)
    .bind(&body)
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

/// The result of an owner action on a draft. `changed` is false for a
/// retry of an action already recorded.
#[derive(Debug, Clone)]
pub struct Settled {
    pub draft: EmailDraft,
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
/// final edits, atomically, and its turn continues to send it. A retry of
/// the same approval changes nothing.
pub async fn approve(
    pool: &SqlitePool,
    bot_id: Uuid,
    draft_id: Uuid,
    version: i64,
    edits: Edits<'_>,
) -> Result<Settled, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let draft = load(&mut tx, bot_id, draft_id).await?;
    if draft.status != Status::Pending {
        if is_button_approval_replay(&draft, version, edits) {
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
    sqlx::query(
        "UPDATE email_drafts SET status = 'approved', approval_source = 'button',
             approved_version = version, approved_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             send_turn_id = turn_id, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .execute(&mut *tx)
    .await?;
    let requeued = turns::continue_waiting(&mut tx, draft.turn_id).await?;
    events::turn_changed(&mut tx, bot_id, draft.turn_id).await?;
    let draft = reload(&mut tx, draft_id).await?;
    tx.commit().await?;
    Ok(Settled {
        draft,
        changed: true,
        requeued,
    })
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
        "UPDATE email_drafts SET status = 'discarded',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(draft.id)
    .execute(&mut *tx)
    .await?;
    let requeued = turns::continue_waiting(&mut tx, draft.turn_id).await?;
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
/// version shown, and the owner's unsaved edits.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: Uuid,
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

impl Attachment {
    pub fn edits(&self) -> Edits<'_> {
        Edits {
            subject: self.subject.as_deref(),
            body: self.body.as_deref(),
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
    apply_edits(conn, &draft, attachment.version, attachment.edits()).await?;
    sqlx::query(
        "UPDATE email_drafts SET context_turn_id = ?2, context_version = version,
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
pub async fn confirm(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
) -> Result<EmailDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let draft = load(&mut tx, caller.bot_id, draft_id).await?;
    if draft.status == Status::Approved
        && draft.send_turn_id == Some(turn_id)
        && draft.approval_source == Some(ApprovalSource::Message)
    {
        return Ok(draft);
    }
    if !draft.status.approvable_in_chat() {
        return Err(DraftError::Settled(draft.status));
    }
    let source: Option<Uuid> = sqlx::query_scalar(
        "SELECT i.id FROM turns t JOIN items i ON i.id = t.owner_item_id
         WHERE t.id = ?1 AND i.kind = 'user'",
    )
    .bind(turn_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(source) = source else {
        return Err(DraftError::NotInContext);
    };
    if draft.context_turn_id != Some(turn_id) || draft.context_version != Some(draft.version) {
        return Err(DraftError::NotInContext);
    }
    sqlx::query(
        "UPDATE email_drafts SET status = 'approved', approval_source = 'message',
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
/// needs a new approval.
pub async fn claim(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
    delivery: &Delivery,
) -> Result<EmailDraft, DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let draft = load(&mut tx, caller.bot_id, draft_id).await?;
    if draft.status != Status::Approved {
        return Err(DraftError::Settled(draft.status));
    }
    if draft.send_turn_id != Some(turn_id) {
        return Err(DraftError::OtherRequest);
    }
    sqlx::query(
        "UPDATE email_drafts SET status = 'sending', delivery = ?2,
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

/// The bot reports how its claimed send ended. Only the send request's own
/// claim can be reported; `failed` may also release an approval never
/// claimed (nothing was sent). The same report again changes nothing.
pub async fn report(
    pool: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
    outcome: Outcome,
    detail: Option<&str>,
) -> Result<(EmailDraft, bool), DraftError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    let draft = load(&mut tx, caller.bot_id, draft_id).await?;
    if draft.send_turn_id != Some(turn_id) {
        return Err(DraftError::OtherRequest);
    }
    match (draft.status, outcome) {
        (status, outcome) if status == outcome.status() => return Ok((draft, false)),
        (Status::Sending, _) | (Status::Approved, Outcome::Failed) => {}
        (status, _) => return Err(DraftError::Settled(status)),
    }
    sqlx::query(
        "UPDATE email_drafts SET status = ?2, result_detail = ?3,
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

/// What a turn must know about drafts: owner decisions on its cards not yet
/// given to an attempt, and the draft open when its owner message was sent.
pub async fn notes(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<email::Note>, sqlx::Error> {
    let rows: Vec<EmailDraft> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE (d.turn_id = ?1 AND d.delivered_attempt IS NULL
                          AND d.status IN ('approved', 'discarded')
                          AND d.approval_source IS NOT 'message')
                     OR (d.context_turn_id = ?1 AND d.status <> 'superseded')
         ORDER BY d.created_at, d.id"
    )))
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| email::Note {
            id: row.id,
            email_id: row.email_id,
            revision: row.revision,
            draft: row.draft(),
            status: row.status,
            version: row.version,
            in_context: row.context_turn_id == Some(turn_id)
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
    sqlx::query(
        "UPDATE email_drafts SET delivered_attempt = ?3
         WHERE turn_id = ?1 AND delivered_attempt IS NULL
           AND lower(hex(id)) IN (SELECT replace(value, '-', '') FROM json_each(?2))",
    )
    .bind(turn_id)
    .bind(Json(ids))
    .bind(attempt)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
