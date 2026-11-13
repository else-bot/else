//! The delegating bot's control over a task it already sent.
//!
//! A turn may update or cancel the tasks it waits on: the ones it delegated
//! and the ones carried to it from an earlier owner message. An update
//! continues the same task conversation in a new turn that replaces the
//! current one, so the target sees its earlier work and the change, and the
//! delegation's outcome follows the new turn. A cancel commits the stop
//! (the turn moves to `interrupted` or `cancelling`) before the scheduler
//! interrupts the provider; the task reports `stopping` until the turn ends.

use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{MAX_TASK_BYTES, events, lock_targets_of_root, mark_delivered, stop_children};
use crate::db::turns::TurnStatus;

/// A delegation's lifecycle, as stored in `delegations.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    fn is_ended(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Where a task stands for the bot that delegated it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Running,
    /// A stop was committed; the target's provider has not ended yet.
    Stopping,
    Ended {
        status: Status,
        /// The task's reply, or its error.
        outcome: Option<String>,
    },
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Ended { status, .. } => status.as_str(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: Uuid,
    pub target_bot_id: Uuid,
    pub target_name: String,
    pub state: TaskState,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ControlError {
    #[error("that task_id is not a task this request is waiting on")]
    NotFound,
    #[error("the message must be 1 to {MAX_TASK_BYTES} bytes")]
    InvalidMessage,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("that bot is not available")]
    BotUnavailable,
    #[error("the task already ended")]
    Ended(Box<Task>),
    #[error("the owner took this task over in that bot's chat")]
    TakenOver(Box<Task>),
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for ControlError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "task control failed");
        ControlError::Db
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    parent_bot_id: Uuid,
    target_bot_id: Uuid,
    conversation_id: Uuid,
    child_turn_id: Uuid,
    takeover_turn_id: Option<Uuid>,
    status: Status,
}

/// Validate the caller and lock what the change touches: the target bots
/// first, as the child's own transactions do, then the root.
async fn owned(conn: &mut SqliteConnection, caller: Uuid, task: Uuid) -> Result<Row, ControlError> {
    lock_targets_of_root(conn, caller).await?;
    let running: Option<bool> = sqlx::query_scalar(
        "SELECT t.status IN ('sending', 'running') AND b.status = 'active'
         FROM turns t JOIN bots b ON b.id = t.bot_id WHERE t.id = ?1",
    )
    .bind(caller)
    .fetch_optional(&mut *conn)
    .await?;
    if running != Some(true) {
        return Err(ControlError::NotRunning);
    }
    sqlx::query_as(
        "SELECT parent_bot_id, target_bot_id, conversation_id, child_turn_id, takeover_turn_id,
                status
         FROM delegations WHERE id = ?1 AND root_turn_id = ?2",
    )
    .bind(task)
    .bind(caller)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(ControlError::NotFound)
}

/// The task as stored now, read back after any change.
pub async fn state(conn: &mut SqliteConnection, task: Uuid) -> Result<Task, sqlx::Error> {
    let (target_bot_id, target_name, status, outcome, turn): (
        Uuid,
        String,
        Status,
        Option<String>,
        TurnStatus,
    ) = sqlx::query_as(
        "SELECT d.target_bot_id, b.name, d.status, coalesce(d.result, d.error), t.status
         FROM delegations d JOIN bots b ON b.id = d.target_bot_id
         JOIN turns t ON t.id = coalesce(d.takeover_turn_id, d.child_turn_id)
         WHERE d.id = ?1",
    )
    .bind(task)
    .fetch_one(&mut *conn)
    .await?;
    let state = match status {
        status if status.is_ended() => TaskState::Ended { status, outcome },
        _ if turn == TurnStatus::Cancelling => TaskState::Stopping,
        Status::Queued => TaskState::Queued,
        _ => TaskState::Running,
    };
    Ok(Task {
        id: task,
        target_bot_id,
        target_name,
        state,
    })
}

/// A result that shows an ended task's outcome delivers it to the caller's
/// current attempt, so no continuation repeats it.
async fn deliver_if_ended(
    conn: &mut SqliteConnection,
    caller: Uuid,
    task: &Task,
) -> Result<(), sqlx::Error> {
    if matches!(task.state, TaskState::Ended { .. }) {
        let attempt: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
            .bind(caller)
            .fetch_one(&mut *conn)
            .await?;
        mark_delivered(conn, caller, attempt, &[task.id]).await?;
    }
    Ok(())
}

fn update_request_id(task: Uuid, message: &str) -> Uuid {
    let mut hasher = Sha256::new();
    hasher.update(b"delegation-update\0");
    hasher.update(task.as_bytes());
    hasher.update(message.as_bytes());
    let digest = hasher.finalize();
    Uuid::from_bytes(
        digest[..16]
            .try_into()
            .expect("a SHA-256 digest has 16 bytes"),
    )
}

pub struct Updated {
    pub task: Task,
    /// False when this repeated an update already applied.
    pub created: bool,
}

/// Steer task `task` of the running turn `caller` with `message`. The
/// current task turn ends as superseded, and a new turn in the same task
/// conversation carries the message; it goes ahead of the target's other
/// queued work. The same message again returns the task without a change.
pub async fn update(
    pool: &SqlitePool,
    caller: Uuid,
    task: Uuid,
    message: &str,
) -> Result<Updated, ControlError> {
    let message = message.trim();
    if message.is_empty() || message.len() > MAX_TASK_BYTES || message.contains('\0') {
        return Err(ControlError::InvalidMessage);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row = owned(&mut tx, caller, task).await?;
    let request_id = update_request_id(task, message);
    let repeated: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM turns WHERE conversation_id = ?1 AND request_id = ?2)",
    )
    .bind(row.conversation_id)
    .bind(request_id)
    .fetch_one(&mut *tx)
    .await?;
    if repeated {
        return Ok(Updated {
            task: state(&mut tx, task).await?,
            created: false,
        });
    }
    if row.status.is_ended() {
        let task = state(&mut tx, task).await?;
        deliver_if_ended(&mut tx, caller, &task).await?;
        tx.commit().await?;
        return Err(ControlError::Ended(Box::new(task)));
    }
    if row.takeover_turn_id.is_some() {
        return Err(ControlError::TakenOver(Box::new(
            state(&mut tx, task).await?,
        )));
    }
    let target: Option<(Uuid, i64)> = sqlx::query_as(
        "SELECT c.id, c.last_turn_no
         FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
         WHERE b.id = ?1 AND b.status = 'active' AND NOT b.cancel_pending",
    )
    .bind(row.target_bot_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((owner_conversation, last_turn_no)) = target else {
        return Err(ControlError::BotUnavailable);
    };
    let sender: String = sqlx::query_scalar("SELECT name FROM bots WHERE id = ?1")
        .bind(row.parent_bot_id)
        .fetch_one(&mut *tx)
        .await?;

    let turn_no = last_turn_no + 1;
    let receipt_seq: i64 = sqlx::query_scalar(
        "UPDATE conversations SET last_turn_no = ?2, last_item_seq = last_item_seq + 1
         WHERE id = ?1 RETURNING last_item_seq",
    )
    .bind(owner_conversation)
    .bind(turn_no)
    .fetch_one(&mut *tx)
    .await?;
    let item_seq: i64 = sqlx::query_scalar(
        "UPDATE conversations SET last_turn_no = ?2, last_item_seq = last_item_seq + 1
         WHERE id = ?1 RETURNING last_item_seq",
    )
    .bind(row.conversation_id)
    .bind(turn_no)
    .fetch_one(&mut *tx)
    .await?;
    let new_turn = Uuid::new_v4();
    let item_id = Uuid::new_v4();
    let request_hash = Sha256::digest(message.as_bytes()).to_vec();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, profile_revision, profile)
         SELECT ?1, bot_id, conversation_id, ?2, ?3, ?4, ?5, provider_id, profile_revision, profile
         FROM turns WHERE id = ?6",
    )
    .bind(new_turn)
    .bind(request_id)
    .bind(&request_hash)
    .bind(turn_no)
    .bind(item_id)
    .bind(row.child_turn_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, ?3, ?4, 'user', ?5)",
    )
    .bind(item_id)
    .bind(row.conversation_id)
    .bind(item_seq)
    .bind(new_turn)
    .bind(message)
    .execute(&mut *tx)
    .await?;
    // Move the outcome to the new turn before ending the old one, so the
    // old turn's end no longer settles the delegation.
    let moved = sqlx::query(
        "UPDATE delegations SET child_turn_id = ?2, status = 'queued'
         WHERE id = ?1 AND child_turn_id = ?3 AND takeover_turn_id IS NULL
           AND status IN ('queued', 'running')",
    )
    .bind(task)
    .bind(new_turn)
    .bind(row.child_turn_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if moved != 1 {
        return Err(ControlError::Db);
    }
    let superseded = sqlx::query(
        "UPDATE turns SET
             status = CASE WHEN status IN ('queued', 'waiting') THEN 'interrupted' ELSE 'cancelling' END,
             error_code = 'superseded', error_message = ?2,
             finished_at = CASE WHEN status IN ('queued', 'waiting') THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') ELSE finished_at END,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status IN ('queued', 'waiting', 'preparing', 'sending', 'running')",
    )
    .bind(row.child_turn_id)
    .bind(format!("{sender} updated the task before this attempt finished."))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if superseded > 0 {
        events::turn_changed(&mut tx, row.target_bot_id, row.child_turn_id).await?;
    }
    let receipt_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, ?3, ?4, 'status', ?5)",
    )
    .bind(receipt_id)
    .bind(owner_conversation)
    .bind(receipt_seq)
    .bind(new_turn)
    .bind(format!(
        "Received an update from {sender} on its task.\n\n{message}"
    ))
    .execute(&mut *tx)
    .await?;
    crate::db::memory::log_item(&mut tx, item_id).await?;
    events::item_upsert(&mut tx, row.target_bot_id, receipt_id).await?;
    events::turn_changed(&mut tx, row.target_bot_id, new_turn).await?;
    let task = state(&mut tx, task).await?;
    tx.commit().await?;
    Ok(Updated {
        task,
        created: true,
    })
}

/// Stop task `task` of the running turn `caller`. Commits the stop and
/// returns the task as stored: `cancelled` once its turn ended, `stopping`
/// while the provider still has to be interrupted. Cancelling an ended
/// task returns it unchanged.
pub async fn cancel(pool: &SqlitePool, caller: Uuid, task: Uuid) -> Result<Task, ControlError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row = owned(&mut tx, caller, task).await?;
    if !row.status.is_ended() {
        let sender: String = sqlx::query_scalar("SELECT name FROM bots WHERE id = ?1")
            .bind(row.parent_bot_id)
            .fetch_one(&mut *tx)
            .await?;
        stop_children(
            &mut tx,
            vec![(row.child_turn_id, row.target_bot_id, row.takeover_turn_id)],
            &format!("{sender} cancelled the task."),
        )
        .await?;
    }
    let task = state(&mut tx, task).await?;
    deliver_if_ended(&mut tx, caller, &task).await?;
    tx.commit().await?;
    Ok(task)
}
