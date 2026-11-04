//! One-hop asynchronous delegation (POC-016).
//!
//! The main bot's provider attempt may hand one task to another registered
//! bot, at most two per owner request. The child runs as an ordinary queued
//! turn of the target bot, in a separate task conversation, through the
//! same scheduler. The parent attempt then ends; its owner turn waits
//! (`waiting`) with no provider process, and a database trigger requeues it
//! as a continuation once every child has an outcome. Identity, budget,
//! target, and permissions come from the server, never from tool arguments.

use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use crate::app::providers::Profile;

/// Children per owner request (root turn).
pub const MAX_CHILDREN: i64 = 2;
/// Children per provider attempt: the attempt parks after one.
pub const PER_ATTEMPT: i64 = 1;
pub const MAX_TASK_BYTES: usize = 8_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DelegateError {
    #[error("only the main bot's own conversation can delegate; delegated tasks cannot delegate")]
    NotAllowed,
    #[error("a bot cannot delegate to itself")]
    SelfTarget,
    #[error("that bot is not available")]
    BotUnavailable,
    #[error("this request already used its {MAX_CHILDREN} delegations")]
    BudgetExhausted,
    #[error("this attempt already delegated; wait for the result")]
    AttemptParked,
    #[error("the task must be 1 to {MAX_TASK_BYTES} bytes")]
    InvalidTask,
    #[error("the request is no longer running")]
    NotRunning,
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

/// Accept a delegation from the provider attempt of `turn_id`.
pub async fn create(
    pool: &SqlitePool,
    turn_id: Uuid,
    target: Uuid,
    task: &str,
) -> Result<Accepted, DelegateError> {
    let task = task.trim();
    if task.is_empty() || task.len() > MAX_TASK_BYTES || task.contains('\0') {
        return Err(DelegateError::InvalidTask);
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // Lock order everywhere: bot rows (parent before target), then turns.
    let parent_bot: Option<Uuid> = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(turn_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(parent_bot) = parent_bot else {
        return Err(DelegateError::NotRunning);
    };
    sqlx::query("SELECT 1 FROM bots WHERE id = ?1")
        .bind(parent_bot)
        .execute(&mut *tx)
        .await?;
    let caller: Option<(Uuid, String, i32, String, Option<Uuid>, String)> = sqlx::query_as(
        "SELECT t.bot_id, t.status, t.attempt, c.kind,
                (SELECT default_bot_id FROM installation), b.status
         FROM turns t JOIN conversations c ON c.id = t.conversation_id
         JOIN bots b ON b.id = t.bot_id
         WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((parent, status, attempt, kind, main, bot_status)) = caller else {
        return Err(DelegateError::NotRunning);
    };
    if !matches!(status.as_str(), "sending" | "running") || bot_status != "active" {
        return Err(DelegateError::NotRunning);
    }
    // Main to leaf only: the installation's main bot, in its own owner
    // conversation. Delegated tasks run in task conversations and stop here.
    if kind != "owner" || main != Some(parent) {
        return Err(DelegateError::NotAllowed);
    }
    if target == parent {
        return Err(DelegateError::SelfTarget);
    }
    // A retry is the same task to the same target.
    let mut hasher = Sha256::new();
    hasher.update(target.as_bytes());
    hasher.update(task.as_bytes());
    let hash = hasher.finalize().to_vec();
    let existing: Option<(Uuid, Uuid, String, String, Option<String>)> = sqlx::query_as(
        "SELECT d.id, d.target_bot_id, b.name, d.status, coalesce(d.result, d.error)
         FROM delegations d
         JOIN bots b ON b.id = d.target_bot_id
         WHERE d.root_turn_id = ?1 AND d.task_hash = ?2",
    )
    .bind(turn_id)
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, target_bot_id, target_name, status, outcome)) = existing {
        // A retried call: the same child, never another, with its status.
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
        "SELECT count(*), count(*) FILTER (WHERE parent_attempt = ?2)
         FROM delegations WHERE root_turn_id = ?1",
    )
    .bind(turn_id)
    .bind(attempt)
    .fetch_one(&mut *tx)
    .await?;
    if total >= MAX_CHILDREN {
        return Err(DelegateError::BudgetExhausted);
    }
    if this_attempt >= PER_ATTEMPT {
        return Err(DelegateError::AttemptParked);
    }
    // The target: active, not stopping, rechecked inside an immediate transaction.
    let target_row: Option<(String, String, bool, Option<String>, Uuid, i64)> = sqlx::query_as(
        "SELECT b.status, b.name, b.cancel_pending, b.selected_provider_id, c.id, c.last_turn_no
         FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
         WHERE b.id = ?1",
    )
    .bind(target)
    .fetch_optional(&mut *tx)
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
    // The child uses the target's own provider and profile, like an owner
    // message would: else the installation's default provider, else the
    // provider it last used, else Codex.
    let own: Option<String> =
        sqlx::query_scalar("SELECT provider_id FROM bot_profiles WHERE bot_id = ?1")
            .bind(target)
            .fetch_optional(&mut *tx)
            .await?;
    let provider = match own {
        Some(provider) => provider,
        None => match crate::db::bot_profiles::default_provider(&mut tx).await? {
            Some(provider) => provider,
            None => selected.unwrap_or_else(|| "codex".to_owned()),
        },
    };
    let profile: Option<(i64, sqlx::types::Json<Profile>)> =
        crate::db::bot_profiles::effective(&mut tx, target, &provider).await?;
    let delegation_id = Uuid::new_v4();
    let conversation_id = Uuid::new_v4();
    let child_turn = Uuid::new_v4();
    let item_id = Uuid::new_v4();
    // Child turns are numbered from the target's owner counter, so a Stop
    // of the target covers them by the same boundary.
    let turn_no = last_turn_no + 1;
    sqlx::query("UPDATE conversations SET last_turn_no = ?2 WHERE id = ?1")
        .bind(owner_conversation)
        .bind(turn_no)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO conversations (id, bot_id, kind, last_item_seq, last_turn_no)
         VALUES (?1, ?2, 'task', 1, ?3)",
    )
    .bind(conversation_id)
    .bind(target)
    .bind(turn_no)
    .execute(&mut *tx)
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
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, 1, ?3, 'user', ?4)",
    )
    .bind(item_id)
    .bind(conversation_id)
    .bind(child_turn)
    .bind(task)
    .execute(&mut *tx)
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
    .execute(&mut *tx)
    .await?;
    // The task enters the target's memory as model-authored text.
    crate::db::memory::log_item(&mut tx, item_id).await?;
    events::bot_updated(&mut tx, target).await?;
    tx.commit().await?;
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

/// End the pending children: a queued child is interrupted at once, a
/// running one moves to `cancelling` for the scheduler to interrupt. The
/// trigger on the child turn records each delegation's outcome. The caller
/// holds the target bot locks (see `lock_targets_of_*`).
async fn stop_children(
    conn: &mut SqliteConnection,
    children: Vec<(Uuid, Uuid)>,
    message: &str,
) -> Result<(), sqlx::Error> {
    for (child, target) in children {
        let changed = sqlx::query(
            "UPDATE turns SET
                 status = CASE WHEN status = 'queued' THEN 'interrupted' ELSE 'cancelling' END,
                 error_code = 'cancelled',
                 error_message = ?2,
                 finished_at = CASE WHEN status = 'queued' THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') ELSE finished_at END,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status IN ('queued', 'preparing', 'sending', 'running')",
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
    let children: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT d.child_turn_id, d.target_bot_id FROM delegations d JOIN turns r ON r.id = d.root_turn_id
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
    let children: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT child_turn_id, target_bot_id FROM delegations
         WHERE root_turn_id = ?1 AND status IN ('queued', 'running') ORDER BY created_at",
    )
    .bind(root)
    .fetch_all(&mut *conn)
    .await?;
    stop_children(conn, children, "The delegating request ended.").await
}

pub use crate::app::providers::DelegationOutcome as Outcome;

/// Outcomes of a root's children not yet given to a parent attempt.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    root: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    let rows: Vec<(
        Uuid,
        Uuid,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "SELECT d.id, d.target_bot_id, b.name, d.task, d.status, d.result, d.error
             FROM delegations d JOIN bots b ON b.id = d.target_bot_id
             WHERE d.root_turn_id = ?1 AND d.delivered_attempt IS NULL
               AND d.status IN ('completed', 'failed', 'cancelled')
             ORDER BY d.created_at",
    )
    .bind(root)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, target_bot_id, target_name, task, status, result, error)| Outcome {
                id,
                target_bot_id,
                target_name,
                task,
                status,
                result,
                error,
            },
        )
        .collect())
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
    let rows: Vec<(Uuid, Uuid, Uuid, String, String, Option<String>, Option<String>)> =
        sqlx::query_as(
            "UPDATE delegations AS d SET delivered_attempt = ?3
             FROM turns r
             WHERE lower(hex(d.id)) IN (SELECT replace(value, '-', '') FROM json_each(?2)) AND d.root_turn_id = ?1 AND d.delivered_attempt IS NULL
               AND r.id = d.root_turn_id
             RETURNING id, parent_bot_id, (SELECT conversation_id FROM turns WHERE id = root_turn_id), status, task, result, error",
        )
        .bind(root)
        .bind(sqlx::types::Json(ids))
        .bind(attempt)
        .fetch_all(&mut *conn)
        .await?;
    for (id, parent, conversation, status, task, result, error) in rows {
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

pub async fn context(conn: &mut SqliteConnection, turn_id: Uuid) -> Result<Context, sqlx::Error> {
    let (bot, kind, main): (Uuid, String, Option<Uuid>) = sqlx::query_as(
        "SELECT t.bot_id, c.kind, (SELECT default_bot_id FROM installation)
         FROM turns t JOIN conversations c ON c.id = t.conversation_id WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_one(&mut *conn)
    .await?;
    if kind == "task" {
        let parent: Option<Uuid> =
            sqlx::query_scalar("SELECT parent_bot_id FROM delegations WHERE child_turn_id = ?1")
                .bind(turn_id)
                .fetch_optional(&mut *conn)
                .await?;
        return Ok(Context {
            delegated_task: true,
            parent_bot_id: parent,
            ..Context::default()
        });
    }
    if main != Some(bot) {
        return Ok(Context::default());
    }
    let registry: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, name, role FROM bots WHERE status = 'active' AND id <> ?1
         ORDER BY created_at, id",
    )
    .bind(bot)
    .fetch_all(&mut *conn)
    .await?;
    let used: i64 = sqlx::query_scalar("SELECT count(*) FROM delegations WHERE root_turn_id = ?1")
        .bind(turn_id)
        .fetch_one(&mut *conn)
        .await?;
    Ok(Context {
        can_delegate: true,
        delegated_task: false,
        parent_bot_id: None,
        registry,
        remaining: (MAX_CHILDREN - used).max(0),
        outcomes: undelivered(conn, turn_id).await?,
    })
}
