//! What a task delegated to a bot needs from the owner.
//!
//! The bot calls `notify_owner` before its final message to declare a
//! [`Need`], stored on the delegation. `nothing` and `fyi` stay there. A
//! `decision` or `blocked` ask is also a notice: a card in the bot's own chat
//! with the owner, beside the task, and inside the exchange row of the chat
//! that delegated it, so the owner sees what is needed without opening the
//! task. A notice never holds the turn and outlives it. `pending` lasts until the owner messages the bot (`replied`)
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

use super::{events, in_ids};

pub const MAX_SUMMARY: usize = 300;
pub const MAX_TRIED: usize = 200;
pub const MAX_LINE: usize = 200;

/// What a delegated task declared it needs from the owner, stored as
/// `delegations.owner_need`. NULL there means it declared nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Need {
    Nothing,
    /// Done, with one line the owner may want to know.
    Fyi,
    Decision,
    Blocked,
}

impl Need {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

impl From<Kind> for Need {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Blocked => Self::Blocked,
            Kind::Decision => Self::Decision,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Kind {
    /// Something only the owner can clear stops the work.
    Blocked,
    /// The work waits on a choice only the owner can make.
    Decision,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr, strum::EnumString)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
    pub fn as_str(self) -> &'static str {
        self.into()
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
        kind: kind.parse().unwrap_or(Kind::Blocked),
        summary,
        tried,
        status: status.parse().unwrap_or(Status::Pending),
        created_at,
        settled_at,
    }
}

/// What the bot asks the owner for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub kind: Kind,
    pub summary: String,
    pub tried: Option<String>,
}

/// A task's declaration, parsed from `notify_owner` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declaration {
    Nothing,
    Fyi { line: String },
    Ask(Spec),
}

impl Declaration {
    pub fn parse(args: &Value) -> Result<Self, &'static str> {
        let need: Need = args["kind"]
            .as_str()
            .and_then(|k| k.parse().ok())
            .ok_or("kind is nothing, fyi, decision, or blocked")?;
        let text = |raw: Option<&str>, max: usize| {
            raw.map(str::trim)
                .filter(|t| !t.is_empty() && t.len() <= max && !t.contains('\0'))
                .map(str::to_owned)
        };
        let ask = |kind| -> Result<Self, &'static str> {
            let summary = text(args["summary"].as_str(), MAX_SUMMARY)
                .ok_or("summary is 1 to 300 bytes of text")?;
            let tried = match &args["tried"] {
                Value::Null => None,
                raw => {
                    Some(text(raw.as_str(), MAX_TRIED).ok_or("tried is 1 to 200 bytes of text")?)
                }
            };
            Ok(Self::Ask(Spec {
                kind,
                summary,
                tried,
            }))
        };
        match need {
            Need::Nothing => Ok(Self::Nothing),
            Need::Fyi => Ok(Self::Fyi {
                line: text(args["summary"].as_str(), MAX_LINE)
                    .ok_or("an fyi summary is 1 to 200 bytes of text")?,
            }),
            Need::Decision => ask(Kind::Decision),
            Need::Blocked => ask(Kind::Blocked),
        }
    }

    fn need(&self) -> Need {
        match self {
            Self::Nothing => Need::Nothing,
            Self::Fyi { .. } => Need::Fyi,
            Self::Ask(spec) => spec.kind.into(),
        }
    }
}

/// What a declaration recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Declared {
    Quiet(Need),
    Ask(Notice),
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
    #[error("the owner has not settled your pending ask, so this task still needs them")]
    AskPending,
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for CreateError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "owner notice failed");
        Self::Db
    }
}

/// Record what the running attempt of `turn_id`, a delegated task, needs
/// from the owner. An ask records its notice, or rewrites the pending one
/// the turn already has. `nothing` or `fyi` cannot replace a pending ask.
pub async fn declare(
    pool: &SqlitePool,
    turn_id: Uuid,
    declaration: &Declaration,
) -> Result<Declared, CreateError> {
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
    let need = declaration.need();
    let declared = match declaration {
        Declaration::Ask(spec) => {
            Declared::Ask(record_ask(&mut tx, turn_id, caller.bot_id, spec).await?)
        }
        Declaration::Nothing | Declaration::Fyi { .. } => {
            // Any attempt of this task: an update leaves the earlier
            // attempt's ask pending in the same task conversation.
            let pending: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM owner_notices WHERE status = 'pending'
                     AND turn_id IN (SELECT id FROM turns WHERE conversation_id =
                                         (SELECT conversation_id FROM turns WHERE id = ?1)))",
            )
            .bind(turn_id)
            .fetch_one(&mut *tx)
            .await?;
            if pending {
                return Err(CreateError::AskPending);
            }
            Declared::Quiet(need)
        }
    };
    let line = match declaration {
        Declaration::Fyi { line } => Some(line.as_str()),
        _ => None,
    };
    let stored: Option<String> = sqlx::query_scalar(
        "UPDATE delegations SET owner_need = ?2, owner_line = ?3
         WHERE child_turn_id = ?1 AND status IN ('queued', 'running') RETURNING owner_need",
    )
    .bind(turn_id)
    .bind(need.as_str())
    .bind(line)
    .fetch_optional(&mut *tx)
    .await?;
    if stored.as_deref() != Some(need.as_str()) {
        return Err(CreateError::NotRunning);
    }
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    events::exchange_changed(&mut tx, turn_id).await?;
    tx.commit().await?;
    Ok(declared)
}

/// The turn's notice for `spec`: a repeat returns the notice it matches,
/// otherwise it rewrites the pending one or adds one.
async fn record_ask(
    tx: &mut SqliteConnection,
    turn_id: Uuid,
    bot_id: Uuid,
    spec: &Spec,
) -> Result<Notice, CreateError> {
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
            .bind(bot_id)
            .bind(turn_id)
            .bind(spec.kind.as_str())
            .bind(&spec.summary)
            .bind(&spec.tried)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    Ok(notice(row))
}

/// What an owner message to a bot answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// The ask the owner replied to from its card.
    Notice(Uuid),
    /// A message without a target answers the bot's ask only when exactly
    /// one is pending; with several, it cannot tell which, so all stay.
    Untargeted,
}

/// Mark the ask the owner's message answers replied. A target already
/// settled stays as it is: the message still goes to the bot.
pub async fn answer(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    answer: Answer,
) -> Result<(), AnswerError> {
    let replied: Vec<Uuid> = match answer {
        Answer::Notice(id) => {
            let owner: Option<Uuid> =
                sqlx::query_scalar("SELECT bot_id FROM owner_notices WHERE id = ?1")
                    .bind(id)
                    .fetch_optional(&mut *conn)
                    .await?;
            if owner != Some(bot_id) {
                return Err(AnswerError::NotThisBots);
            }
            sqlx::query_scalar(
                "UPDATE owner_notices SET status = 'replied',
                     settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1 AND status = 'pending' RETURNING turn_id",
            )
            .bind(id)
            .fetch_all(&mut *conn)
            .await?
        }
        Answer::Untargeted => {
            sqlx::query_scalar(
                "UPDATE owner_notices SET status = 'replied',
                     settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE bot_id = ?1 AND status = 'pending'
                   AND (SELECT count(*) FROM owner_notices
                        WHERE bot_id = ?1 AND status = 'pending') = 1
                 RETURNING turn_id",
            )
            .bind(bot_id)
            .fetch_all(&mut *conn)
            .await?
        }
    };
    for turn_id in replied {
        events::turn_changed(conn, bot_id, turn_id).await?;
        events::exchange_changed(conn, turn_id).await?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum AnswerError {
    #[error("the answered notice is not one of this bot's")]
    NotThisBots,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
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
         WHERE {}
         ORDER BY created_at, id",
        in_ids("turn_id", "?1")
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
    events::exchange_changed(&mut tx, found.turn_id).await?;
    tx.commit().await?;
    Ok(dismissed)
}
