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
//! The only standing grant is `destructive` for one bot, given by choosing
//! "Always allow" on a destructive card. External actions are approved one
//! at a time, always.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use super::turns;

pub use crate::app::providers::{QuestionChoice as Choice, QuestionOutcome as Outcome};

/// Questions per owner request (turn).
pub const MAX_PER_TURN: i64 = 8;
pub const MAX_TITLE: usize = 500;
pub const MAX_CONTEXT: usize = 4000;
pub const MAX_CHOICES: usize = 4;
pub const MIN_CHOICES: usize = 2;
pub const MAX_CHOICE_LABEL: usize = 120;

pub const APPROVE: &str = "approve";
pub const DENY: &str = "deny";
pub const APPROVE_FOREVER: &str = "approve_forever";
/// The one standing grant a bot can hold.
pub const DESTRUCTIVE_SCOPE: &str = "destructive";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Choice,
    Destructive,
    External,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Choice => "choice",
            Self::Destructive => "destructive",
            Self::External => "external",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "destructive" => Self::Destructive,
            "external" => Self::External,
            _ => Self::Choice,
        }
    }

    /// The effect a bot may declare for an approval: never `choice`.
    pub fn approval(text: &str) -> Option<Self> {
        match text {
            "destructive" => Some(Self::Destructive),
            "external" => Some(Self::External),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Answered,
    Skipped,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Answered => "answered",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "answered" => Self::Answered,
            "skipped" => Self::Skipped,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }
}

/// The options of an approval card. Only a destructive action offers a
/// standing grant.
pub fn approval_choices(kind: Kind) -> Vec<Choice> {
    let mut choices = vec![
        Choice {
            id: APPROVE.into(),
            label: "Approve once".into(),
        },
        Choice {
            id: DENY.into(),
            label: "Deny".into(),
        },
    ];
    if kind == Kind::Destructive {
        choices.push(Choice {
            id: APPROVE_FOREVER.into(),
            label: "Always allow destructive actions".into(),
        });
    }
    choices
}

/// What the bot asks: a question with options, or an action to approve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub kind: Kind,
    pub title: String,
    pub context: Option<String>,
    pub choices: Vec<Choice>,
    /// What an approval is for; `None` for a choice.
    pub action: Option<Value>,
    /// Binds a tool approval to its exact call; set for every approval.
    pub action_hash: Option<[u8; 32]>,
}

fn text_arg<'a>(args: &'a Value, key: &str, max: usize) -> Result<Option<&'a str>, ()> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() || text.len() > max || text.contains('\0') {
                Err(())
            } else {
                Ok(Some(text))
            }
        }
        Some(_) => Err(()),
    }
}

/// Keys of `args` outside `allowed` are refused (no value rides along).
fn only_keys(args: &Value, allowed: &[&str]) -> Result<(), ()> {
    let object = args.as_object().ok_or(())?;
    if object.keys().all(|k| allowed.contains(&k.as_str())) {
        Ok(())
    } else {
        Err(())
    }
}

/// JSON with object keys in sorted order at every depth, so the same
/// arguments hash the same however the model ordered them.
fn canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                canonical(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
        hasher.update(b"\0");
    }
    hasher.finalize().into()
}

impl Spec {
    /// Omit choices for a question answered through the chat composer.
    pub fn choice(args: &Value) -> Result<Self, &'static str> {
        only_keys(args, &["question", "choices", "context"])
            .map_err(|_| "use question, choices, and optionally context")?;
        let title = text_arg(args, "question", MAX_TITLE)
            .ok()
            .flatten()
            .ok_or("question is 1 to 500 bytes of text")?;
        let context = text_arg(args, "context", MAX_CONTEXT)
            .map_err(|_| "context is at most 4000 bytes of text")?;
        let list = match args.get("choices") {
            None => &[][..],
            Some(Value::Array(list))
                if list.is_empty() || (MIN_CHOICES..=MAX_CHOICES).contains(&list.len()) =>
            {
                list.as_slice()
            }
            _ => return Err("choices lists 2 to 4 short labels, or omit it for a chat answer"),
        };
        let mut choices = Vec::new();
        for (index, label) in list.iter().enumerate() {
            let label = label
                .as_str()
                .map(str::trim)
                .filter(|l| !l.is_empty() && l.len() <= MAX_CHOICE_LABEL && !l.contains('\0'))
                .ok_or("each choice is 1 to 120 bytes of text")?;
            if choices.iter().any(|c: &Choice| c.label == label) {
                return Err("choices must be distinct");
            }
            choices.push(Choice {
                id: ((b'a' + index as u8) as char).to_string(),
                label: label.to_owned(),
            });
        }
        Ok(Self {
            kind: Kind::Choice,
            title: title.to_owned(),
            context: context.map(str::to_owned),
            choices,
            action: None,
            action_hash: None,
        })
    }

    /// Parse `ask_permission` arguments: the kind, the exact action the bot
    /// will take on the host, and why.
    pub fn permission(args: &Value) -> Result<Self, &'static str> {
        only_keys(args, &["kind", "action", "reason"])
            .map_err(|_| "use kind, action, and optionally reason")?;
        let kind = args["kind"]
            .as_str()
            .and_then(Kind::approval)
            .ok_or("kind is destructive or external")?;
        let action = text_arg(args, "action", MAX_CONTEXT)
            .ok()
            .flatten()
            .ok_or("action is 1 to 4000 bytes: exactly what will be done")?;
        let reason = text_arg(args, "reason", MAX_TITLE)
            .map_err(|_| "reason is at most 500 bytes of text")?;
        let title = reason.map(str::to_owned).unwrap_or_else(|| {
            match kind {
                Kind::External => "Allow this action that reaches outside your machine?",
                _ => "Allow this action on your machine?",
            }
            .to_owned()
        });
        Ok(Self {
            kind,
            title,
            context: Some(action.to_owned()),
            choices: approval_choices(kind),
            action: Some(json!({ "host": action })),
            action_hash: Some(hash(&[
                b"host",
                kind.as_str().as_bytes(),
                action.as_bytes(),
            ])),
        })
    }

    /// An approval bound to one computer tool call: the tool and its exact
    /// arguments. `title` and `context` are what the card shows.
    pub fn tool_approval(
        kind: Kind,
        tool: &str,
        args: &Value,
        title: String,
        context: String,
    ) -> Self {
        let mut canonical = String::new();
        self::canonical(args, &mut canonical);
        Self {
            kind,
            title,
            context: Some(context),
            choices: approval_choices(kind),
            action: Some(json!({ "tool": tool, "args": args })),
            action_hash: Some(hash(&[b"tool", tool.as_bytes(), canonical.as_bytes()])),
        }
    }
}

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
    pub action: Option<Value>,
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
                Some(APPROVE) | Some(APPROVE_FOREVER)
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
    String,
    String,
    Option<String>,
    Json<Vec<Choice>>,
    Option<Json<Value>>,
    String,
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
        kind: Kind::parse(&kind),
        title,
        context,
        choices: choices.0,
        action: action.map(|a| a.0),
        status: Status::parse(&status),
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

/// The calling turn, checked for asking: its bot, attempt, and whether it
/// is the owner's conversation.
struct Caller {
    bot_id: Uuid,
    attempt: i32,
    owner_conversation: bool,
}

async fn caller(conn: &mut SqliteConnection, turn_id: Uuid) -> Result<Caller, RequestError> {
    let row: Option<(Uuid, String, i32, String, String, bool)> = sqlx::query_as(
        "SELECT t.bot_id, t.status, t.attempt, c.kind, b.status, b.cancel_pending
         FROM turns t JOIN conversations c ON c.id = t.conversation_id
         JOIN bots b ON b.id = t.bot_id WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((bot_id, status, attempt, kind, bot_status, cancel_pending)) = row else {
        return Err(RequestError::NotRunning);
    };
    if !matches!(status.as_str(), "sending" | "running") || bot_status != "active" || cancel_pending
    {
        return Err(RequestError::NotRunning);
    }
    Ok(Caller {
        bot_id,
        attempt,
        owner_conversation: kind == "owner",
    })
}

/// The newest question of `turn_id` with this title (a choice) or action
/// hash (an approval).
async fn find_same(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    spec: &Spec,
) -> Result<Option<QuestionInfo>, sqlx::Error> {
    let row: Option<Row> = match &spec.action_hash {
        Some(hash) => {
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {COLUMNS} FROM owner_questions WHERE turn_id = ?1 AND action_hash = ?2
                 ORDER BY created_at DESC, id DESC LIMIT 1"
            )))
            .bind(turn_id)
            .bind(hash.as_slice())
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

/// Insert a new pending question for the caller's attempt, after the
/// parking and budget checks.
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
    .bind(spec.action.as_ref().map(Json))
    .bind(spec.action_hash.as_ref().map(|h| h.as_slice()))
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
    let caller = caller(&mut tx, turn_id).await?;
    if let Some(found) = find_same(&mut tx, turn_id, spec).await? {
        if found.kind != spec.kind || found.choices != spec.choices || found.context != spec.context
        {
            return Err(RequestError::Conflict);
        }
        let consumed_host_approval = found.approved()
            && found
                .action
                .as_ref()
                .is_some_and(|a| a.get("host").is_some())
            && sqlx::query_scalar::<_, bool>(
                "SELECT consumed_attempt IS NOT NULL FROM owner_questions WHERE id = ?1",
            )
            .bind(found.id)
            .fetch_one(&mut *tx)
            .await?;
        if !consumed_host_approval {
            return Ok(found);
        }
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

/// What a gated computer tool call may do now.
#[derive(Debug)]
pub enum Gate {
    /// Run it: the owner approved exactly this call (consumed here), or the
    /// bot holds the standing destructive grant.
    Run { standing: bool },
    /// The owner has not answered yet; the attempt must end.
    Pending(QuestionInfo),
    /// The owner declined this call in this request.
    Denied(QuestionInfo),
}

/// Decide a destructive or external computer tool call, atomically: a
/// standing grant runs a destructive call; an approval for exactly these
/// arguments is consumed by this one call; otherwise the owner is asked.
/// A permission request from `ask_permission` is never consumed here: its
/// hash binds a described host action, not a tool call.
pub async fn gate(pool: &SqlitePool, turn_id: Uuid, spec: &Spec) -> Result<Gate, RequestError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = caller(&mut tx, turn_id).await?;
    if spec.kind == Kind::Destructive
        && has_permission(&mut tx, caller.bot_id, DESTRUCTIVE_SCOPE).await?
    {
        return Ok(Gate::Run { standing: true });
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
                    return Ok(Gate::Run { standing: false });
                }
                // Already used once: the same action again needs its own approval.
            }
            Status::Answered => return Ok(Gate::Denied(found)),
            Status::Skipped | Status::Cancelled => {}
        }
    }
    let question = insert(&mut tx, turn_id, &caller, spec).await?;
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
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE owner_questions SET status = 'answered', answer = ?2,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 RETURNING {COLUMNS}"
    )))
    .bind(question.id)
    .bind(choice)
    .fetch_one(&mut *tx)
    .await?;
    if choice == APPROVE_FOREVER {
        // The choice exists only on destructive cards (approval_choices).
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
            kind: q.kind.as_str().to_owned(),
            title: q.title.clone(),
            action: q.action.clone(),
            status: q.status.as_str().to_owned(),
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
                 AND answer IN ('approve', 'approve_forever') THEN ?3 ELSE consumed_attempt END
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_question_parses_only_its_fields_and_letters_its_options() {
        let spec = Spec::choice(&json!({
            "question": " Which newsletter template? ",
            "choices": ["Plain text", "HTML digest", "Both"],
            "context": "Draft: Hello everyone...",
        }))
        .unwrap();
        assert_eq!(spec.kind, Kind::Choice);
        assert_eq!(spec.title, "Which newsletter template?");
        let ids: Vec<&str> = spec.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert_eq!(spec.action, None);
        for bad in [
            json!({ "question": "q", "choices": ["only one"] }),
            json!({ "question": "q", "choices": ["a", "a"] }),
            json!({ "question": "q", "choices": ["a", "b", "c", "d", "e"] }),
            json!({ "question": " ", "choices": ["a", "b"] }),
            json!({ "question": "q", "choices": ["a", "b"], "value": "x" }),
            json!({ "question": "q", "choices": [1, 2] }),
            json!(["q"]),
        ] {
            assert!(Spec::choice(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_permission_binds_the_described_action_and_offers_forever_only_for_destructive() {
        let delete = Spec::permission(&json!({
            "kind": "destructive", "action": "rm -rf ~/old-site", "reason": "Remove the old site",
        }))
        .unwrap();
        assert_eq!(delete.kind, Kind::Destructive);
        assert_eq!(delete.title, "Remove the old site");
        assert_eq!(delete.context.as_deref(), Some("rm -rf ~/old-site"));
        let ids: Vec<&str> = delete.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [APPROVE, DENY, APPROVE_FOREVER]);

        let send =
            Spec::permission(&json!({ "kind": "external", "action": "mail -s hi bob@x" })).unwrap();
        let ids: Vec<&str> = send.choices.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, [APPROVE, DENY]);
        assert_ne!(send.action_hash, delete.action_hash);
        assert!(Spec::permission(&json!({ "kind": "local", "action": "ls" })).is_err());
        assert!(Spec::permission(&json!({ "kind": "external" })).is_err());
    }

    #[test]
    fn a_tool_approval_hashes_the_exact_arguments_regardless_of_key_order() {
        let a = Spec::tool_approval(
            Kind::External,
            "computer_run",
            &json!({ "command": "./send.sh", "effect": "external" }),
            "t".into(),
            "c".into(),
        );
        let b = Spec::tool_approval(
            Kind::External,
            "computer_run",
            &json!({ "effect": "external", "command": "./send.sh" }),
            "t".into(),
            "c".into(),
        );
        let c = Spec::tool_approval(
            Kind::External,
            "computer_run",
            &json!({ "command": "./send.sh --all", "effect": "external" }),
            "t".into(),
            "c".into(),
        );
        assert_eq!(a.action_hash, b.action_hash);
        assert_ne!(a.action_hash, c.action_hash);
        assert_ne!(
            a.action_hash,
            Spec::tool_approval(
                Kind::External,
                "computer_job_start",
                &json!({ "command": "./send.sh", "effect": "external" }),
                "t".into(),
                "c".into()
            )
            .action_hash
        );
    }
}
