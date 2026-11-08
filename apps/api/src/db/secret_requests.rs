//! A bot's request for credentials, answered by the owner in a private form.
//!
//! The provider attempt names a task secret and the variables it needs
//! (`request_secret`); only that metadata is stored and shown. The attempt
//! then ends and its turn waits (`waiting`) with no provider process. The
//! owner's values go through the owner API straight into the shared `task_secrets`,
//! encrypted, in the same transaction that marks the request stored and
//! requeues the turn as a continuation. A continuation learns only the
//! secret's name, variable names, and whether it was stored or declined.
//! Ending the turn for any reason withdraws a pending request (a trigger).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use super::task_secrets::{self, SecretError, valid_name, valid_var};
use crate::credentials::SecretKey;

/// Requests per owner request (turn).
pub const MAX_PER_TURN: i64 = 3;
pub const MAX_REASON: usize = 500;

pub use crate::app::providers::SecretOutcome as Outcome;

/// What the bot asks for: a task secret name, why, and its variables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    pub reason: String,
    pub env: BTreeSet<String>,
}

impl Spec {
    /// Parse tool arguments: exactly `name`, `reason`, and `env`, nothing
    /// else, so no value can ride along. Errors never echo the input.
    pub fn parse(args: &Value) -> Result<Self, &'static str> {
        let Some(args) = args.as_object() else {
            return Err("arguments must be an object with name, reason, and env");
        };
        if args.len() != 3
            || !["name", "reason", "env"]
                .iter()
                .all(|k| args.contains_key(*k))
        {
            return Err("use exactly name, reason, and env; never send values");
        }
        let name = args["name"]
            .as_str()
            .filter(|n| valid_name(n))
            .ok_or("name is 1 to 40 of a-z, 0-9, _ and -, starting with a letter")?;
        let reason = args["reason"]
            .as_str()
            .map(str::trim)
            .filter(|r| !r.is_empty() && r.len() <= MAX_REASON && !r.contains('\0'))
            .ok_or("reason is 1 to 500 bytes of text")?;
        let list = args["env"]
            .as_array()
            .filter(|l| (1..=task_secrets::MAX_VARS).contains(&l.len()))
            .ok_or("env lists 1 to 16 variable names")?;
        let mut env = BTreeSet::new();
        for var in list {
            let var = var
                .as_str()
                .filter(|v| valid_var(v))
                .ok_or("each env entry is an allowed variable name (A-Z, 0-9, _)")?;
            if !env.insert(var.to_owned()) {
                return Err("env names must be distinct");
            }
        }
        Ok(Self {
            name: name.to_owned(),
            reason: reason.to_owned(),
            env,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Stored,
    Cancelled,
}

impl Status {
    fn parse(text: &str) -> Self {
        match text {
            "stored" => Self::Stored,
            "cancelled" => Self::Cancelled,
            _ => Self::Pending,
        }
    }
}

/// Public metadata of a request. It never holds a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestInfo {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub turn_id: Uuid,
    pub name: String,
    pub reason: String,
    pub env: Vec<String>,
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
    Json<Vec<String>>,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

const COLUMNS: &str =
    "id, bot_id, turn_id, name, reason, env_names, status, created_at, settled_at";

fn info(
    (id, bot_id, turn_id, name, reason, env, status, created_at, settled_at): Row,
) -> RequestInfo {
    RequestInfo {
        id,
        bot_id,
        turn_id,
        name,
        reason,
        env: env.0,
        status: Status::parse(&status),
        created_at,
        settled_at,
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RequestError {
    #[error(
        "only the owner's own conversation or a task delegated for an owner request can ask for secrets"
    )]
    NotAllowed,
    #[error("this attempt already asked or delegated; end it and wait for the answer")]
    AttemptParked,
    #[error("this request already asked for {MAX_PER_TURN} secrets")]
    BudgetExhausted,
    #[error("this request already asked for that secret with other variables or reason")]
    Conflict,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for RequestError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "secret request failed");
        RequestError::Db
    }
}

/// Record a request from the provider attempt of `turn_id`. A retry of the
/// same request returns it with its current status.
pub async fn create(
    pool: &SqlitePool,
    turn_id: Uuid,
    spec: &Spec,
) -> Result<RequestInfo, RequestError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = super::turns::asking_caller(&mut tx, turn_id)
        .await?
        .ok_or(RequestError::NotRunning)?;
    if !super::turns::may_ask_owner(&mut tx, turn_id, &caller).await? {
        return Err(RequestError::NotAllowed);
    }
    let (bot_id, attempt) = (caller.bot_id, caller.attempt);
    let existing: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM secret_requests WHERE turn_id = ?1 AND name = ?2"
    )))
    .bind(turn_id)
    .bind(&spec.name)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = existing {
        let found = info(row);
        let env: BTreeSet<String> = found.env.iter().cloned().collect();
        if env != spec.env || found.reason != spec.reason {
            return Err(RequestError::Conflict);
        }
        return Ok(found);
    }
    if super::turns::attempt_parked(&mut tx, turn_id, attempt).await? {
        return Err(RequestError::AttemptParked);
    }
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM secret_requests WHERE turn_id = ?1")
        .bind(turn_id)
        .fetch_one(&mut *tx)
        .await?;
    if total >= MAX_PER_TURN {
        return Err(RequestError::BudgetExhausted);
    }
    let id = Uuid::new_v4();
    let env: Vec<&String> = spec.env.iter().collect();
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO secret_requests (id, bot_id, turn_id, attempt, name, reason, env_names)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING {COLUMNS}"
    )))
    .bind(id)
    .bind(bot_id)
    .bind(turn_id)
    .bind(attempt)
    .bind(&spec.name)
    .bind(&spec.reason)
    .bind(Json(&env))
    .fetch_one(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(info(row))
}

/// Every request of these turns, oldest first per turn.
pub async fn for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<RequestInfo>>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM secret_requests
         WHERE lower(hex(turn_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1))
         ORDER BY created_at, id"
    )))
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    let mut by_turn: HashMap<Uuid, Vec<RequestInfo>> = HashMap::new();
    for row in rows {
        let request = info(row);
        by_turn.entry(request.turn_id).or_default().push(request);
    }
    Ok(by_turn)
}

#[derive(Debug, thiserror::Error)]
pub enum SettleError {
    #[error("not found")]
    NotFound,
    #[error("the request was already answered")]
    Settled(Status),
    #[error("send exactly the requested variables: {}", .0.join(", "))]
    EnvMismatch(Vec<String>),
    #[error(transparent)]
    Secret(#[from] SecretError),
}

impl From<sqlx::Error> for SettleError {
    fn from(err: sqlx::Error) -> Self {
        Self::Secret(SecretError::Db(err))
    }
}

/// The result of an owner answer. `changed` is false for a retry of an
/// answer already recorded; nothing was written then.
#[derive(Debug, Clone)]
pub struct Settled {
    pub request: RequestInfo,
    pub changed: bool,
    /// The waiting turn was queued again as a continuation.
    pub requeued: bool,
}

async fn load(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    request_id: Uuid,
) -> Result<RequestInfo, SettleError> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT r.{} FROM secret_requests r JOIN bots b ON b.id = r.bot_id
         WHERE r.id = ?1 AND r.bot_id = ?2 AND b.status = 'active'",
        COLUMNS.replace(", ", ", r.")
    )))
    .bind(request_id)
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?;
    row.map(info).ok_or(SettleError::NotFound)
}

fn answered(request: RequestInfo, wanted: Status) -> Result<Settled, SettleError> {
    if request.status == wanted {
        Ok(Settled {
            request,
            changed: false,
            requeued: false,
        })
    } else {
        Err(SettleError::Settled(request.status))
    }
}

async fn settle_in(
    conn: &mut SqliteConnection,
    request: &RequestInfo,
    status: Status,
) -> Result<Settled, sqlx::Error> {
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE secret_requests SET status = ?2,
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 RETURNING {COLUMNS}"
    )))
    .bind(request.id)
    .bind(if status == Status::Stored {
        "stored"
    } else {
        "cancelled"
    })
    .fetch_one(&mut *conn)
    .await?;
    let requeued = super::turns::continue_waiting(conn, request.turn_id).await?;
    events::turn_changed(conn, request.bot_id, request.turn_id).await?;
    Ok(Settled {
        request: info(row),
        changed: true,
        requeued,
    })
}

/// Store the owner's values for a pending request as a task secret every
/// bot can use, replacing one with the same name, and continue its turn, atomically. `env` must name exactly the requested
/// variables. A retry after success changes nothing and never overwrites
/// what was stored.
pub async fn fulfill(
    pool: &SqlitePool,
    key: Option<&SecretKey>,
    bot_id: Uuid,
    request_id: Uuid,
    env: &BTreeMap<String, String>,
) -> Result<Settled, SettleError> {
    let request = load(&mut *pool.acquire().await?, bot_id, request_id).await?;
    if request.status != Status::Pending {
        return answered(request, Status::Stored);
    }
    if !env.keys().eq(request.env.iter()) {
        return Err(SettleError::EnvMismatch(request.env));
    }
    let key = key.ok_or(SecretError::NoKey)?;
    let sealed = task_secrets::seal_env(key, &request.name, env)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let request = load(&mut tx, bot_id, request_id).await?;
    if request.status != Status::Pending {
        return answered(request, Status::Stored);
    }
    task_secrets::write_in(&mut tx, &request.name, &sealed).await?;
    let settled = settle_in(&mut tx, &request, Status::Stored).await?;
    tx.commit().await?;
    Ok(settled)
}

/// The owner declines a pending request; its turn continues without it.
pub async fn cancel(
    pool: &SqlitePool,
    bot_id: Uuid,
    request_id: Uuid,
) -> Result<Settled, SettleError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let request = load(&mut tx, bot_id, request_id).await?;
    if request.status != Status::Pending {
        return answered(request, Status::Cancelled);
    }
    let settled = settle_in(&mut tx, &request, Status::Cancelled).await?;
    tx.commit().await?;
    Ok(settled)
}

/// Answers of a turn's requests not yet given to an attempt.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    let rows: Vec<(Uuid, String, Json<Vec<String>>, String)> = sqlx::query_as(
        "SELECT id, name, env_names, status FROM secret_requests
         WHERE turn_id = ?1 AND delivered_attempt IS NULL AND status <> 'pending'
         ORDER BY created_at, id",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, env, status)| Outcome {
            id,
            name,
            env: env.0,
            stored: status == "stored",
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
        "UPDATE secret_requests SET delivered_attempt = ?3
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_exact_metadata_arguments_parse() {
        let spec = Spec::parse(&json!({
            "name": "smtp", "reason": " Send the newsletter ", "env": ["SMTP_USER", "SMTP_PASSWORD"],
        }))
        .unwrap();
        assert_eq!(spec.reason, "Send the newsletter");
        assert_eq!(
            spec.env.iter().collect::<Vec<_>>(),
            ["SMTP_PASSWORD", "SMTP_USER"]
        );
        let canary = "CANARY-hunter2";
        for bad in [
            json!({ "name": "smtp", "reason": "r", "env": ["X"], "value": canary }),
            json!({ "name": "smtp", "reason": "r", "env": { "X": canary } }),
            json!({ "name": "smtp", "reason": "r", "env": [canary] }),
            json!({ "name": "smtp", "reason": "r", "env": ["X", "X"] }),
            json!({ "name": "smtp", "reason": "r", "env": [] }),
            json!({ "name": "smtp", "reason": "r", "env": ["PATH"] }),
            json!({ "name": "Smtp", "reason": "r", "env": ["X"] }),
            json!({ "name": "smtp", "reason": " ", "env": ["X"] }),
            json!({ "name": "smtp", "env": ["X"] }),
            json!([canary]),
        ] {
            let err = Spec::parse(&bad).unwrap_err();
            assert!(!err.contains(canary), "{err}");
        }
    }
}
