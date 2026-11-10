//! One-hop asynchronous delegation.
//!
//! A bot's provider attempt in its own owner conversation may hand one task
//! to another registered bot (the main bot included), at most two individual
//! delegations per request. A council accepts a group of up to 32 reports
//! atomically. Each child runs as an ordinary queued turn of the target bot,
//! in a separate task conversation, through the same scheduler. The target's owner conversation shows a receipt and the
//! child's reply, and the delegating bot's conversation shows the settled
//! reply; the task conversation keeps its execution (source item, grants,
//! native session). The parent attempt then ends; its owner turn waits
//! (`waiting`) with no provider process, and a database trigger requeues it
//! as a continuation once every child has an outcome. When the owner replies
//! in the target's chat while the child waits on an approval card there,
//! that chat turn takes the task over: the card is skipped, the child ends,
//! and the chat turn's outcome becomes the child's. Identity, budget,
//! target, and permissions come from the server, never from tool arguments.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use crate::app::messages::{Attachment, MessagePart};
use crate::app::providers::Profile;
use crate::providers::output_files::OutputFile;

/// Child limit when accepting an individual delegation.
pub const MAX_CHILDREN: i64 = 2;
/// Individual delegations per provider attempt; a council parks after its group.
pub const PER_ATTEMPT: i64 = 1;
pub const MAX_COUNCIL_BOTS: usize = 32;
pub const MAX_TASK_BYTES: usize = 8_000;

/// SQL over a delegation `d`: its outcome is unread and its batch has
/// settled. A batch is what one provider attempt sent, an individual task
/// or a council; a task carried from an earlier owner message (attempt 0)
/// stands alone. Matches the wake condition of the `turns_delegation_child`
/// trigger, so the parent reads each batch as soon as it lands.
pub const READY: &str = "d.delivered_attempt IS NULL
     AND d.status IN ('completed', 'failed', 'cancelled')
     AND NOT EXISTS (SELECT 1 FROM delegations p
                     WHERE p.root_turn_id = d.root_turn_id AND p.status IN ('queued', 'running')
                       AND d.parent_attempt > 0 AND p.parent_attempt = d.parent_attempt)";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DelegateError {
    #[error("only a bot's own conversation can delegate; delegated tasks cannot delegate")]
    NotAllowed,
    #[error("a bot cannot delegate to itself")]
    SelfTarget,
    #[error("that bot is not available")]
    BotUnavailable,
    #[error("this request already used its {MAX_CHILDREN} delegations")]
    BudgetExhausted,
    #[error("this request already used its {MAX_COUNCIL_BOTS} council reports")]
    CouncilBudgetExhausted,
    #[error("this attempt already delegated; wait for the result")]
    AttemptParked,
    #[error("the task must be 1 to {MAX_TASK_BYTES} bytes")]
    InvalidTask,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("a council needs 1 to 32 distinct bot IDs")]
    InvalidCouncil,
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for DelegateError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "delegation failed");
        DelegateError::Db
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accepted {
    pub delegation_id: Uuid,
    pub target_bot_id: Uuid,
    pub target_name: String,
    pub status: String,
    /// For a retry of a child that already ended: its result or error.
    pub outcome: Option<String>,
    /// False when this was a retry of an existing delegation.
    pub created: bool,
}

/// A delegation as shown under the delegating turn: one card per provider
/// attempt, so a council's reports share a card.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct Handoff {
    pub id: Uuid,
    #[serde(skip)]
    pub turn_id: Uuid,
    pub attempt: i64,
    pub target_bot_id: Uuid,
    pub target_name: String,
    pub task: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// An attempt of the delegating turn has received the outcome.
    pub seen: bool,
    /// Messages the task's turn has written so far.
    pub updates: i64,
    /// While the task runs, the start of its latest message.
    pub progress: Option<String>,
}

/// The delegations started by each of `turn_ids`, oldest first.
pub async fn for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Handoff>>, sqlx::Error> {
    let rows: Vec<Handoff> = sqlx::query_as(
        "SELECT d.id, d.root_turn_id AS turn_id, d.parent_attempt AS attempt, d.target_bot_id,
                b.name AS target_name, d.task, d.status, d.created_at, d.finished_at,
                d.delivered_attempt IS NOT NULL AS seen,
                (SELECT count(*) FROM items i
                 WHERE i.turn_id = coalesce(d.takeover_turn_id, d.child_turn_id)
                   AND i.kind = 'assistant') AS updates,
                CASE WHEN d.status IN ('queued', 'running') THEN
                    (SELECT substr(i.text, 1, 400) FROM items i
                     WHERE i.turn_id = coalesce(d.takeover_turn_id, d.child_turn_id)
                       AND i.kind = 'assistant'
                     ORDER BY i.seq DESC LIMIT 1)
                END AS progress
         FROM delegations d JOIN bots b ON b.id = d.target_bot_id
         WHERE lower(hex(d.root_turn_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1))
         ORDER BY d.created_at, d.id",
    )
    .bind(sqlx::types::Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    let mut by_turn: HashMap<Uuid, Vec<Handoff>> = HashMap::new();
    for row in rows {
        by_turn.entry(row.turn_id).or_default().push(row);
    }
    Ok(by_turn)
}

/// Accept a delegation from the provider attempt of `turn_id`. The target
/// receives its own copies of `files`, attached to the task message.
pub async fn create(
    pool: &SqlitePool,
    turn_id: Uuid,
    target: Uuid,
    task: &str,
    files: &[OutputFile],
) -> Result<Accepted, DelegateError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let accepted = create_in(&mut tx, turn_id, target, task, files, false).await?;
    tx.commit().await?;
    Ok(accepted)
}

pub async fn council(
    pool: &SqlitePool,
    turn_id: Uuid,
    targets: &[Uuid],
    task: &str,
) -> Result<Vec<Accepted>, DelegateError> {
    let mut targets = targets.to_vec();
    targets.sort_unstable();
    let count = targets.len();
    targets.dedup();
    if count == 0 || count > MAX_COUNCIL_BOTS || targets.len() != count {
        return Err(DelegateError::InvalidCouncil);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let attempt: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(DelegateError::NotRunning)?;
    let parked = super::turns::attempt_parked(&mut tx, turn_id, attempt).await?;
    let mut accepted = Vec::with_capacity(count);
    for target in targets {
        let result = create_in(&mut tx, turn_id, target, task, &[], true).await?;
        if parked && result.created {
            return Err(DelegateError::AttemptParked);
        }
        accepted.push(result);
    }
    tx.commit().await?;
    Ok(accepted)
}

async fn create_in(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    target: Uuid,
    task: &str,
    files: &[OutputFile],
    council: bool,
) -> Result<Accepted, DelegateError> {
    let task = task.trim();
    if task.is_empty() || task.len() > MAX_TASK_BYTES || task.contains('\0') {
        return Err(DelegateError::InvalidTask);
    }
    let parent_bot: Option<Uuid> = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(turn_id)
        .fetch_optional(&mut *conn)
        .await?;
    let Some(parent_bot) = parent_bot else {
        return Err(DelegateError::NotRunning);
    };
    sqlx::query("SELECT 1 FROM bots WHERE id = ?1")
        .bind(parent_bot)
        .execute(&mut *conn)
        .await?;
    let caller: Option<(Uuid, String, i32, String, String)> = sqlx::query_as(
        "SELECT t.bot_id, t.status, t.attempt, c.kind, b.status
         FROM turns t JOIN conversations c ON c.id = t.conversation_id
         JOIN bots b ON b.id = t.bot_id
         WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((parent, status, attempt, kind, bot_status)) = caller else {
        return Err(DelegateError::NotRunning);
    };
    if !matches!(status.as_str(), "sending" | "running") || bot_status != "active" {
        return Err(DelegateError::NotRunning);
    }
    if kind != "owner"
        || matches!(
            origin(conn, turn_id).await?,
            crate::app::providers::Origin::ConnectorEvent
                | crate::app::providers::Origin::SocialEvent
        )
    {
        return Err(DelegateError::NotAllowed);
    }
    if target == parent {
        return Err(DelegateError::SelfTarget);
    }
    let mut hasher = Sha256::new();
    hasher.update(target.as_bytes());
    hasher.update(task.as_bytes());
    // A task has no NUL, nor a file name, so NUL ends each one; a task
    // without files keeps the hash it always had.
    for file in files {
        hasher.update(b"\0");
        hasher.update(file.file.name.as_bytes());
        hasher.update(b"\0");
        hasher.update(Sha256::digest(&file.data));
    }
    let hash = hasher.finalize().to_vec();
    let existing: Option<(Uuid, Uuid, String, String, Option<String>)> = sqlx::query_as(
        "SELECT d.id, d.target_bot_id, b.name, d.status, coalesce(d.result, d.error)
         FROM delegations d
         JOIN bots b ON b.id = d.target_bot_id
         WHERE d.root_turn_id = ?1 AND d.task_hash = ?2",
    )
    .bind(turn_id)
    .bind(&hash)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some((id, target_bot_id, target_name, status, outcome)) = existing {
        return Ok(Accepted {
            delegation_id: id,
            target_bot_id,
            target_name,
            status,
            outcome,
            created: false,
        });
    }
    let (total, this_attempt): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE parent_attempt > 0), count(*) FILTER (WHERE parent_attempt = ?2)
         FROM delegations WHERE root_turn_id = ?1",
    )
    .bind(turn_id)
    .bind(attempt)
    .fetch_one(&mut *conn)
    .await?;
    if council {
        if total >= MAX_COUNCIL_BOTS as i64 {
            return Err(DelegateError::CouncilBudgetExhausted);
        }
    } else if total >= MAX_CHILDREN {
        return Err(DelegateError::BudgetExhausted);
    }
    if !council
        && (this_attempt >= PER_ATTEMPT
            || super::turns::attempt_parked(conn, turn_id, attempt).await?)
    {
        return Err(DelegateError::AttemptParked);
    }
    let target_row: Option<(String, String, bool, Option<String>, Uuid, i64)> = sqlx::query_as(
        "SELECT b.status, b.name, b.cancel_pending, b.selected_provider_id, c.id, c.last_turn_no
         FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
         WHERE b.id = ?1",
    )
    .bind(target)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((
        target_status,
        target_name,
        cancel_pending,
        selected,
        owner_conversation,
        last_turn_no,
    )) = target_row
    else {
        return Err(DelegateError::BotUnavailable);
    };
    if target_status != "active" || cancel_pending {
        return Err(DelegateError::BotUnavailable);
    }
    let own: Option<String> =
        sqlx::query_scalar("SELECT provider_id FROM bot_profiles WHERE bot_id = ?1")
            .bind(target)
            .fetch_optional(&mut *conn)
            .await?;
    let provider = match own {
        Some(provider) => provider,
        None => match crate::db::bot_profiles::default_provider(conn).await? {
            Some(provider) => provider,
            None => selected.unwrap_or_else(|| "codex".to_owned()),
        },
    };
    let profile: Option<(i64, sqlx::types::Json<Profile>)> =
        crate::db::bot_profiles::effective(conn, target, &provider).await?;
    let mut parts = Vec::with_capacity(files.len() + 1);
    if !files.is_empty() {
        parts.push(MessagePart::Text {
            text: format!("{task}\n\n"),
        });
    }
    for output in files {
        let file = &output.file;
        let (copy, _) = super::attachments::store_in(
            conn,
            target,
            Uuid::new_v4(),
            &file.name,
            &file.media_type,
            &output.data,
        )
        .await
        .map_err(|err| match err {
            super::attachments::StoreError::Db(err) => DelegateError::from(err),
            _ => DelegateError::BotUnavailable,
        })?;
        parts.push(MessagePart::File { file: copy });
    }
    let (message, receipt) = if parts.is_empty() {
        (task.to_owned(), task.to_owned())
    } else {
        let receipt = parts
            .iter()
            .map(|part| match part {
                MessagePart::Text { text } => text.clone(),
                MessagePart::File { file } => crate::providers::prompt::file_reference(file),
            })
            .collect();
        (crate::app::messages::flatten(&parts), receipt)
    };
    let delegation_id = Uuid::new_v4();
    let conversation_id = Uuid::new_v4();
    let child_turn = Uuid::new_v4();
    let item_id = Uuid::new_v4();
    let receipt_id = Uuid::new_v4();
    let turn_no = last_turn_no + 1;
    let receipt_seq: i64 = sqlx::query_scalar(
        "UPDATE conversations SET last_turn_no = ?2, last_item_seq = last_item_seq + 1
         WHERE id = ?1 RETURNING last_item_seq",
    )
    .bind(owner_conversation)
    .bind(turn_no)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO conversations (id, bot_id, kind, last_item_seq, last_turn_no)
         VALUES (?1, ?2, 'task', 1, ?3)",
    )
    .bind(conversation_id)
    .bind(target)
    .bind(turn_no)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, profile_revision, profile)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(child_turn)
    .bind(target)
    .bind(conversation_id)
    .bind(delegation_id)
    .bind(&hash)
    .bind(turn_no)
    .bind(item_id)
    .bind(&provider)
    .bind(profile.as_ref().map(|(revision, _)| *revision))
    .bind(profile.as_ref().map(|(_, profile)| profile))
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text, parts)
         VALUES (?1, ?2, 1, ?3, 'user', ?4, ?5)",
    )
    .bind(item_id)
    .bind(conversation_id)
    .bind(child_turn)
    .bind(&message)
    .bind((!parts.is_empty()).then_some(sqlx::types::Json(&parts)))
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO delegations (id, root_turn_id, parent_attempt, parent_bot_id, target_bot_id,
             task, task_hash, conversation_id, child_turn_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(delegation_id)
    .bind(turn_id)
    .bind(attempt)
    .bind(parent)
    .bind(target)
    .bind(task)
    .bind(&hash)
    .bind(conversation_id)
    .bind(child_turn)
    .execute(&mut *conn)
    .await?;
    let sender: String = sqlx::query_scalar("SELECT name FROM bots WHERE id = ?1")
        .bind(parent)
        .fetch_one(&mut *conn)
        .await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, ?3, ?4, 'status', ?5)",
    )
    .bind(receipt_id)
    .bind(owner_conversation)
    .bind(receipt_seq)
    .bind(child_turn)
    .bind(format!(
        "Received a task from {sender}. I'll take care of it for you.\n\n{receipt}"
    ))
    .execute(&mut *conn)
    .await?;
    crate::db::memory::log_item(conn, item_id).await?;
    events::item_upsert(conn, target, receipt_id).await?;
    events::turn_changed(conn, target, child_turn).await?;
    Ok(Accepted {
        delegation_id,
        target_bot_id: target,
        target_name,
        status: "queued".into(),
        outcome: None,
        created: true,
    })
}

/// Lock, in ID order, the target bots of children of `bot_id`'s roots that
/// are still pending. Call before locking any of those roots or child turns:
/// a child's own transactions lock its bot before its turn and the root.
pub async fn lock_targets_of_bot(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT 1 FROM bots WHERE id IN (
             SELECT target_bot_id FROM delegations
             WHERE parent_bot_id = ?1 AND status IN ('queued', 'running'))
         ORDER BY id",
    )
    .bind(bot_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The same for one root's pending children.
pub async fn lock_targets_of_root(
    conn: &mut SqliteConnection,
    root: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "SELECT 1 FROM bots WHERE id IN (
             SELECT target_bot_id FROM delegations
             WHERE root_turn_id = ?1 AND status IN ('queued', 'running'))
         ORDER BY id",
    )
    .bind(root)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The owner's new message `turn_id` in `bot_id`'s chat steers the work
/// delegated to that bot: a task it is starting or running, a parked
/// task whose owner card (question, secret, draft, or connection) is pending
/// there, and a task an earlier chat message in `conversation_id` took over.
/// A task parked on anything else holds no slot and keeps waiting. The
/// message's outcome becomes the delegation's; the caller then skips the
/// tasks' cards and stops their child turns.
pub async fn take_over(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    conversation_id: Uuid,
    turn_no: i64,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE delegations SET takeover_turn_id = ?4
         WHERE target_bot_id = ?1 AND status IN ('queued', 'running')
           AND (takeover_turn_id IN (
                    SELECT id FROM turns WHERE conversation_id = ?2 AND turn_no < ?3
                      AND status IN ('queued', 'waiting', 'preparing', 'sending', 'running'))
                OR (takeover_turn_id IS NULL AND child_turn_id IN (
                    SELECT id FROM turns WHERE status IN ('preparing', 'sending', 'running')
                    UNION ALL SELECT id FROM turns WHERE status = 'waiting' AND id IN (
                        SELECT turn_id FROM owner_questions WHERE status = 'pending'
                        UNION ALL SELECT turn_id FROM secret_requests WHERE status = 'pending'
                        UNION ALL SELECT turn_id FROM outgoing_drafts WHERE status = 'pending'
                        UNION ALL SELECT turn_id FROM social_setups WHERE status = 'pending'
                        UNION ALL SELECT turn_id FROM skill_installs WHERE status = 'pending'))))",
    )
    .bind(bot_id)
    .bind(conversation_id)
    .bind(turn_no)
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The owner's new message `turn_id` ends the earlier active turns of
/// `conversation_id`; it receives the outcomes they have not seen yet and
/// waits for their tasks still in progress, so a follow-up never stops
/// work it did not ask to stop. A task the owner took over keeps running
/// in the target's chat however this conversation ends; this message waits
/// for it unless that task already waits on this message, in which case
/// the caller stops it. Carried tasks keep attempt 0, so they count neither
/// toward this message's budget nor as its attempt's own delegation. A task
/// still running for a routine run is not carried: its authority comes from
/// its root's source item, and an owner message would widen it.
pub async fn carry_forward(
    conn: &mut SqliteConnection,
    conversation_id: Uuid,
    turn_no: i64,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    const EARLIER: &str = "SELECT id FROM turns WHERE conversation_id = ?1 AND turn_no < ?2
         AND status IN ('queued', 'waiting', 'preparing', 'sending', 'running')";
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE delegations SET root_turn_id = ?3, parent_attempt = 0
         WHERE root_turn_id IN ({EARLIER})
           AND ((takeover_turn_id IS NULL AND status IN ('queued', 'running')
                 AND root_turn_id IN (SELECT t.id FROM turns t
                                      JOIN items i ON i.id = t.owner_item_id
                                      WHERE i.kind = 'user'))
                OR (delivered_attempt IS NULL AND status IN ('completed', 'failed', 'cancelled')))"
    )))
    .bind(conversation_id)
    .bind(turn_no)
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    let taken_over: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, takeover_turn_id FROM delegations
         WHERE root_turn_id IN ({EARLIER}) AND takeover_turn_id IS NOT NULL
           AND status IN ('queued', 'running')"
    )))
    .bind(conversation_id)
    .bind(turn_no)
    .fetch_all(&mut *conn)
    .await?;
    for (id, owner_turn) in taken_over {
        let waits_on_this: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "WITH RECURSIVE waits(turn) AS (
                 SELECT ?4
                 UNION SELECT coalesce(d.takeover_turn_id, d.child_turn_id)
                 FROM delegations d JOIN waits w ON d.root_turn_id = w.turn
                 WHERE d.status IN ('queued', 'running'))
             SELECT EXISTS (SELECT 1 FROM waits
                            WHERE turn = ?3 OR turn IN ({EARLIER}))"
        )))
        .bind(conversation_id)
        .bind(turn_no)
        .bind(turn_id)
        .bind(owner_turn)
        .fetch_one(&mut *conn)
        .await?;
        if !waits_on_this {
            sqlx::query(
                "UPDATE delegations SET root_turn_id = ?2, parent_attempt = 0 WHERE id = ?1",
            )
            .bind(id)
            .bind(turn_id)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

async fn stop_children(
    conn: &mut SqliteConnection,
    children: Vec<(Uuid, Uuid, Option<Uuid>)>,
    message: &str,
) -> Result<(), sqlx::Error> {
    for (child, target, takeover) in children {
        if let Some(owner_turn) = takeover {
            // The owner's own chat turn keeps running; only the task ends.
            let changed = sqlx::query(
                "UPDATE delegations SET status = 'cancelled', error = 'cancelled: ' || ?2,
                     finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE child_turn_id = ?1 AND status IN ('queued', 'running')",
            )
            .bind(child)
            .bind(message)
            .execute(&mut *conn)
            .await?
            .rows_affected();
            if changed > 0 {
                events::turn_changed(conn, target, owner_turn).await?;
            }
            continue;
        }
        let changed = sqlx::query(
            "UPDATE turns SET
                 status = CASE WHEN status IN ('queued', 'waiting') THEN 'interrupted' ELSE 'cancelling' END,
                 error_code = 'cancelled',
                 error_message = ?2,
                 finished_at = CASE WHEN status IN ('queued', 'waiting') THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') ELSE finished_at END,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status IN ('queued', 'waiting', 'preparing', 'sending', 'running')",
        )
        .bind(child)
        .bind(message)
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if changed > 0 {
            events::turn_changed(conn, target, child).await?;
        }
    }
    Ok(())
}

/// Stop work delegated by roots of `bot_id` up to `boundary`.
pub async fn cancel_for_bot(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    boundary: i64,
) -> Result<(), sqlx::Error> {
    let children: Vec<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT d.child_turn_id, d.target_bot_id, d.takeover_turn_id
         FROM delegations d JOIN turns r ON r.id = d.root_turn_id
         WHERE d.parent_bot_id = ?1 AND r.turn_no <= ?2 AND d.status IN ('queued', 'running')
         ORDER BY d.created_at",
    )
    .bind(bot_id)
    .bind(boundary)
    .fetch_all(&mut *conn)
    .await?;
    stop_children(conn, children, "The delegating request was stopped.").await
}

/// Stop the pending children of one root whose attempt ended without a
/// result to wait for (failed, uncertain, or reconciled after a restart).
pub async fn cancel_for_root(conn: &mut SqliteConnection, root: Uuid) -> Result<(), sqlx::Error> {
    let children: Vec<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT child_turn_id, target_bot_id, takeover_turn_id FROM delegations
         WHERE root_turn_id = ?1 AND status IN ('queued', 'running') ORDER BY created_at",
    )
    .bind(root)
    .fetch_all(&mut *conn)
    .await?;
    stop_children(conn, children, "The delegating request ended.").await
}

/// Show a settled task's reply in the delegating bot's conversation, on
/// the root turn, once: the item's ID is the delegation's. `turn` is the
/// child turn, or the owner chat turn that took the task over. Every
/// terminal transition journals `turn_changed` in its own transaction,
/// which calls this, so the reply commits with the outcome and before the
/// root resumes. Display only: provider history and memory skip it.
pub async fn publish_reply(conn: &mut SqliteConnection, turn: Uuid) -> Result<(), sqlx::Error> {
    let rows: Vec<(Uuid, Uuid, Uuid, Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT d.id, d.parent_bot_id, d.target_bot_id, d.root_turn_id, r.conversation_id,
                coalesce(nullif(d.result, ''), nullif(c.error_message, ''),
                    CASE d.status WHEN 'failed' THEN 'Task failed before it could reply.'
                         WHEN 'cancelled' THEN 'Task stopped before it could reply.' ELSE '' END)
         FROM delegations d JOIN turns r ON r.id = d.root_turn_id
         JOIN turns c ON c.id = ?1
         JOIN bots p ON p.id = d.parent_bot_id
         WHERE ((d.child_turn_id = ?1 AND d.takeover_turn_id IS NULL) OR d.takeover_turn_id = ?1)
           AND d.status IN ('completed', 'failed', 'cancelled')
           AND p.status = 'active' AND NOT EXISTS (SELECT 1 FROM items WHERE id = d.id)
         ORDER BY d.created_at",
    )
    .bind(turn)
    .fetch_all(&mut *conn)
    .await?;
    for (id, parent, target, root, conversation, text) in rows {
        for (source, copy) in returned_files(conn, id, turn).await? {
            sqlx::query(
                "INSERT OR IGNORE INTO attachments (id, bot_id, name, media_type, size, sha256, data)
                 SELECT ?1, ?2, name, media_type, size, sha256, data FROM attachments
                 WHERE id = ?3 AND bot_id = ?4",
            )
            .bind(copy)
            .bind(parent)
            .bind(source)
            .bind(target)
            .execute(&mut *conn)
            .await?;
        }
        let seq: i64 = sqlx::query_scalar(
            "UPDATE conversations SET last_item_seq = last_item_seq + 1 WHERE id = ?1
             RETURNING last_item_seq",
        )
        .bind(conversation)
        .fetch_one(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
             VALUES (?1, ?2, ?3, ?4, 'status', ?5)",
        )
        .bind(id)
        .bind(conversation)
        .bind(seq)
        .bind(root)
        .bind(text)
        .execute(&mut *conn)
        .await?;
        events::item_upsert(conn, parent, id).await?;
    }
    Ok(())
}

/// The files a task's replies attached, at most a message's worth, each
/// paired with the ID of the delegating bot's copy. The copy's ID derives
/// from the delegation and the source, so copying twice is a no-op.
async fn returned_files(
    conn: &mut SqliteConnection,
    delegation: Uuid,
    turn: Uuid,
) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
    let replies: Vec<sqlx::types::Json<Vec<MessagePart>>> = sqlx::query_scalar(
        "SELECT assistant_parts FROM items
         WHERE turn_id = ?1 AND kind = 'assistant' AND assistant_parts IS NOT NULL ORDER BY seq",
    )
    .bind(turn)
    .fetch_all(&mut *conn)
    .await?;
    let mut sources: Vec<Uuid> = Vec::new();
    for part in replies.iter().flat_map(|parts| parts.0.iter()) {
        if let MessagePart::File { file } = part
            && !sources.contains(&file.id)
            && sources.len() < crate::app::messages::MAX_MESSAGE_FILES
        {
            sources.push(file.id);
        }
    }
    Ok(sources
        .into_iter()
        .map(|source| {
            let digest = Sha256::new()
                .chain_update(b"delegation-file")
                .chain_update(delegation.as_bytes())
                .chain_update(source.as_bytes())
                .finalize();
            let bytes: [u8; 16] = digest[..16].try_into().expect("digest has 16 bytes");
            (source, uuid::Builder::from_random_bytes(bytes).into_uuid())
        })
        .collect())
}

pub use crate::app::providers::DelegationOutcome as Outcome;

/// Outcomes of a root's settled batches not yet given to a parent attempt.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    root: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct OutcomeRow {
        id: Uuid,
        parent_bot_id: Uuid,
        turn: Uuid,
        target_bot_id: Uuid,
        target_name: String,
        task: String,
        status: String,
        result: Option<String>,
        error: Option<String>,
    }
    let rows: Vec<OutcomeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT d.id, d.parent_bot_id, coalesce(d.takeover_turn_id, d.child_turn_id) AS turn,
                d.target_bot_id, b.name AS target_name, d.task, d.status, d.result, d.error
             FROM delegations d JOIN bots b ON b.id = d.target_bot_id
             WHERE d.root_turn_id = ?1 AND {READY}
             ORDER BY d.created_at"
    )))
    .bind(root)
    .fetch_all(&mut *conn)
    .await?;
    let mut outcomes = Vec::with_capacity(rows.len());
    for row in rows {
        let mut files = Vec::new();
        for (_, copy) in returned_files(conn, row.id, row.turn).await? {
            let file: Option<(Uuid, String, String, i64)> = sqlx::query_as(
                "SELECT id, name, media_type, size FROM attachments WHERE id = ?1 AND bot_id = ?2",
            )
            .bind(copy)
            .bind(row.parent_bot_id)
            .fetch_optional(&mut *conn)
            .await?;
            files.extend(file.map(|(id, name, media_type, size)| Attachment {
                id,
                name,
                media_type,
                size: size as u64,
            }));
        }
        outcomes.push(Outcome {
            id: row.id,
            target_bot_id: row.target_bot_id,
            target_name: row.target_name,
            task: row.task,
            status: row.status,
            result: row.result,
            error: row.error,
            files,
        });
    }
    Ok(outcomes)
}

/// Record that `attempt` received these outcomes, and log each to the
/// parent bot's memory as tool activity (derived data, never owner text).
pub async fn mark_delivered(
    conn: &mut SqliteConnection,
    root: Uuid,
    attempt: i32,
    ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    #[derive(sqlx::FromRow)]
    struct DeliveredRow {
        id: Uuid,
        parent_bot_id: Uuid,
        conversation_id: Uuid,
        status: String,
        task: String,
        result: Option<String>,
        error: Option<String>,
    }
    let rows: Vec<DeliveredRow> =
        sqlx::query_as(
            "UPDATE delegations AS d SET delivered_attempt = ?3
             FROM turns r
             WHERE lower(hex(d.id)) IN (SELECT replace(value, '-', '') FROM json_each(?2)) AND d.root_turn_id = ?1 AND d.delivered_attempt IS NULL
               AND r.id = d.root_turn_id
             RETURNING id, parent_bot_id, (SELECT conversation_id FROM turns WHERE id = root_turn_id) AS conversation_id, status, task, result, error",
        )
        .bind(root)
        .bind(sqlx::types::Json(ids))
        .bind(attempt)
        .fetch_all(&mut *conn)
        .await?;
    for DeliveredRow {
        id,
        parent_bot_id: parent,
        conversation_id: conversation,
        status,
        task,
        result,
        error,
    } in rows
    {
        let key = format!("delegation:{id}");
        let body = format!(
            "delegated task {id} ({status}): {task}\n-> {}",
            result.or(error).unwrap_or_default()
        );
        let entry = crate::db::memory::NewEntry {
            request_key: &key,
            kind: crate::db::memory::Kind::ToolActivity,
            body: crate::db::memory::split(&body, 32_000)[0],
            source: json!({ "delegation_id": id, "root_turn_id": root,
                            "conversation_id": conversation, "tool": "delegate" }),
            supersedes_seq: None,
        };
        match crate::db::memory::append(conn, parent, &entry).await {
            Ok(_) | Err(crate::db::memory::AppendError::BotInactive) => {}
            Err(crate::db::memory::AppendError::Db(err)) => return Err(err),
            Err(_) => {}
        }
    }
    Ok(())
}

pub use crate::app::providers::DelegationContext as Context;
use crate::app::providers::DelegationInProgress as InProgress;
use crate::app::providers::Origin;

/// What a turn's work traces back to: the source item of the turn itself
/// in an owner conversation, or of the root that delegated it for a task.
/// Never derived from text a model wrote.
pub async fn origin(conn: &mut SqliteConnection, turn_id: Uuid) -> Result<Origin, sqlx::Error> {
    let source: Option<String> = sqlx::query_scalar(
        "SELECT CASE c.kind WHEN 'owner' THEN i.kind ELSE ri.kind END
         FROM turns t JOIN conversations c ON c.id = t.conversation_id
         JOIN items i ON i.id = t.owner_item_id
         LEFT JOIN delegations d ON d.child_turn_id = t.id
         LEFT JOIN turns r ON r.id = d.root_turn_id
         LEFT JOIN items ri ON ri.id = r.owner_item_id
         WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    Ok(match source.as_deref() {
        Some("user") => Origin::OwnerRequest,
        Some("routine") => Origin::RoutineRun,
        Some("connector") => {
            let social: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM social_deliveries WHERE turn_id=?1)",
            )
            .bind(turn_id)
            .fetch_one(&mut *conn)
            .await?;
            if social {
                Origin::SocialEvent
            } else {
                Origin::ConnectorEvent
            }
        }
        _ => Origin::Other,
    })
}

pub async fn context(conn: &mut SqliteConnection, turn_id: Uuid) -> Result<Context, sqlx::Error> {
    let (bot, kind, main): (Uuid, String, Option<Uuid>) = sqlx::query_as(
        "SELECT t.bot_id, c.kind, (SELECT default_bot_id FROM installation)
         FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_one(&mut *conn)
    .await?;
    let origin = origin(conn, turn_id).await?;
    if kind == "task" {
        let parent: Option<(Uuid, String)> = sqlx::query_as(
            "SELECT d.parent_bot_id, b.name FROM delegations d JOIN bots b ON b.id = d.parent_bot_id
             WHERE d.child_turn_id = ?1",
        )
        .bind(turn_id)
        .fetch_optional(&mut *conn)
        .await?;
        return Ok(Context {
            origin,
            delegated_task: true,
            delegated_by: parent,
            ..Context::default()
        });
    }
    let registry: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id, name FROM bots WHERE status = 'active' AND id <> ?1
         ORDER BY created_at, id",
    )
    .bind(bot)
    .fetch_all(&mut *conn)
    .await?;
    let used: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM delegations WHERE root_turn_id = ?1 AND parent_attempt > 0",
    )
    .bind(turn_id)
    .fetch_one(&mut *conn)
    .await?;
    let in_progress: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "SELECT d.id, d.target_bot_id, b.name, d.task
         FROM delegations d JOIN bots b ON b.id = d.target_bot_id
         WHERE d.root_turn_id = ?1 AND d.status IN ('queued', 'running')
         ORDER BY d.created_at, d.id",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Context {
        origin,
        delegated_task: false,
        delegated_by: None,
        main_bot: main == Some(bot),
        can_create_bot: origin == Origin::OwnerRequest,
        registry,
        remaining: (MAX_CHILDREN - used).max(0),
        outcomes: undelivered(conn, turn_id).await?,
        in_progress: in_progress
            .into_iter()
            .map(|(id, target_bot_id, target_name, task)| InProgress {
                id,
                target_bot_id,
                target_name,
                task,
            })
            .collect(),
    })
}
