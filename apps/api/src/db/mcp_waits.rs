//! A bot's wait on a long-running job in an installed MCP (`mcp_wait`).
//!
//! The attempt names a read-only status tool and the status values that
//! mean the job still runs. It then ends and its turn waits with no
//! provider process. The watcher repeats the status call on the API host
//! and settles the wait once the status leaves those values, the checks
//! keep failing or report no status, or the deadline passes; settling
//! requeues the turn as a continuation that receives the last result.
//! A new owner message joins the waiting turn and keeps the wait on it;
//! the owner may stop it ([`stop_by_owner`]), which the bot receives like
//! any other result; any other end of the turn withdraws a pending wait (a
//! trigger), except the owner's Stop of the attempt alone.

use serde_json::Value;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{events, in_ids};

pub use crate::app::providers::McpWaitOutcome as Outcome;

pub const MIN_INTERVAL_SECONDS: u32 = 15;
pub const MAX_INTERVAL_SECONDS: u32 = 3600;
pub const MAX_TIMEOUT_MINUTES: u32 = 1440;
/// Consecutive failed checks that end a wait.
pub const MAX_FAILURES: i64 = 3;
/// Bytes of the last status result kept for the continuation.
pub const MAX_RESULT: usize = 16 * 1024;

/// A wait's lifecycle, as stored in `mcp_waits.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, strum::IntoStaticStr)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Status {
    Pending,
    Finished,
    Failed,
    Expired,
    /// Withdrawn with its turn; never delivered.
    Cancelled,
    /// The owner stopped it; delivered like a result.
    Stopped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

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

/// What [`create`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    Waiting(Uuid),
    /// A pending wait from an earlier attempt of the turn already checks
    /// the same status; the turn keeps it after this attempt ends. This
    /// attempt holds nothing, so it is not parked.
    Carried(Uuid),
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
/// attempt returns its id, and a pending wait on the same status from an
/// earlier attempt returns [`Created::Carried`].
pub async fn create(
    pool: &SqlitePool,
    turn_id: Uuid,
    attempt: i32,
    spec: &Spec,
    first: &str,
) -> Result<Created, CreateError> {
    type Row = (Uuid, i32, String, String, Json<Value>, String);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = super::turns::asking_caller(&mut tx, turn_id)
        .await?
        .filter(|caller| caller.attempt == attempt)
        .ok_or(CreateError::NotRunning)?;
    let waits: Vec<Row> = sqlx::query_as(
        "SELECT id, attempt, mcp_id, tool, arguments, status_pointer FROM mcp_waits
         WHERE turn_id = ?1 AND (attempt = ?2 OR status = 'pending')",
    )
    .bind(turn_id)
    .bind(caller.attempt)
    .fetch_all(&mut *tx)
    .await?;
    let same = |(_, _, mcp_id, tool, Json(arguments), pointer): &&Row| {
        *mcp_id == spec.mcp_id
            && *tool == spec.tool
            && *arguments == spec.arguments
            && *pointer == spec.status_pointer
    };
    if let Some(own) = waits.iter().find(|wait| wait.1 == caller.attempt) {
        return if same(&own) {
            Ok(Created::Waiting(own.0))
        } else {
            Err(CreateError::AttemptParked)
        };
    }
    if super::turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
        return Err(CreateError::AttemptParked);
    }
    if let Some(carried) = waits.iter().find(same) {
        return Ok(Created::Carried(carried.0));
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
    Ok(Created::Waiting(id))
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
    let settled: Option<(Uuid, Uuid, Status)> = sqlx::query_as(
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
    if status == Status::Pending {
        // The owner's row shows each check's status as it lands.
        events::turn_changed(&mut tx, bot_id, turn_id).await?;
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
    super::turns::lifecycle::continue_waiting(&mut tx, turn_id).await?;
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
        super::turns::lifecycle::continue_waiting(&mut tx, *turn_id).await?;
        events::turn_changed(&mut tx, *bot_id, *turn_id).await?;
    }
    tx.commit().await?;
    Ok(expired.len())
}

/// The pending waits of `turn_id`, oldest first, for the bot's context.
pub async fn in_progress(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<InProgress>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, mcp_id, tool, purpose, checks FROM mcp_waits
         WHERE turn_id = ?1 AND status = 'pending'
         ORDER BY created_at, id",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await
}

pub use crate::app::providers::McpWaitInProgress as InProgress;

/// Bytes of the last result the owner's turn view shows.
const SHOWN_RESULT: usize = 2048;

/// A wait still checking its job, as the owner's turn view shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub id: Uuid,
    pub turn_id: Uuid,
    pub mcp_id: String,
    pub tool: String,
    pub purpose: String,
    pub interval_seconds: i64,
    pub deadline_at: String,
    pub checks: i64,
    /// The job's status value in the last result, when it is readable.
    pub last_status: Option<String>,
    /// The start of the last status result.
    pub last_result: Option<String>,
    pub created_at: String,
}

impl From<&Pending> for else_wire::waits::McpWait {
    fn from(row: &Pending) -> Self {
        Self {
            id: row.id,
            mcp_id: row.mcp_id.clone(),
            tool: row.tool.clone(),
            purpose: row.purpose.clone(),
            interval_seconds: row.interval_seconds,
            deadline_at: Some(row.deadline_at.clone()),
            checks: row.checks,
            last_status: row.last_status.clone(),
            last_result: row.last_result.clone(),
            created_at: Some(row.created_at.clone()),
        }
    }
}

/// The status a tool result reports at `pointer`: from structuredContent,
/// or else from the JSON in its first text block that parses.
pub fn status_at(result: &Value, pointer: &str) -> Option<Value> {
    if let Some(found) = result
        .get("structuredContent")
        .and_then(|d| d.pointer(pointer))
    {
        return Some(found.clone());
    }
    result["content"].as_array()?.iter().find_map(|block| {
        let text = block["text"].as_str().filter(|_| block["type"] == "text")?;
        serde_json::from_str::<Value>(text)
            .ok()?
            .pointer(pointer)
            .cloned()
    })
}

/// The status a stored result reports at `pointer`, as short text.
fn shown_status(result: &str, pointer: &str) -> Option<String> {
    let result: Value = serde_json::from_str(result).ok()?;
    let text = match status_at(&result, pointer)? {
        Value::String(text) => text,
        Value::Null | Value::Array(_) | Value::Object(_) => return None,
        other => other.to_string(),
    };
    Some(text.chars().take(64).collect())
}

/// The pending waits of these turns.
pub async fn pending_for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<Pending>>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Row {
        id: Uuid,
        turn_id: Uuid,
        mcp_id: String,
        tool: String,
        purpose: String,
        interval_seconds: i64,
        deadline_at: String,
        checks: i64,
        status_pointer: String,
        last_result: Option<String>,
        created_at: String,
    }
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, turn_id, mcp_id, tool, purpose, interval_seconds, deadline_at, checks,
             status_pointer, last_result, created_at
         FROM mcp_waits
         WHERE status = 'pending'
           AND {}
         ORDER BY created_at, id",
        in_ids("turn_id", "?1")
    )))
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    let pending = rows.into_iter().map(|row| {
        let last_status = row
            .last_result
            .as_deref()
            .and_then(|result| shown_status(result, &row.status_pointer));
        Pending {
            id: row.id,
            turn_id: row.turn_id,
            mcp_id: row.mcp_id,
            tool: row.tool,
            purpose: row.purpose,
            interval_seconds: row.interval_seconds,
            deadline_at: row.deadline_at,
            checks: row.checks,
            last_status,
            last_result: row
                .last_result
                .map(|result| result[..result.floor_char_boundary(SHOWN_RESULT)].to_owned()),
            created_at: row.created_at,
        }
    });
    Ok(super::holds::by_turn(pending, |wait| wait.turn_id))
}

/// The owner stops wait `id` of `bot_id`. A pending wait becomes `stopped`,
/// an outcome its turn receives, and the turn continues if nothing else
/// holds it. `None` when the bot has no such wait.
pub async fn stop_by_owner(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    id: Uuid,
) -> Result<Option<super::waits::Stop>, sqlx::Error> {
    use super::waits::Stop;
    let found: Option<(Status, Uuid)> =
        sqlx::query_as("SELECT status, turn_id FROM mcp_waits WHERE id = ?1 AND bot_id = ?2")
            .bind(id)
            .bind(bot_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((status, turn_id)) = found else {
        return Ok(None);
    };
    Ok(Some(match status {
        Status::Stopped => Stop::Repeated { turn_id },
        Status::Pending => {
            sqlx::query(
                "UPDATE mcp_waits SET status = 'stopped',
                     settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1 AND status = 'pending'",
            )
            .bind(id)
            .execute(&mut *conn)
            .await?;
            Stop::Stopped { turn_id }
        }
        Status::Finished | Status::Failed | Status::Expired | Status::Cancelled => Stop::Ended,
    }))
}

/// Settled waits of `turn_id` that no attempt has received yet.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    type Row = (Uuid, String, String, String, Status, i64, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, mcp_id, tool, purpose, status, checks, last_result FROM mcp_waits
         WHERE turn_id = ?1 AND delivered_attempt IS NULL
           AND status IN ('finished', 'failed', 'expired', 'stopped')
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

/// Withdraw the pending waits of `turn_id`, whose turn ends without a
/// status change that the trigger would see.
pub async fn withdraw_for_turn(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE mcp_waits SET status = 'cancelled',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE turn_id = ?1 AND status = 'pending'",
    )
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
