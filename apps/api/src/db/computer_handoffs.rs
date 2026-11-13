//! A bot's request that the owner take over its computer.
//!
//! The bot calls `computer_handoff` and ends its attempt; the turn waits on
//! the request like a question. One request per bot is open at a time.
//! `pending` waits for the owner, who takes over (`taken`) or declines
//! (`dismissed`). Taking over only hands them control: the turn continues
//! once they press Hand back (`handed_back`). A newer owner message or a
//! newer request replaces a pending one (`skipped`), and the end of its
//! turn withdraws it (`cancelled`, by trigger). A taken request outlives its
//! turn: the owner holds the computer until they hand it back, and the
//! bot's later turns are told so.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;

pub const MAX_REASON: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Status {
    Pending,
    Taken,
    HandedBack,
    Dismissed,
    Skipped,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Handoff {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub turn_id: Uuid,
    pub reason: String,
    pub status: Status,
    pub created_at: DateTime<Utc>,
    pub taken_at: Option<DateTime<Utc>>,
    pub settled_at: Option<DateTime<Utc>>,
}

type Row = (
    Uuid,
    Uuid,
    Uuid,
    String,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

const COLUMNS: &str = "id, bot_id, turn_id, reason, status, created_at, taken_at, settled_at";

fn handoff(
    (id, bot_id, turn_id, reason, status, created_at, taken_at, settled_at): Row,
) -> Handoff {
    Handoff {
        id,
        bot_id,
        turn_id,
        reason,
        status: status.parse().unwrap_or(Status::Pending),
        created_at,
        taken_at,
        settled_at,
    }
}

/// The reason shown to the owner: trimmed, 1 to 200 bytes of text.
pub fn parse_reason(raw: Option<&str>) -> Result<String, &'static str> {
    raw.map(str::trim)
        .filter(|r| !r.is_empty() && r.len() <= MAX_REASON && !r.contains('\0'))
        .map(str::to_owned)
        .ok_or("reason is 1 to 200 bytes of text")
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CreateError {
    #[error(
        "only the owner's own conversation or a task delegated for an owner request can ask the owner"
    )]
    NotAllowed,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("this attempt already waits on the owner and must end")]
    AttemptParked,
    #[error("the owner already controls your computer for an earlier request")]
    OwnerInControl,
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for CreateError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "computer handoff failed");
        Self::Db
    }
}

/// Record a request from the running attempt of `turn_id`, which then
/// waits on it. It replaces the bot's earlier pending request, so the owner
/// sees only the latest; a request the owner already took over stays.
pub async fn create(
    pool: &SqlitePool,
    turn_id: Uuid,
    reason: &str,
) -> Result<Handoff, CreateError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = super::turns::asking_caller(&mut tx, turn_id)
        .await?
        .ok_or(CreateError::NotRunning)?;
    if !super::turns::may_ask_owner(&mut tx, turn_id, &caller).await? {
        return Err(CreateError::NotAllowed);
    }
    if super::turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
        return Err(CreateError::AttemptParked);
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM computer_handoffs WHERE bot_id = ?1 AND status = 'taken')",
    )
    .bind(caller.bot_id)
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(CreateError::OwnerInControl);
    }
    skip_pending(&mut tx, caller.bot_id).await?;
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO computer_handoffs (id, bot_id, turn_id, attempt, reason)
         VALUES (?1, ?2, ?3, ?4, ?5) RETURNING {COLUMNS}"
    )))
    .bind(Uuid::new_v4())
    .bind(caller.bot_id)
    .bind(turn_id)
    .bind(caller.attempt)
    .bind(reason)
    .fetch_one(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(handoff(row))
}

/// Withdraw the bot's pending request, when the owner messages it or it
/// asks again. The turn that waits on it continues and learns it was
/// skipped. A request the owner already took over stays theirs.
pub async fn skip_pending(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<(), sqlx::Error> {
    let skipped: Option<Uuid> = sqlx::query_scalar(
        "UPDATE computer_handoffs SET status = 'skipped',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND status = 'pending' RETURNING turn_id",
    )
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(turn_id) = skipped {
        super::turns::continue_waiting(conn, turn_id).await?;
        events::turn_changed(conn, bot_id, turn_id).await?;
    }
    Ok(())
}

/// Every request of these turns, oldest first per turn.
pub async fn for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Handoff>>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM computer_handoffs
         WHERE lower(hex(turn_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1))
         ORDER BY created_at, id"
    )))
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    let mut by_turn: HashMap<Uuid, Vec<Handoff>> = HashMap::new();
    for row in rows {
        let h = handoff(row);
        by_turn.entry(h.turn_id).or_default().push(h);
    }
    Ok(by_turn)
}

/// What the owner did with a request, for the attempt that continues the
/// turn that asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub id: Uuid,
    pub reason: String,
    pub status: Status,
}

/// Settled requests of a turn not yet given to an attempt.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT id, reason, status FROM computer_handoffs
         WHERE turn_id = ?1 AND attempt > 0 AND delivered_attempt IS NULL
           AND status IN ('handed_back', 'dismissed', 'skipped')
         ORDER BY created_at, id",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, reason, status)| Outcome {
            id,
            reason,
            status: status.parse().unwrap_or(Status::Pending),
        })
        .collect())
}

/// Record that `attempt` received these outcomes.
pub async fn mark_delivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
    ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE computer_handoffs SET delivered_attempt = ?3
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

/// The bot's latest request the owner took over, for the context every
/// turn of the bot carries: whether the owner still holds its computer.
pub async fn latest_taken(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<Option<Handoff>, sqlx::Error> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM computer_handoffs
         WHERE bot_id = ?1 AND taken_at IS NOT NULL
         ORDER BY taken_at DESC, id DESC LIMIT 1"
    )))
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(handoff))
}

/// What the owner does with a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Take control of the computer; the turn keeps waiting.
    Take,
    /// Return control; the turn continues.
    HandBack,
    /// Decline a request not taken over; the turn continues.
    Dismiss,
}

impl Action {
    fn transition(self) -> (Status, &'static [Status]) {
        match self {
            Self::Take => (Status::Taken, &[Status::Pending]),
            Self::HandBack => (Status::HandedBack, &[Status::Taken]),
            Self::Dismiss => (Status::Dismissed, &[Status::Pending]),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettleError {
    #[error("not found")]
    NotFound,
    #[error("the request is already {}", .0.status.as_str())]
    Settled(Handoff),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Apply the owner's `action` to a request. Repeating an action that
/// already applied returns the request unchanged; any other action from a
/// state it does not start from is refused.
pub async fn settle(
    pool: &SqlitePool,
    bot_id: Uuid,
    handoff_id: Uuid,
    action: Action,
) -> Result<Handoff, SettleError> {
    let (target, sources) = action.transition();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT h.{} FROM computer_handoffs h JOIN bots b ON b.id = h.bot_id
         WHERE h.id = ?1 AND h.bot_id = ?2 AND b.status = 'active'",
        COLUMNS.replace(", ", ", h.")
    )))
    .bind(handoff_id)
    .bind(bot_id)
    .fetch_optional(&mut *tx)
    .await?;
    let found = row.map(handoff).ok_or(SettleError::NotFound)?;
    if found.status == target {
        return Ok(found);
    }
    if !sources.contains(&found.status) {
        return Err(SettleError::Settled(found));
    }
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE computer_handoffs SET status = ?2,
             taken_at = CASE WHEN ?2 = 'taken'
                 THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') ELSE taken_at END,
             settled_at = CASE WHEN ?2 = 'taken'
                 THEN NULL ELSE strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END
         WHERE id = ?1 AND status = ?3 RETURNING {COLUMNS}"
    )))
    .bind(handoff_id)
    .bind(target.as_str())
    .bind(found.status.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let settled = handoff(row.ok_or(SettleError::Settled(found.clone()))?);
    if target != Status::Taken {
        super::turns::continue_waiting(&mut tx, found.turn_id).await?;
    }
    events::turn_changed(&mut tx, bot_id, found.turn_id).await?;
    tx.commit().await?;
    Ok(settled)
}
