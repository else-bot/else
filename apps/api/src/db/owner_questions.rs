//! A bot's question to the owner, answered by choosing one option in the
//! conversation (see docs/decisions/0002-owner-questions.md).
//!
//! Three kinds share one lifecycle. A `choice` asks the owner to pick one
//! of up to four options. A `destructive` or `external` question asks the
//! owner to approve one action: either a computer tool call, bound to its
//! exact arguments by `action_hash` and consumed by exactly one later call
//! with the same arguments, or a host action the bot described with
//! `ask_permission`. The attempt that asks must end; its turn waits and
//! continues once the owner answers. A newer owner message marks a pending
//! question `skipped` (it was not permission); any other end of the turn
//! marks it `cancelled` (a trigger).
//!
//! Destructive actions may be allowed in advance in two separate scopes,
//! each chosen on a destructive card. "Always allow destructive actions"
//! covers the bot's own requests but never a routine run. "Always allow for
//! this routine" covers runs of that one routine admitted with its current
//! prompt revision; editing the prompt ends it. Which scope a card offers
//! and which grant applies come from the turn's source in the database,
//! never from what a model wrote. External actions are approved one at a
//! time, always.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use super::turns::{self, AskingCaller as Caller};

use crate::app::providers::Origin;
use crate::app::questions::{
    APPROVE, APPROVE_FOREVER, APPROVE_ROUTINE, Choice, DESTRUCTIVE_SCOPE, Kind, MAX_PER_TURN,
    Outcome, Spec, Standing, Status, approval_choices,
};

/// A question as shown to the owner and recorded in events.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuestionInfo {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub turn_id: Uuid,
    pub kind: Kind,
    pub title: String,
    pub context: Option<String>,
    pub choices: Vec<Choice>,
    pub action: Option<crate::app::questions::Action>,
    pub status: Status,
    pub answer: Option<String>,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

impl QuestionInfo {
    pub fn is_pending(&self) -> bool {
        self.status == Status::Pending
    }

    pub fn approved(&self) -> bool {
        self.status == Status::Answered
            && matches!(
                self.answer.as_deref(),
                Some(APPROVE) | Some(APPROVE_FOREVER) | Some(APPROVE_ROUTINE)
            )
    }

    pub fn chosen(&self) -> Option<&Choice> {
        let answer = self.answer.as_deref()?;
        self.choices.iter().find(|c| c.id == answer)
    }
}

type Row = (
    Uuid,
    Uuid,
    Uuid,
    Kind,
    String,
    Option<String>,
    Json<Vec<Choice>>,
    Option<Json<crate::app::questions::Action>>,
    Status,
    Option<String>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

const COLUMNS: &str = "id, bot_id, turn_id, kind, title, context, choices, action, status, answer, \
                       created_at, settled_at";

fn info(
    (
        id,
        bot_id,
        turn_id,
        kind,
        title,
        context,
        choices,
        action,
        status,
        answer,
        created_at,
        settled_at,
    ): Row,
) -> QuestionInfo {
    QuestionInfo {
        id,
        bot_id,
        turn_id,
        kind,
        title,
        context,
        choices: choices.0,
        action: action.map(|a| a.0),
        status,
        answer,
        created_at,
        settled_at,
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestError {
    #[error("only the owner's own conversation can ask the owner; delegated tasks cannot")]
    NotAllowed,
    #[error("this attempt already asked or delegated; end it and wait for the answer")]
    AttemptParked,
    #[error("this request already asked the owner {MAX_PER_TURN} times")]
    BudgetExhausted,
    #[error("this request already asked that question with other choices")]
    Conflict,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for RequestError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "owner question failed");
        RequestError::Db
    }
}

async fn find_same(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    spec: &Spec,
) -> Result<Option<QuestionInfo>, sqlx::Error> {
    let row: Option<Row> = match &spec.approval {
        Some(approval) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {COLUMNS} FROM owner_questions WHERE turn_id = ?1 AND action_hash = ?2
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            )))
            .bind(turn_id)
            .bind(approval.hash.as_slice())
            .fetch_optional(&mut *conn)
            .await?
        }
        None => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {COLUMNS} FROM owner_questions
                 WHERE turn_id = ?1 AND kind = 'choice' AND title = ?2
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            )))
            .bind(turn_id)
            .bind(&spec.title)
            .fetch_optional(&mut *conn)
            .await?
        }
    };
    Ok(row.map(info))
}

async fn insert(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    caller: &Caller,
    spec: &Spec,
) -> Result<QuestionInfo, RequestError> {
    if !caller.owner_conversation {
        return Err(RequestError::NotAllowed);
    }
    if turns::attempt_parked(conn, turn_id, caller.attempt).await? {
        return Err(RequestError::AttemptParked);
    }
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions WHERE turn_id = ?1")
        .bind(turn_id)
        .fetch_one(&mut *conn)
        .await?;
    if total >= MAX_PER_TURN {
        return Err(RequestError::BudgetExhausted);
    }
    let id = Uuid::new_v4();
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO owner_questions (id, bot_id, turn_id, attempt, kind, title, context, choices,
             action, action_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) RETURNING {COLUMNS}"
    )))
    .bind(id)
    .bind(caller.bot_id)
    .bind(turn_id)
    .bind(caller.attempt)
    .bind(spec.kind.as_str())
    .bind(&spec.title)
    .bind(&spec.context)
    .bind(Json(&spec.choices))
    .bind(spec.approval.as_ref().map(|a| Json(&a.action)))
    .bind(spec.approval.as_ref().map(|a| a.hash.as_slice()))
    .fetch_one(&mut *conn)
    .await?;
    events::turn_changed(conn, caller.bot_id, turn_id).await?;
    Ok(info(row))
}

/// Record a question from the provider attempt of `turn_id`. Asking the
/// same thing again in this turn returns the earlier question with its
/// current status, so a retried tool call never makes a second card.
pub async fn create(
    pool: &SqlitePool,
    turn_id: Uuid,
    spec: &Spec,
) -> Result<QuestionInfo, RequestError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = turns::asking_caller(&mut tx, turn_id)
        .await?
        .ok_or(RequestError::NotRunning)?;
    if let Some(found) = find_same(&mut tx, turn_id, spec).await? {
        if found.kind != spec.kind || found.choices != spec.choices || found.context != spec.context
        {
            return Err(RequestError::Conflict);
        }
        return Ok(found);
    }
    let question = insert(&mut tx, turn_id, &caller, spec).await?;
    tx.commit().await?;
    Ok(question)
}

/// Whether `bot_id` holds the standing grant for `scope`.
pub async fn has_permission(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    scope: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM bot_permissions WHERE bot_id = ?1 AND scope = ?2)",
    )
    .bind(bot_id)
    .bind(scope)
    .fetch_one(&mut *conn)
    .await
}

/// Which standing grant can apply to a turn, by where its work comes from.
#[derive(Debug, Clone, Copy)]
enum Scope {
    /// An owner request or other work of the bot: the bot's own grant.
    Bot,
    /// A routine run. `current`: it was admitted with the routine's current
    /// prompt revision and the routine still exists, so a grant can be
    /// given and used. `granted`: the routine's grant is for that revision.
    Routine { current: bool, granted: bool },
    /// A task a routine run delegated: no standing grant applies.
    RoutineTask,
}

impl Scope {
    /// The standing grant a destructive card of this turn offers.
    fn offer(self) -> Option<Standing> {
        match self {
            Scope::Bot => Some(Standing::Bot),
            Scope::Routine { current: true, .. } => Some(Standing::Routine),
            Scope::Routine { .. } | Scope::RoutineTask => None,
        }
    }
}

async fn scope(conn: &mut SqliteConnection, turn_id: Uuid) -> Result<Scope, sqlx::Error> {
    let run: Option<(bool, bool)> = sqlx::query_as(
        "SELECT coalesce(rr.prompt_revision = r.prompt_revision AND r.deleted_at IS NULL, false),
             EXISTS (SELECT 1 FROM routine_grants g
                     WHERE g.routine_id = rr.routine_id AND g.prompt_revision = rr.prompt_revision)
         FROM routine_runs rr JOIN routines r ON r.id = rr.routine_id WHERE rr.turn_id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(match run {
        Some((current, granted)) => Scope::Routine { current, granted },
        None if super::delegations::origin(conn, turn_id).await? == Origin::RoutineRun => {
            Scope::RoutineTask
        }
        None => Scope::Bot,
    })
}

/// What a gated computer tool call may do now.
#[derive(Debug)]
pub enum Gate {
    /// Run it: the owner approved exactly this call (consumed here), or a
    /// standing destructive grant of this scope applies.
    Run { standing: Option<Standing> },
    /// The owner has not answered yet; the attempt must end.
    Pending(QuestionInfo),
    /// The owner declined this call in this request.
    Denied(QuestionInfo),
}

/// Decide a destructive or external action, atomically: a standing grant
/// of the turn's scope runs a destructive call; an approval for exactly
/// these arguments is consumed by this one call; otherwise the owner is
/// asked, on a card offering only the grant this turn's scope can receive.
pub async fn gate(pool: &SqlitePool, turn_id: Uuid, spec: &Spec) -> Result<Gate, RequestError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = turns::asking_caller(&mut tx, turn_id)
        .await?
        .ok_or(RequestError::NotRunning)?;
    let scope = scope(&mut tx, turn_id).await?;
    if spec.kind == Kind::Destructive {
        let standing = match scope {
            Scope::Bot if has_permission(&mut tx, caller.bot_id, DESTRUCTIVE_SCOPE).await? => {
                Some(Standing::Bot)
            }
            Scope::Routine {
                current: true,
                granted: true,
            } => Some(Standing::Routine),
            _ => None,
        };
        if standing.is_some() {
            return Ok(Gate::Run { standing });
        }
    }
    if let Some(found) = find_same(&mut tx, turn_id, spec).await? {
        match found.status {
            Status::Pending => return Ok(Gate::Pending(found)),
            Status::Answered if found.approved() => {
                let consumed = sqlx::query(
                    "UPDATE owner_questions SET consumed_attempt = ?2
                     WHERE id = ?1 AND consumed_attempt IS NULL",
                )
                .bind(found.id)
                .bind(caller.attempt)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    == 1;
                if consumed {
                    tx.commit().await?;
                    return Ok(Gate::Run { standing: None });
                }
            }
            Status::Answered => return Ok(Gate::Denied(found)),
            Status::Skipped | Status::Cancelled => {}
        }
    }
    let spec = Spec {
        choices: approval_choices(spec.kind, scope.offer()),
        ..spec.clone()
    };
    let question = insert(&mut tx, turn_id, &caller, &spec).await?;
    tx.commit().await?;
    Ok(Gate::Pending(question))
}

/// Every question of these turns, oldest first per turn.
pub async fn for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<QuestionInfo>>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM owner_questions
         WHERE lower(hex(turn_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1))
         ORDER BY created_at, id"
    )))
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    let mut by_turn: HashMap<Uuid, Vec<QuestionInfo>> = HashMap::new();
    for row in rows {
        let question = info(row);
        by_turn.entry(question.turn_id).or_default().push(question);
    }
    Ok(by_turn)
}

#[derive(Debug, thiserror::Error)]
pub enum SettleError {
    #[error("not found")]
    NotFound,
    #[error("the question was already {}", .0.as_str())]
    Settled(Status),
    #[error("choose one of this question's options")]
    InvalidChoice,
    #[error("The routine task changed or was deleted. Approve once to allow only this action.")]
    RoutineChanged,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The result of an owner answer. `changed` is false for a retry of the
/// answer already recorded; nothing was written then.
#[derive(Debug, Clone)]
pub struct Settled {
    pub question: QuestionInfo,
    pub changed: bool,
    /// The waiting turn was queued again as a continuation.
    pub requeued: bool,
}

async fn load(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    question_id: Uuid,
) -> Result<QuestionInfo, SettleError> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT q.{} FROM owner_questions q JOIN bots b ON b.id = q.bot_id
         WHERE q.id = ?1 AND q.bot_id = ?2 AND b.status = 'active'",
        COLUMNS.replace(", ", ", q.")
    )))
    .bind(question_id)
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(info).ok_or(SettleError::NotFound)
}

/// The owner picks `choice` for a pending question and its turn continues
/// once nothing else holds it, atomically. A retry of the same answer
/// changes nothing; any other answer to a settled question is refused, so
/// a stale card can never answer for a newer state.
pub async fn answer(
    pool: &SqlitePool,
    bot_id: Uuid,
    question_id: Uuid,
    choice: &str,
) -> Result<Settled, SettleError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let question = load(&mut tx, bot_id, question_id).await?;
    if !question.choices.iter().any(|c| c.id == choice) {
        return Err(SettleError::InvalidChoice);
    }
    if question.status != Status::Pending {
        if question.status == Status::Answered && question.answer.as_deref() == Some(choice) {
            return Ok(Settled {
                question,
                changed: false,
                requeued: false,
            });
        }
        return Err(SettleError::Settled(question.status));
    }
    if question.kind == Kind::Destructive {
        match (choice, scope(&mut tx, question.turn_id).await?) {
            (APPROVE_ROUTINE, Scope::Routine { current: true, .. }) => {}
            (APPROVE_ROUTINE, _) => return Err(SettleError::RoutineChanged),
            (APPROVE_FOREVER, Scope::Bot) => {}
            (APPROVE_FOREVER, _) => return Err(SettleError::InvalidChoice),
            _ => {}
        }
    }
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE owner_questions SET status = 'answered', answer = ?2,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 RETURNING {COLUMNS}"
    )))
    .bind(question.id)
    .bind(choice)
    .fetch_one(&mut *tx)
    .await?;
    if question.kind == Kind::Destructive && choice == APPROVE_FOREVER {
        sqlx::query(
            "INSERT INTO bot_permissions (bot_id, scope, question_id) VALUES (?1, ?2, ?3)
             ON CONFLICT (bot_id, scope) DO NOTHING",
        )
        .bind(bot_id)
        .bind(DESTRUCTIVE_SCOPE)
        .bind(question.id)
        .execute(&mut *tx)
        .await?;
    }
    if question.kind == Kind::Destructive && choice == APPROVE_ROUTINE {
        sqlx::query(
            "INSERT INTO routine_grants (routine_id, prompt_revision, question_id)
             SELECT r.id, r.prompt_revision, ?2
             FROM routine_runs rr JOIN routines r ON r.id = rr.routine_id
             WHERE rr.turn_id = ?1 AND r.bot_id = ?3 AND r.deleted_at IS NULL
               AND rr.prompt_revision = r.prompt_revision
             ON CONFLICT (routine_id) DO UPDATE SET prompt_revision = excluded.prompt_revision,
                 question_id = excluded.question_id, granted_at = excluded.granted_at",
        )
        .bind(question.turn_id)
        .bind(question.id)
        .bind(bot_id)
        .execute(&mut *tx)
        .await?;
    }
    let requeued = turns::continue_waiting(&mut tx, question.turn_id).await?;
    events::turn_changed(&mut tx, bot_id, question.turn_id).await?;
    tx.commit().await?;
    Ok(Settled {
        question: info(row),
        changed: true,
        requeued,
    })
}

/// Answers of a turn's questions not yet given to an attempt.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM owner_questions
         WHERE turn_id = ?1 AND delivered_attempt IS NULL AND status <> 'pending'
         ORDER BY created_at, id"
    )))
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(info)
        .map(|q| Outcome {
            id: q.id,
            kind: q.kind,
            title: q.title.clone(),
            action: q.action.clone(),
            status: q.status,
            answer: q.chosen().cloned(),
        })
        .collect())
}

/// Record that `attempt` received these answers.
pub async fn mark_delivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
    ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE owner_questions SET delivered_attempt = ?3,
             consumed_attempt = CASE WHEN json_type(action, '$.host') = 'text'
                 AND answer IN ('approve', 'approve_forever', 'approve_routine') THEN ?3 ELSE consumed_attempt END
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

/// A standing grant the owner gave a bot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Permission {
    pub scope: String,
    pub granted_at: DateTime<Utc>,
    pub question_id: Option<Uuid>,
}

pub async fn permissions(pool: &SqlitePool, bot_id: Uuid) -> Result<Vec<Permission>, sqlx::Error> {
    let rows: Vec<(String, DateTime<Utc>, Option<Uuid>)> = sqlx::query_as(
        "SELECT scope, granted_at, question_id FROM bot_permissions WHERE bot_id = ?1 ORDER BY scope",
    )
    .bind(bot_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(scope, granted_at, question_id)| Permission {
            scope,
            granted_at,
            question_id,
        })
        .collect())
}

/// Withdraw a standing grant. False when the bot did not hold it.
pub async fn revoke_permission(
    pool: &SqlitePool,
    bot_id: Uuid,
    scope: &str,
) -> Result<bool, sqlx::Error> {
    let removed = sqlx::query("DELETE FROM bot_permissions WHERE bot_id = ?1 AND scope = ?2")
        .bind(bot_id)
        .bind(scope)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(removed == 1)
}

/// The owner's standing approval of destructive actions for one routine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutineGrant {
    pub routine_id: Uuid,
    pub granted_at: DateTime<Utc>,
    pub question_id: Uuid,
}

/// The routine grants of a bot that still apply: to an existing routine,
/// for its current prompt.
pub async fn routine_grants(
    pool: &SqlitePool,
    bot_id: Uuid,
) -> Result<Vec<RoutineGrant>, sqlx::Error> {
    let rows: Vec<(Uuid, DateTime<Utc>, Uuid)> = sqlx::query_as(
        "SELECT g.routine_id, g.granted_at, g.question_id
         FROM routine_grants g JOIN routines r ON r.id = g.routine_id
         WHERE r.bot_id = ?1 AND r.deleted_at IS NULL AND g.prompt_revision = r.prompt_revision
         ORDER BY g.granted_at, g.routine_id",
    )
    .bind(bot_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(routine_id, granted_at, question_id)| RoutineGrant {
            routine_id,
            granted_at,
            question_id,
        })
        .collect())
}

/// Withdraw a routine's grant. False when it held none.
pub async fn revoke_routine_grant(
    pool: &SqlitePool,
    bot_id: Uuid,
    routine_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let removed = sqlx::query(
        "DELETE FROM routine_grants WHERE routine_id = ?1
             AND routine_id IN (SELECT id FROM routines WHERE bot_id = ?2)",
    )
    .bind(routine_id)
    .bind(bot_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(removed == 1)
}
