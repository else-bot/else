//! A bot's note that a task delegated to it ends needing the owner.
//!
//! The bot calls `notify_owner` while it runs the task. The notice shows as
//! a card in its own chat with the owner, beside the task, so the owner sees
//! what is needed without opening the task. It never holds the turn and
//! outlives it. `pending` lasts until the owner messages the bot (`replied`)
//! or marks it handled (`dismissed`), or until the attempt ends with a card
//! for the owner pending on the turn (`superseded`), which shows instead. A
//! turn has at most one pending notice: flagging again rewrites it, and
//! repeating a notice the turn already has returns it unchanged.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;

pub const MAX_SUMMARY: usize = 300;
pub const MAX_TRIED: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Something only the owner can clear stops the work.
    Blocked,
    /// The work waits on a choice only the owner can make.
    Decision,
}

impl Kind {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "blocked" => Some(Self::Blocked),
            "decision" => Some(Self::Decision),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::Decision => "decision",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    /// The owner messaged the bot after the notice.
    Replied,
    /// The owner marked it handled.
    Dismissed,
    /// A card for the owner on the same turn replaced it.
    Superseded,
}

impl Status {
    fn parse(text: &str) -> Self {
        match text {
            "replied" => Self::Replied,
            "dismissed" => Self::Dismissed,
            "superseded" => Self::Superseded,
            _ => Self::Pending,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Replied => "replied",
            Self::Dismissed => "dismissed",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Notice {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub turn_id: Uuid,
    pub kind: Kind,
    pub summary: String,
    pub tried: Option<String>,
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
    Option<String>,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

const COLUMNS: &str = "id, bot_id, turn_id, kind, summary, tried, status, created_at, settled_at";

fn notice(
    (id, bot_id, turn_id, kind, summary, tried, status, created_at, settled_at): Row,
) -> Notice {
    Notice {
        id,
        bot_id,
        turn_id,
        kind: Kind::parse(&kind).unwrap_or(Kind::Blocked),
        summary,
        tried,
        status: Status::parse(&status),
        created_at,
        settled_at,
    }
}

/// What the bot asks the owner for, parsed from `notify_owner` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub kind: Kind,
    pub summary: String,
    pub tried: Option<String>,
}

impl Spec {
    pub fn parse(args: &Value) -> Result<Self, &'static str> {
        let kind = args["kind"]
            .as_str()
            .and_then(Kind::parse)
            .ok_or("kind is blocked or decision")?;
        let text = |raw: Option<&str>, max: usize| {
            raw.map(str::trim)
                .filter(|t| !t.is_empty() && t.len() <= max && !t.contains('\0'))
                .map(str::to_owned)
        };
        let summary = text(args["summary"].as_str(), MAX_SUMMARY)
            .ok_or("summary is 1 to 300 bytes of text")?;
        let tried = match &args["tried"] {
            Value::Null => None,
            raw => Some(text(raw.as_str(), MAX_TRIED).ok_or("tried is 1 to 200 bytes of text")?),
        };
        Ok(Self {
            kind,
            summary,
            tried,
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CreateError {
    #[error(
        "only a task another bot delegated to you can notify the owner; reply to the owner directly"
    )]
    NotDelegated,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("this attempt already waits on the owner through a card, which the owner sees")]
    AttemptParked,
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for CreateError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "owner notice failed");
        Self::Db
    }
}

/// Record the notice of the running attempt of `turn_id`, a delegated task,
/// or rewrite the pending one it already has.
pub async fn create(pool: &SqlitePool, turn_id: Uuid, spec: &Spec) -> Result<Notice, CreateError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = super::turns::asking_caller(&mut tx, turn_id)
        .await?
        .ok_or(CreateError::NotRunning)?;
    let delegated: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM delegations WHERE child_turn_id = ?1)")
            .bind(turn_id)
            .fetch_one(&mut *tx)
            .await?;
    if caller.owner_conversation || !delegated {
        return Err(CreateError::NotDelegated);
    }
    if super::turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
        return Err(CreateError::AttemptParked);
    }
    let repeated: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM owner_notices
         WHERE turn_id = ?1 AND kind = ?2 AND summary = ?3 AND tried IS ?4
         ORDER BY created_at DESC, id DESC LIMIT 1"
    )))
    .bind(turn_id)
    .bind(spec.kind.as_str())
    .bind(&spec.summary)
    .bind(&spec.tried)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = repeated {
        return Ok(notice(row));
    }
    let rewritten: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE owner_notices SET kind = ?2, summary = ?3, tried = ?4
         WHERE turn_id = ?1 AND status = 'pending' RETURNING {COLUMNS}"
    )))
    .bind(turn_id)
    .bind(spec.kind.as_str())
    .bind(&spec.summary)
    .bind(&spec.tried)
    .fetch_optional(&mut *tx)
    .await?;
    let row = match rewritten {
        Some(row) => row,
        None => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "INSERT INTO owner_notices (id, bot_id, turn_id, kind, summary, tried)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING {COLUMNS}"
            )))
            .bind(Uuid::new_v4())
            .bind(caller.bot_id)
            .bind(turn_id)
            .bind(spec.kind.as_str())
            .bind(&spec.summary)
            .bind(&spec.tried)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(notice(row))
}

/// Mark the bot's pending notices replied, when the owner messages it.
pub async fn reply_pending(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<(), sqlx::Error> {
    let replied: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE owner_notices SET status = 'replied',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND status = 'pending' RETURNING turn_id",
    )
    .bind(bot_id)
    .fetch_all(&mut *conn)
    .await?;
    for turn_id in replied {
        events::turn_changed(conn, bot_id, turn_id).await?;
    }
    Ok(())
}

/// Supersede the turn's pending notice when the attempt ending now left a
/// card for the owner pending on the turn: the card asks instead.
pub async fn supersede_by_card(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE owner_notices SET status = 'superseded',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE turn_id = ?1 AND status = 'pending'
           AND (EXISTS (SELECT 1 FROM owner_questions WHERE turn_id = ?1 AND status = 'pending')
                OR EXISTS (SELECT 1 FROM secret_requests WHERE turn_id = ?1 AND status = 'pending')
                OR EXISTS (SELECT 1 FROM outgoing_drafts WHERE turn_id = ?1 AND status = 'pending')
                OR EXISTS (SELECT 1 FROM social_setups WHERE turn_id = ?1 AND status = 'pending')
                OR EXISTS (SELECT 1 FROM skill_installs WHERE turn_id = ?1 AND status = 'pending')
                OR EXISTS (SELECT 1 FROM computer_handoffs
                           WHERE turn_id = ?1 AND status IN ('pending', 'taken')))",
    )
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Every notice of these turns, oldest first per turn.
pub async fn for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Notice>>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM owner_notices
         WHERE lower(hex(turn_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1))
         ORDER BY created_at, id"
    )))
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    let mut by_turn: HashMap<Uuid, Vec<Notice>> = HashMap::new();
    for row in rows {
        let n = notice(row);
        by_turn.entry(n.turn_id).or_default().push(n);
    }
    Ok(by_turn)
}

#[derive(Debug, thiserror::Error)]
pub enum DismissError {
    #[error("not found")]
    NotFound,
    #[error("the notice is already {}", .0.as_str())]
    Settled(Status),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Mark a pending notice handled. Dismissing it again returns it
/// unchanged; a notice the owner already replied to is refused.
pub async fn dismiss(
    pool: &SqlitePool,
    bot_id: Uuid,
    notice_id: Uuid,
) -> Result<Notice, DismissError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let found: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM owner_notices WHERE id = ?1 AND bot_id = ?2"
    )))
    .bind(notice_id)
    .bind(bot_id)
    .fetch_optional(&mut *tx)
    .await?;
    let found = found.map(notice).ok_or(DismissError::NotFound)?;
    match found.status {
        Status::Dismissed => return Ok(found),
        status @ (Status::Replied | Status::Superseded) => {
            return Err(DismissError::Settled(status));
        }
        Status::Pending => {}
    }
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE owner_notices SET status = 'dismissed',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'pending' RETURNING {COLUMNS}"
    )))
    .bind(notice_id)
    .fetch_optional(&mut *tx)
    .await?;
    let dismissed = notice(row.ok_or(DismissError::Settled(Status::Replied))?);
    events::turn_changed(&mut tx, bot_id, found.turn_id).await?;
    tx.commit().await?;
    Ok(dismissed)
}
