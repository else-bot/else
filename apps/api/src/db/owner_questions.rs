//! A bot's question to the owner, answered by choosing one option in the
//! conversation.
//!
//! Three kinds share one lifecycle. A `choice` asks the owner to pick one
//! or several of up to five options. A `destructive` or `external` question asks the
//! owner to approve one action: either a computer tool call, bound to its
//! exact arguments by `action_hash` and consumed by exactly one later call
//! with the same arguments, or a host action the bot described with
//! `ask_permission`. The attempt that asks must end; its turn waits and
//! continues once the owner answers. A newer owner message marks a pending
//! question `skipped` (it was not permission); any other end of the turn
//! marks it `cancelled` (a trigger).
//!
//! Actions may be allowed in advance, chosen on a card or explicitly
//! authorized in owner chat. The broad bot grant covers destructive actions
//! in the bot's own requests but never a routine run; host action cards no
//! longer offer it. "Always allow for this routine" covers runs of that one
//! routine admitted with its current prompt revision; editing the prompt
//! ends it. A tool grant covers one MCP tool and effect, and a task grant
//! one declared task, target, and effect of host actions and computer tool calls. A tool grant
//! applies only in the bot's owner requests; a task grant also in tasks
//! delegated to the bot for an owner request, whose cards approve once, and
//! in scheduled runs of the bot's routines. Which scope a card offers and which grant applies
//! come from the turn's source in the database; a task grant matches the
//! declared task and target exactly and never interprets the action.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use super::turns::{self, AskingCaller as Caller};

pub mod standing;

use crate::app::providers::Origin;
use crate::app::questions::{
    APPROVE, APPROVE_FOREVER, APPROVE_ROUTINE, APPROVE_TASK, APPROVE_TOOL, Action, Answer, Choice,
    DESTRUCTIVE_SCOPE, GrantReach, Kind, MAX_PER_TURN, Outcome, PermissionTarget, Spec, Standing,
    Status, approval_choices,
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
    pub multiple: bool,
    pub action: Option<crate::app::questions::Action>,
    pub status: Status,
    pub answer: Option<Answer>,
    pub created_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
    /// What became of the approved tool call once the gate let it run.
    pub used: Option<Use>,
}

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
    fn decode(
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
async fn consume(
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

impl QuestionInfo {
    pub fn is_pending(&self) -> bool {
        self.status == Status::Pending
    }

    pub fn approved(&self) -> bool {
        self.status == Status::Answered
            && matches!(
                self.answer.as_ref().and_then(Answer::single),
                Some(APPROVE)
                    | Some(APPROVE_FOREVER)
                    | Some(APPROVE_ROUTINE)
                    | Some(APPROVE_TOOL)
                    | Some(APPROVE_TASK)
            )
    }

    pub fn chosen(&self) -> Option<&Choice> {
        let answer = self.answer.as_ref()?.single()?;
        self.choices.iter().find(|c| c.id == answer)
    }

    pub fn selected(&self) -> Vec<Choice> {
        self.choices
            .iter()
            .filter(|choice| self.answer.as_ref().is_some_and(|a| a.contains(&choice.id)))
            .cloned()
            .collect()
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
    bool,
    Option<DateTime<Utc>>,
    Option<UseOutcome>,
);

const COLUMNS: &str = "id, bot_id, turn_id, kind, title, context, choices, action, status, answer, \
                       created_at, settled_at, multiple, used_at, use_outcome";

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
        multiple,
        used_at,
        use_outcome,
    ): Row,
) -> Result<QuestionInfo, sqlx::Error> {
    let answer = answer
        .map(|answer| {
            if multiple {
                serde_json::from_str(&answer)
                    .map(Answer::Multiple)
                    .map_err(|err| sqlx::Error::Decode(Box::new(err)))
            } else {
                Ok(Answer::Single(answer))
            }
        })
        .transpose()?;
    Ok(QuestionInfo {
        id,
        bot_id,
        turn_id,
        kind,
        title,
        context,
        choices: choices.0,
        multiple,
        action: action.map(|a| a.0),
        status,
        answer,
        created_at,
        settled_at,
        used: Use::decode(used_at, use_outcome)?,
    })
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestError {
    #[error(
        "only the owner's own conversation or a task delegated for an owner request can ask the owner"
    )]
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
    row.map(info).transpose()
}

async fn insert(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    caller: &Caller,
    spec: &Spec,
) -> Result<QuestionInfo, RequestError> {
    if !turns::may_ask_owner(conn, turn_id, caller).await? {
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
             action, action_hash, multiple)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) RETURNING {COLUMNS}"
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
    .bind(spec.multiple)
    .fetch_one(&mut *conn)
    .await?;
    Ok(info(row)?)
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
        if found.kind != spec.kind
            || found.choices != spec.choices
            || found.context != spec.context
            || found.multiple != spec.multiple
        {
            return Err(RequestError::Conflict);
        }
        return Ok(found);
    }
    let question = insert(&mut tx, turn_id, &caller, spec).await?;
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
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

#[derive(Debug, Clone, Copy)]
enum Scope {
    Bot,
    Routine { current: bool, granted: bool },
    RoutineTask,
}

impl Scope {
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

/// Which saved grants `turn_id` may use.
async fn reach(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    scope: Scope,
) -> Result<GrantReach, sqlx::Error> {
    let owner_chat: Option<bool> = sqlx::query_scalar(
        "SELECT c.kind = 'owner' FROM turns t JOIN conversations c ON c.id = t.conversation_id
         WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(GrantReach::of(
        super::delegations::origin(conn, turn_id).await?,
        matches!(scope, Scope::Routine { .. }),
        owner_chat.unwrap_or(false),
    ))
}

/// Which saved grants `turn_id` may use.
pub(crate) async fn grant_reach(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<GrantReach, sqlx::Error> {
    let scope = scope(conn, turn_id).await?;
    reach(conn, turn_id, scope).await
}

/// What a gated computer tool call may do now.
#[derive(Debug)]
pub enum Gate {
    /// Run it: the owner approved exactly this call (consumed here), or a
    /// standing grant of this scope applies.
    Run { standing: Option<Standing> },
    /// The owner has not answered yet; the attempt must end.
    Pending(QuestionInfo),
    /// The owner declined this call in this request.
    Denied(QuestionInfo),
}

async fn owner_message_quotes(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    quote: &str,
) -> Result<bool, sqlx::Error> {
    let source: Option<String> = sqlx::query_scalar(
        "SELECT i.text FROM turns t JOIN items i ON i.id = t.owner_item_id
         WHERE t.id = ?1 AND i.kind = 'user'",
    )
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(source.is_some_and(|text| text.contains(quote)))
}

/// Decide a destructive or external action, atomically: a broad or routine
/// grant of the turn's scope runs a destructive call; a denial of exactly
/// this action in this request stands; an approval for exactly these
/// arguments is consumed by this one call; a tool or task grant matching the
/// action runs it in an owner request (a task grant also in work delegated
/// for one and in a scheduled routine run); otherwise the owner is asked, on a
/// card offering only the grant this turn's scope can receive. A host action
/// without a task scope is offered no standing grant.
pub async fn gate(pool: &SqlitePool, turn_id: Uuid, spec: &Spec) -> Result<Gate, RequestError> {
    gate_host(pool, turn_id, spec, None).await
}

/// [`gate`] for a host action the bot may say the owner asked for, quoting
/// `authorization` from the request's owner message. A quote found in this
/// turn's own owner chat message runs the action without a card, in this
/// request only: it records nothing, offers no standing grant, and never
/// overrides a pending or denied card for the same action. Whether the quote
/// asks for exactly this action is the bot's judgment, as with any host
/// command; the gate enforces only its source and lifetime.
pub async fn gate_host(
    pool: &SqlitePool,
    turn_id: Uuid,
    spec: &Spec,
    authorization: Option<&str>,
) -> Result<Gate, RequestError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = turns::asking_caller(&mut tx, turn_id)
        .await?
        .ok_or(RequestError::NotRunning)?;
    let scope = scope(&mut tx, turn_id).await?;
    let reach = reach(&mut tx, turn_id, scope).await?;
    let target = spec
        .approval
        .as_ref()
        .and_then(|a| PermissionTarget::for_action(spec.kind, &a.action))
        .filter(|t| reach.covers(t));
    let granted = match &target {
        Some(target)
            if has_permission(&mut tx, caller.bot_id, &target.bot_scope().unwrap()).await? =>
        {
            Some(target.standing())
        }
        _ => None,
    };
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
            // The card approved with a broad grant still shows that its call ran.
            if let Some(found) = find_same(&mut tx, turn_id, spec).await?
                && found.approved()
                && consume(&mut tx, &found, caller.attempt).await?
            {
                tx.commit().await?;
            }
            return Ok(Gate::Run { standing });
        }
    }
    if let Some(found) = find_same(&mut tx, turn_id, spec).await? {
        match found.status {
            Status::Pending => return Ok(Gate::Pending(found)),
            Status::Answered if found.approved() => {
                if consume(&mut tx, &found, caller.attempt).await? {
                    tx.commit().await?;
                    return Ok(Gate::Run { standing: granted });
                }
            }
            Status::Answered => return Ok(Gate::Denied(found)),
            Status::Skipped | Status::Cancelled => {}
        }
    }
    if granted.is_some() {
        return Ok(Gate::Run { standing: granted });
    }
    let host = matches!(
        spec.approval.as_ref().map(|a| &a.action),
        Some(Action::Host { .. })
    );
    if let Some(quote) = authorization.filter(|_| host)
        && matches!(scope, Scope::Bot)
        && caller.owner_conversation
        && owner_message_quotes(&mut tx, turn_id, quote).await?
    {
        if turns::attempt_parked(&mut tx, turn_id, caller.attempt).await? {
            return Err(RequestError::AttemptParked);
        }
        return Ok(Gate::Run { standing: None });
    }
    let offer = match (scope, &target) {
        (_, Some(target)) => Some(target.standing()),
        (Scope::Bot, None) if host => None,
        _ => scope.offer(),
    };
    let spec = Spec {
        // Work another bot delegated approves one call at a time: its card
        // never offers a standing grant.
        choices: approval_choices(spec.kind, offer.filter(|_| caller.owner_conversation)),
        ..spec.clone()
    };
    let question = insert(&mut tx, turn_id, &caller, &spec).await?;
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
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
        let question = info(row)?;
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
    #[error("Sign in to the inbox before approving this connection.")]
    InboxNotConnected,
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

/// One question by its ID, in any state.
pub async fn get(
    conn: &mut SqliteConnection,
    question_id: Uuid,
) -> Result<Option<QuestionInfo>, sqlx::Error> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM owner_questions WHERE id = ?1"
    )))
    .bind(question_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(info).transpose()
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
    Ok(info(row.ok_or(SettleError::NotFound)?)?)
}

/// The owner submits an answer for a pending question and its turn continues
/// once nothing else holds it, atomically. A retry of the same answer
/// changes nothing; any other answer to a settled question is refused, so
/// a stale card can never answer for a newer state.
pub async fn answer(
    pool: &SqlitePool,
    bot_id: Uuid,
    question_id: Uuid,
    answer: &Answer,
) -> Result<Settled, SettleError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let question = load(&mut tx, bot_id, question_id).await?;
    let answer = match (question.multiple, answer) {
        (false, Answer::Single(id)) if question.choices.iter().any(|c| c.id == *id) => {
            Answer::Single(id.clone())
        }
        (true, Answer::Multiple(ids))
            if !ids.is_empty()
                && ids.len() <= question.choices.len()
                && ids
                    .iter()
                    .all(|id| question.choices.iter().any(|c| c.id == *id)) =>
        {
            let ordered: Vec<String> = question
                .choices
                .iter()
                .filter(|c| ids.contains(&c.id))
                .map(|c| c.id.clone())
                .collect();
            if ordered.len() != ids.len() {
                return Err(SettleError::InvalidChoice);
            }
            Answer::Multiple(ordered)
        }
        _ => return Err(SettleError::InvalidChoice),
    };
    if question.status != Status::Pending {
        if question.status == Status::Answered && question.answer.as_ref() == Some(&answer) {
            return Ok(Settled {
                question,
                changed: false,
                requeued: false,
            });
        }
        return Err(SettleError::Settled(question.status));
    }
    let choice = answer.single();
    let standing = match choice {
        Some(APPROVE_TOOL) => Some(Standing::Tool),
        Some(APPROVE_TASK) => Some(Standing::Task),
        _ => None,
    };
    let granted_scope = match standing {
        Some(standing) => {
            let scope = scope(&mut tx, question.turn_id).await?;
            let reach = reach(&mut tx, question.turn_id, scope).await?;
            Some(
                question
                    .action
                    .as_ref()
                    .and_then(|a| PermissionTarget::for_action(question.kind, a))
                    .filter(|target| target.standing() == standing && reach.covers(target))
                    .and_then(|target| target.bot_scope())
                    .ok_or(SettleError::InvalidChoice)?,
            )
        }
        None => None,
    };
    if question.kind == Kind::Destructive {
        match (choice, scope(&mut tx, question.turn_id).await?) {
            (Some(APPROVE_ROUTINE), Scope::Routine { current: true, .. }) => {}
            (Some(APPROVE_ROUTINE), _) => return Err(SettleError::RoutineChanged),
            (Some(APPROVE_FOREVER), Scope::Bot) => {}
            (Some(APPROVE_FOREVER), _) => return Err(SettleError::InvalidChoice),
            _ => {}
        }
    }
    // An inbox card approves only the requested address, or an account of
    // its provider connected since it was asked: Google may authorize a
    // different mailbox than the bot named. The asking bot then wakes on
    // that inbox's new mail.
    if choice == Some(APPROVE)
        && let Some(Action::EmailConnection { email_connection }) = &question.action
    {
        let connected: Vec<(Uuid, bool)> = sqlx::query_as(
            "SELECT id, lower(json_extract(config, '$.address')) = ?1 FROM connector_accounts
             WHERE state <> 'removed'
                AND (lower(json_extract(config, '$.address')) = ?1
                     OR (json_extract(config, '$.provider') = ?3
                         AND created_at >= (SELECT created_at FROM owner_questions WHERE id = ?2)))",
        )
        .bind(email_connection.address.to_lowercase())
        .bind(question.id)
        .bind(
            serde_json::to_value(email_connection.provider)
                .map_err(|err| sqlx::Error::Encode(Box::new(err)))?
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
        .fetch_all(&mut *tx)
        .await?;
        if connected.is_empty() {
            return Err(SettleError::InboxNotConnected);
        }
        // Wake only on an inbox this card surely connected: the requested
        // address, or the one account of its provider connected since.
        let account = match connected.iter().find(|(_, exact)| *exact) {
            Some((id, _)) => Some(*id),
            None if connected.len() == 1 => Some(connected[0].0),
            None => None,
        };
        if let Some(account_id) = account {
            use crate::connectors::store::{StoreError, wake_if_readable_in};
            match wake_if_readable_in(&mut tx, account_id, bot_id).await {
                Ok(_) => {}
                Err(StoreError::Db(error)) => return Err(error.into()),
                Err(error) => {
                    tracing::warn!(%account_id, %bot_id, %error, "inbox connected without wake-ups");
                }
            }
        }
    }
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE owner_questions SET status = 'answered', answer = ?2,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 RETURNING {COLUMNS}"
    )))
    .bind(question.id)
    .bind(match &answer {
        Answer::Single(id) => id.clone(),
        Answer::Multiple(ids) => {
            serde_json::to_string(ids).map_err(|err| sqlx::Error::Encode(Box::new(err)))?
        }
    })
    .fetch_one(&mut *tx)
    .await?;
    if question.kind == Kind::Destructive && choice == Some(APPROVE_FOREVER) {
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
    if let Some(scope) = granted_scope {
        sqlx::query(
            "INSERT INTO bot_permissions (bot_id, scope, question_id) VALUES (?1, ?2, ?3)
             ON CONFLICT (bot_id, scope) DO NOTHING",
        )
        .bind(bot_id)
        .bind(scope)
        .bind(question.id)
        .execute(&mut *tx)
        .await?;
    }
    if question.kind == Kind::Destructive && choice == Some(APPROVE_ROUTINE) {
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
        question: info(row)?,
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
    rows.into_iter()
        .map(|row| {
            let q = info(row)?;
            Ok(Outcome {
                answers: q.selected(),
                id: q.id,
                kind: q.kind,
                title: q.title,
                action: q.action,
                status: q.status,
            })
        })
        .collect()
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
                 AND answer IN ('approve', 'approve_forever', 'approve_routine', 'approve_task')
                 THEN ?3 ELSE consumed_attempt END
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
    pub target: Option<PermissionTarget>,
}

pub async fn permissions(pool: &SqlitePool, bot_id: Uuid) -> Result<Vec<Permission>, sqlx::Error> {
    type PermissionRow = (
        String,
        DateTime<Utc>,
        Option<Uuid>,
        Option<Json<crate::app::questions::Action>>,
        Option<Kind>,
    );
    let rows: Vec<PermissionRow> = sqlx::query_as(
        "SELECT p.scope, p.granted_at, p.question_id, q.action, q.kind
         FROM bot_permissions p LEFT JOIN owner_questions q ON q.id = p.question_id
         WHERE p.bot_id = ?1 ORDER BY p.scope",
    )
    .bind(bot_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(scope, granted_at, question_id, action, kind)| Permission {
                scope,
                granted_at,
                question_id,
                target: action.and_then(|action| match action.0 {
                    crate::app::questions::Action::Permission { permission } => Some(permission),
                    action => kind.and_then(|kind| PermissionTarget::for_action(kind, &action)),
                }),
            },
        )
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
