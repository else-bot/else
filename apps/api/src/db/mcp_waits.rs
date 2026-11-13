//! A bot's wait on a long-running job in an installed MCP (`mcp_wait`).
//!
//! The attempt names a read-only status tool and the status values that
//! mean the job still runs. It then ends and its turn waits with no
//! provider process. The watcher repeats the status call on the API host
//! and settles the wait once the status leaves those values, the checks
//! keep failing or report no status, or the deadline passes; settling
//! requeues the turn as a continuation that receives the last result.
//! Ending the turn for any reason withdraws a pending wait (a trigger).

use serde_json::Value;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;

pub use crate::app::providers::McpWaitOutcome as Outcome;

pub const MIN_INTERVAL_SECONDS: u32 = 15;
pub const MAX_INTERVAL_SECONDS: u32 = 3600;
pub const MAX_TIMEOUT_MINUTES: u32 = 1440;
/// Consecutive failed checks that end a wait.
pub const MAX_FAILURES: i64 = 3;
/// Bytes of the last status result kept for the continuation.
pub const MAX_RESULT: usize = 16 * 1024;

/// The status call to repeat and when to stop.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    pub mcp_id: String,
    pub tool: String,
    pub arguments: Value,
    pub status_pointer: String,
    pub running_values: Vec<Value>,
    pub purpose: String,
    pub interval_seconds: u32,
    pub timeout_minutes: u32,
}

/// A pending wait the watcher should check now.
#[derive(Debug, Clone)]
pub struct Due {
    pub id: Uuid,
    pub mcp_id: String,
    pub tool: String,
    pub arguments: Value,
    pub status_pointer: String,
    pub running_values: Vec<Value>,
}

/// The result of one status call.
#[derive(Debug, Clone, PartialEq)]
pub enum Check {
    Running(String),
    Finished(String),
    /// A result with no status at the pointer, so whether the job still
    /// runs is unknown. It counts as a failed check, never as finished.
    Unreadable(String),
    /// A failed call; `permanent` ends the wait at once.
    Failed {
        message: String,
        permanent: bool,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CreateError {
    #[error("this attempt already waits or asked; end it and wait for the result")]
    AttemptParked,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for CreateError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "MCP wait failed");
        CreateError::Db
    }
}

pub fn bounded(text: &str) -> String {
    text[..text.floor_char_boundary(MAX_RESULT)].to_owned()
}

/// The running attempt of `turn_id` that may wait, read before any remote
/// call so a slow call cannot park a later attempt.
pub async fn caller(
    pool: &SqlitePool,
    turn_id: Uuid,
) -> Result<super::turns::AskingCaller, CreateError> {
    super::turns::running_caller(pool, turn_id)
        .await?
        .ok_or(CreateError::NotRunning)
}

/// Park `attempt` of `turn_id` on `spec`, whose first check returned
/// `first` and found the job running. A retry of the same wait in this
/// attempt returns its id.
pub async fn create(
    pool: &SqlitePool,
    turn_id: Uuid,
    attempt: i32,
    spec: &Spec,
    first: &str,
) -> Result<Uuid, CreateError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = super::turns::asking_caller(&mut tx, turn_id)
        .await?
        .filter(|caller| caller.attempt == attempt)
        .ok_or(CreateError::NotRunning)?;
    let existing: Option<(Uuid, String, String, Json<Value>, String)> = sqlx::query_as(
        "SELECT id, mcp_id, tool, arguments, status_pointer FROM mcp_waits
         WHERE turn_id = ?1 AND attempt = ?2",
    )
    .bind(turn_id)
    .bind(caller.attempt)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, mcp_id, tool, Json(arguments), pointer)) = existing {
        let same = mcp_id == spec.mcp_id
            && tool == spec.tool
            && arguments == spec.arguments
            && pointer == spec.status_pointer;
        return if same {
            Ok(id)
        } else {
            Err(CreateError::AttemptParked)
        };
    }
    if super::turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
        return Err(CreateError::AttemptParked);
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO mcp_waits (id, bot_id, turn_id, attempt, mcp_id, tool, arguments,
             status_pointer, running_values, purpose, interval_seconds, deadline_at,
             next_check_at, last_result)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
             strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+' || ?12 || ' minutes'),
             strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+' || ?11 || ' seconds'), ?13)",
    )
    .bind(id)
    .bind(caller.bot_id)
    .bind(turn_id)
    .bind(caller.attempt)
    .bind(&spec.mcp_id)
    .bind(&spec.tool)
    .bind(Json(&spec.arguments))
    .bind(&spec.status_pointer)
    .bind(Json(&spec.running_values))
    .bind(&spec.purpose)
    .bind(spec.interval_seconds)
    .bind(spec.timeout_minutes)
    .bind(bounded(first))
    .execute(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(id)
}

/// Pending waits whose next check is due: the oldest of each package not
/// in `busy`, at most `limit` packages, so one package with many waits
/// cannot hide another.
pub async fn due(
    pool: &SqlitePool,
    busy: &[String],
    limit: usize,
) -> Result<Vec<Due>, sqlx::Error> {
    type Row = (Uuid, String, String, Json<Value>, String, Json<Vec<Value>>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, mcp_id, tool, arguments, status_pointer, running_values FROM (
             SELECT *, row_number() OVER (PARTITION BY mcp_id ORDER BY next_check_at, id) AS rank
             FROM mcp_waits
             WHERE status = 'pending'
               AND next_check_at <= strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
               AND deadline_at > strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
               AND mcp_id NOT IN (SELECT value FROM json_each(?1))
         ) WHERE rank = 1 ORDER BY next_check_at, id LIMIT ?2",
    )
    .bind(Json(busy))
    .bind(i64::try_from(limit).unwrap_or(i64::MAX))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, mcp_id, tool, Json(arguments), status_pointer, Json(running_values))| Due {
                id,
                mcp_id,
                tool,
                arguments,
                status_pointer,
                running_values,
            },
        )
        .collect())
}

/// Record one check of a pending wait and settle it when the job left its
/// running values, failed for good, or ran out of time. Settling requeues
/// the waiting turn. A wait no longer pending is left as it is, so a check
/// that races a cancellation changes nothing. True when it settled.
pub async fn record(pool: &SqlitePool, id: Uuid, check: &Check) -> Result<bool, sqlx::Error> {
    let (result, finished, failed, permanent) = match check {
        Check::Running(result) => (result.as_str(), false, false, false),
        Check::Finished(result) => (result.as_str(), true, false, false),
        Check::Unreadable(message) => (message.as_str(), false, true, false),
        Check::Failed { message, permanent } => (message.as_str(), false, true, *permanent),
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let settled: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "UPDATE mcp_waits SET
             checks = checks + 1,
             failures = CASE WHEN ?3 THEN failures + 1 ELSE 0 END,
             last_result = ?2,
             status = CASE
                 WHEN ?4 THEN 'finished'
                 WHEN ?3 AND (?5 OR failures + 1 >= ?6) THEN 'failed'
                 WHEN deadline_at <= strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') THEN 'expired'
                 ELSE 'pending' END,
             next_check_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now',
                 '+' || interval_seconds || ' seconds')
         WHERE id = ?1 AND status = 'pending'
         RETURNING turn_id, bot_id, status",
    )
    .bind(id)
    .bind(bounded(result))
    .bind(failed)
    .bind(finished)
    .bind(permanent)
    .bind(MAX_FAILURES)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((turn_id, bot_id, status)) = settled else {
        return Ok(false);
    };
    if status == "pending" {
        tx.commit().await?;
        return Ok(false);
    }
    sqlx::query(
        "UPDATE mcp_waits SET settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    super::turns::continue_waiting(&mut tx, turn_id).await?;
    events::turn_changed(&mut tx, bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(true)
}

/// Settle every pending wait past its deadline without another status
/// call, and requeue its turn. Returns how many expired.
pub async fn expire(pool: &SqlitePool) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let expired: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE mcp_waits SET status = 'expired',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status = 'pending' AND deadline_at <= strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         RETURNING turn_id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for (turn_id, bot_id) in &expired {
        super::turns::continue_waiting(&mut tx, *turn_id).await?;
        events::turn_changed(&mut tx, *bot_id, *turn_id).await?;
    }
    tx.commit().await?;
    Ok(expired.len())
}

/// A wait still checking its job, as the owner's turn view shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, sqlx::FromRow)]
pub struct Pending {
    pub id: Uuid,
    #[serde(skip)]
    pub turn_id: Uuid,
    pub mcp_id: String,
}

/// The pending waits of these turns.
pub async fn pending_for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<Pending>>, sqlx::Error> {
    let rows: Vec<Pending> = sqlx::query_as(
        "SELECT id, turn_id, mcp_id FROM mcp_waits
         WHERE status = 'pending'
           AND lower(hex(turn_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1))
         ORDER BY created_at, id",
    )
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    let mut by_turn: std::collections::HashMap<Uuid, Vec<Pending>> = Default::default();
    for wait in rows {
        by_turn.entry(wait.turn_id).or_default().push(wait);
    }
    Ok(by_turn)
}

/// Settled waits of `turn_id` that no attempt has received yet.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    type Row = (Uuid, String, String, String, String, i64, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, mcp_id, tool, purpose, status, checks, last_result FROM mcp_waits
         WHERE turn_id = ?1 AND delivered_attempt IS NULL
           AND status IN ('finished', 'failed', 'expired')
         ORDER BY created_at, id",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(id, mcp_id, tool, purpose, status, checks, result)| Outcome {
                id,
                mcp_id,
                tool,
                purpose,
                status,
                checks,
                result,
            },
        )
        .collect())
}

/// Record that `attempt` received these results.
pub async fn mark_delivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
    ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE mcp_waits SET delivered_attempt = ?3
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
