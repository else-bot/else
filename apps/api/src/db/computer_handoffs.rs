//! A bot's request that the owner take over its computer.
//!
//! The bot calls `computer_handoff` and ends its reply; nothing waits on the
//! request. It stays pending, one per bot, until the owner takes over or
//! dismisses it, the owner sends the bot a message, or the bot asks again.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;

pub const MAX_REASON: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Taken,
    Dismissed,
    Skipped,
}

impl Status {
    fn parse(text: &str) -> Self {
        match text {
            "taken" => Self::Taken,
            "dismissed" => Self::Dismissed,
            "skipped" => Self::Skipped,
            _ => Self::Pending,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Taken => "taken",
            Self::Dismissed => "dismissed",
            Self::Skipped => "skipped",
        }
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
);

const COLUMNS: &str = "id, bot_id, turn_id, reason, status, created_at, settled_at";

fn handoff((id, bot_id, turn_id, reason, status, created_at, settled_at): Row) -> Handoff {
    Handoff {
        id,
        bot_id,
        turn_id,
        reason,
        status: Status::parse(&status),
        created_at,
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
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for CreateError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "computer handoff failed");
        Self::Db
    }
}

/// Record a request from the running turn `turn_id`. It replaces the bot's
/// earlier pending request, so the owner sees only the latest.
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
    skip_pending(&mut tx, caller.bot_id).await?;
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO computer_handoffs (id, bot_id, turn_id, reason)
         VALUES (?1, ?2, ?3, ?4) RETURNING {COLUMNS}"
    )))
    .bind(Uuid::new_v4())
    .bind(caller.bot_id)
    .bind(turn_id)
    .bind(reason)
    .fetch_one(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(handoff(row))
}

/// Withdraw the bot's pending request, when the owner messages it or it
/// asks again.
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

#[derive(Debug, thiserror::Error)]
pub enum SettleError {
    #[error("not found")]
    NotFound,
    #[error("the request was already settled")]
    Settled(Handoff),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The owner takes over or dismisses a pending request. Settling it again
/// the same way returns it unchanged.
pub async fn settle(
    pool: &SqlitePool,
    bot_id: Uuid,
    handoff_id: Uuid,
    status: Status,
) -> Result<Handoff, SettleError> {
    debug_assert!(matches!(status, Status::Taken | Status::Dismissed));
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
    if found.status == status {
        return Ok(found);
    }
    if found.status != Status::Pending {
        return Err(SettleError::Settled(found));
    }
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE computer_handoffs SET status = ?2,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 RETURNING {COLUMNS}"
    )))
    .bind(handoff_id)
    .bind(status.as_str())
    .fetch_one(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, bot_id, found.turn_id).await?;
    tx.commit().await?;
    Ok(handoff(row))
}
