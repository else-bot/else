//! What became of the approved tool calls the gate let run: each consumed
//! card stamps `used_at`, and the call's result sets `use_outcome`.
//!
//! The dispatcher wraps every tool call in [`track_use`] and records the
//! outcome once the call returns. The gate reports the card it consumed
//! through a task-local rather than through each tool's return value, so the
//! eight gated tools and their early returns cannot forget to: a call outside
//! the scope keeps a started use, whose outcome is unknown.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::QuestionInfo;
use crate::app::questions::Action;
use crate::db::events;
use crate::db::turns;

/// An approved tool call the gate let run, from `used_at` and `use_outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "lowercase")]
pub enum Use {
    /// No result was recorded: the call is still running, or its result was
    /// lost to a crash or to a call the gate could not trace.
    Started {
        at: DateTime<Utc>,
    },
    Completed {
        at: DateTime<Utc>,
    },
    /// The tool returned an error or parked; the effect may not have happened.
    Error {
        at: DateTime<Utc>,
    },
}

/// `owner_questions.use_outcome`, set once the call's tool returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum UseOutcome {
    Completed,
    Error,
}

impl Use {
    pub(super) fn decode(
        at: Option<DateTime<Utc>>,
        outcome: Option<UseOutcome>,
    ) -> Result<Option<Self>, sqlx::Error> {
        Ok(match (at, outcome) {
            (None, None) => None,
            (Some(at), None) => Some(Self::Started { at }),
            (Some(at), Some(UseOutcome::Completed)) => Some(Self::Completed { at }),
            (Some(at), Some(UseOutcome::Error)) => Some(Self::Error { at }),
            (None, Some(_)) => {
                return Err(sqlx::Error::Decode(
                    "an approval outcome without its use time".into(),
                ));
            }
        })
    }

    pub fn at(self) -> DateTime<Utc> {
        match self {
            Self::Started { at } | Self::Completed { at } | Self::Error { at } => at,
        }
    }

    /// How the call ended, for a bot: "completed at 2026-11-13T09:00:00Z".
    pub fn describe(self) -> String {
        let at = self.at().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        match self {
            Self::Completed { .. } => format!("completed at {at}"),
            Self::Error { .. } => format!("returned an error at {at}"),
            Self::Started { .. } => format!("started at {at}, outcome unknown"),
        }
    }
}

tokio::task_local! {
    /// The approval that the tool call inside [`track_use`] consumed.
    static CONSUMED: std::cell::Cell<Option<Uuid>>;
}

/// Runs one tool call and returns the approval the gate consumed for it, so
/// the caller can record the call's outcome on that card.
pub async fn track_use<T>(call: impl Future<Output = T>) -> (T, Option<Uuid>) {
    CONSUMED
        .scope(std::cell::Cell::new(None), async {
            let out = call.await;
            (out, CONSUMED.with(std::cell::Cell::get))
        })
        .await
}

/// Records what the consumed approval's call returned. Only a started use
/// takes an outcome; returns false when the transition was rejected.
pub async fn record_use(
    pool: &SqlitePool,
    question_id: Uuid,
    outcome: UseOutcome,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE owner_questions SET use_outcome = ?2
         WHERE id = ?1 AND used_at IS NOT NULL AND use_outcome IS NULL
         RETURNING bot_id, turn_id",
    )
    .bind(question_id)
    .bind(outcome)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((bot_id, turn_id)) = row else {
        return Ok(false);
    };
    events::turn_changed(&mut tx, bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(true)
}

/// An approved tool call that ran in a turn that did not complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RanAction {
    pub turn_no: i64,
    pub turn_status: turns::TurnStatus,
    pub tool: String,
    pub title: String,
    pub used: Use,
}

/// How many approved calls from turns that did not complete a turn is told
/// about, and how far back: a fresh routine session would otherwise get the
/// same old calls on every run.
const RAN_ACTIONS_SHOWN: i64 = 10;
const RAN_ACTIONS_DAYS: i64 = 7;

/// The latest approved tool calls of the past week that ran in this
/// conversation's turns before `turn_no` that failed, became uncertain, or were stopped, oldest
/// first. A completed turn's own reply already reported its calls.
pub async fn ran_before(
    conn: &mut SqliteConnection,
    conversation_id: Uuid,
    turn_no: i64,
) -> Result<Vec<RanAction>, sqlx::Error> {
    ran(conn, conversation_id, turn_no, None).await
}

/// The same for a delegated task: every turn of its task conversation, plus
/// the owner's takeover turn in the target's chat when there is one.
pub async fn ran_in_task(
    conn: &mut SqliteConnection,
    conversation_id: Uuid,
    takeover_turn: Option<Uuid>,
) -> Result<Vec<RanAction>, sqlx::Error> {
    ran(conn, conversation_id, i64::MAX, takeover_turn).await
}

async fn ran(
    conn: &mut SqliteConnection,
    conversation_id: Uuid,
    before_turn_no: i64,
    also_turn: Option<Uuid>,
) -> Result<Vec<RanAction>, sqlx::Error> {
    type RanRow = (
        i64,
        turns::TurnStatus,
        String,
        String,
        Option<DateTime<Utc>>,
        Option<UseOutcome>,
    );
    let rows: Vec<RanRow> = sqlx::query_as(
        "SELECT t.turn_no, t.status, json_extract(q.action, '$.tool'), q.title, q.used_at,
                q.use_outcome
         FROM owner_questions q JOIN turns t ON t.id = q.turn_id
         WHERE ((t.conversation_id = ?1 AND t.turn_no < ?2) OR t.id = ?3)
           AND t.status IN ('failed', 'uncertain', 'interrupted')
           AND q.used_at > strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', ?5)
         ORDER BY q.used_at DESC, q.id LIMIT ?4",
    )
    .bind(conversation_id)
    .bind(before_turn_no)
    .bind(also_turn)
    .bind(RAN_ACTIONS_SHOWN)
    .bind(format!("-{RAN_ACTIONS_DAYS} days"))
    .fetch_all(&mut *conn)
    .await?;
    let mut ran = rows
        .into_iter()
        .map(|(turn_no, turn_status, tool, title, at, outcome)| {
            Ok(RanAction {
                turn_no,
                turn_status,
                tool,
                title,
                used: Use::decode(at, outcome)?
                    .ok_or_else(|| sqlx::Error::Decode("a ran action without its use".into()))?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    ran.reverse();
    Ok(ran)
}

/// Consumes an approved card for this one call. A tool call's use is stamped
/// and reported to [`track_use`]; other actions only record the attempt.
pub(super) async fn consume(
    tx: &mut SqliteConnection,
    question: &QuestionInfo,
    attempt: i32,
) -> Result<bool, sqlx::Error> {
    let tool = matches!(question.action, Some(Action::Tool { .. }));
    let consumed = sqlx::query(
        "UPDATE owner_questions SET consumed_attempt = ?2,
             used_at = CASE WHEN ?3 THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END
         WHERE id = ?1 AND consumed_attempt IS NULL",
    )
    .bind(question.id)
    .bind(attempt)
    .bind(tool)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if consumed && tool {
        // A call outside `track_use` keeps a started use: its outcome is unknown.
        let _ = CONSUMED.try_with(|slot| slot.set(Some(question.id)));
        events::turn_changed(tx, question.bot_id, question.turn_id).await?;
    }
    Ok(consumed)
}
