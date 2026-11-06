//! Webhooks and their wakes.
//!
//! A webhook belongs to its bot's owner conversation and is managed like a
//! routine (see [`super::routines`]): only that bot, from a running turn
//! rooted in an owner message, or the owner. An event (an authenticated
//! trigger call, or a linked background job exiting 0) only sets
//! `requested_at`; events before the wake coalesce into it. The scheduler
//! admits requested webhooks (see [`admit_requested`]) once their pace
//! allows, queueing a turn whose source is a hidden `routine` item, so a
//! wake has a routine run's origin and permissions.

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use super::events;
use super::routines::{By, Caller, RoutineError, authorize_by, authorized_owner_conversation, ts};
use crate::app::providers::WebhookRun;
use crate::app::routines::{RoutineName, RoutinePrompt};
use crate::app::webhooks::{InvalidPace, MAX_PER_BOT, Pace, WINDOW, new_token, token_digest};
use crate::providers::Registry;

/// Requested webhooks admitted per scheduler scan.
pub const ADMIT_BATCH: i64 = 8;
/// How long a wake that could not queue waits before it is retried.
const RETRY: Duration = Duration::minutes(1);

#[derive(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error(
        "a bot manages its own webhooks only for an owner request; routine runs, webhook wakes \
         and tasks they delegated cannot"
    )]
    NotAllowed,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("no such webhook")]
    NotFound,
    #[error("the bot was deleted")]
    BotDeleted,
    #[error("another webhook already has that name; update it or choose another name")]
    NameTaken,
    #[error(
        "this request already created a webhook with that name and different settings; retry \
         with the original settings or use webhook_update"
    )]
    Conflict,
    #[error("a bot can have at most {MAX_PER_BOT} webhooks; delete one first")]
    Limit,
    #[error(
        "the webhook changed while this call waited, so this faster pace is no longer the one \
         the owner approved; call webhook_list and retry"
    )]
    Changed,
    #[error(transparent)]
    Invalid(#[from] InvalidPace),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<RoutineError> for WebhookError {
    fn from(err: RoutineError) -> Self {
        match err {
            RoutineError::NotRunning => Self::NotRunning,
            RoutineError::NotFound => Self::NotFound,
            RoutineError::BotDeleted => Self::BotDeleted,
            RoutineError::Db(err) => Self::Db(err),
            _ => Self::NotAllowed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastWake {
    pub woke_at: DateTime<Utc>,
    /// `queued`, `running`, or the turn's terminal status.
    pub status: String,
    pub turn_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Webhook {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub name: String,
    pub prompt: String,
    pub pace: Pace,
    pub enabled: bool,
    /// An event is waiting for its wake.
    pub requested_at: Option<DateTime<Utc>>,
    /// The pace holds the requested wake until then.
    pub not_before: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted: bool,
    pub last_wake: Option<LastWake>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    bot_id: Uuid,
    name: String,
    prompt: String,
    min_interval_minutes: i64,
    daily_limit: i64,
    enabled: bool,
    requested_at: Option<DateTime<Utc>>,
    not_before: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
    woke_at: Option<DateTime<Utc>>,
    turn_id: Option<Uuid>,
    turn_status: Option<String>,
}

impl From<Row> for Webhook {
    fn from(row: Row) -> Self {
        let last_wake = row
            .woke_at
            .zip(row.turn_id)
            .map(|(woke_at, turn_id)| LastWake {
                woke_at,
                status: match row.turn_status.as_deref() {
                    Some("preparing" | "sending" | "running" | "cancelling" | "waiting") => {
                        "running".to_owned()
                    }
                    Some(other) => other.to_owned(),
                    None => "queued".to_owned(),
                },
                turn_id,
            });
        Self {
            id: row.id,
            bot_id: row.bot_id,
            name: row.name,
            prompt: row.prompt,
            pace: Pace::new(row.min_interval_minutes, row.daily_limit)
                .expect("the schema bounds a stored pace"),
            enabled: row.enabled,
            requested_at: row.requested_at,
            not_before: row.not_before,
            created_at: row.created_at,
            updated_at: row.updated_at,
            deleted: row.deleted_at.is_some(),
            last_wake,
        }
    }
}

const SELECT: &str = "SELECT w.id, w.bot_id, w.name, w.prompt, w.min_interval_minutes,
        w.daily_limit, w.enabled, w.requested_at, w.not_before, w.created_at, w.updated_at,
        w.deleted_at, lr.woke_at, lr.turn_id, t.status AS turn_status
    FROM webhooks w
    LEFT JOIN webhook_runs lr ON lr.id = (SELECT x.id FROM webhook_runs x
                                          WHERE x.webhook_id = w.id ORDER BY x.woke_at DESC LIMIT 1)
    LEFT JOIN turns t ON t.id = lr.turn_id";

/// A bot's current webhooks, oldest first.
pub async fn list(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<Vec<Webhook>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE w.bot_id = ?1 AND w.deleted_at IS NULL ORDER BY w.created_at, w.id"
    )))
    .bind(bot_id)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(Webhook::from).collect())
}

async fn load(conn: &mut SqliteConnection, id: Uuid) -> Result<Webhook, sqlx::Error> {
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE w.id = ?1")))
        .bind(id)
        .fetch_one(conn)
        .await?;
    Ok(row.into())
}

/// One of `bot_id`'s live webhooks.
pub async fn get(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    id: Uuid,
) -> Result<Option<Webhook>, sqlx::Error> {
    let row: Option<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE w.id = ?1 AND w.bot_id = ?2 AND w.deleted_at IS NULL"
    )))
    .bind(id)
    .bind(bot_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(Webhook::from))
}

pub struct NewWebhook<'a> {
    pub name: &'a RoutineName,
    pub prompt: &'a RoutinePrompt,
    pub pace: Pace,
}

fn create_hash(webhook: &NewWebhook<'_>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for part in [
        "webhook",
        webhook.prompt.as_str(),
        &webhook.pace.min_interval_minutes().to_string(),
        &webhook.pace.daily_limit().to_string(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().to_vec()
}

/// Create an enabled webhook. Returns it with its trigger token, which is
/// never stored or shown again. A retry with the same name from the same
/// turn returns the webhook it created, without a token; changed settings
/// conflict.
pub async fn create(
    pool: &SqlitePool,
    caller: Caller,
    webhook: NewWebhook<'_>,
) -> Result<(Webhook, Option<String>), WebhookError> {
    let hash = create_hash(&webhook);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let conversation_id = authorized_owner_conversation(&mut tx, caller).await?;
    let existing: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT id, request_hash FROM webhooks WHERE created_turn_id = ?1 AND created_name = ?2",
    )
    .bind(caller.turn_id)
    .bind(webhook.name.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, saved)) = existing {
        if saved != hash {
            return Err(WebhookError::Conflict);
        }
        let webhook = load(&mut tx, id).await?;
        tx.commit().await?;
        return Ok((webhook, None));
    }
    let (count, taken): (i64, bool) = sqlx::query_as(
        "SELECT count(*), coalesce(max(name = ?2), false) FROM webhooks
         WHERE bot_id = ?1 AND deleted_at IS NULL",
    )
    .bind(caller.bot_id)
    .bind(webhook.name.as_str())
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(WebhookError::NameTaken);
    }
    if count >= MAX_PER_BOT {
        return Err(WebhookError::Limit);
    }
    let id = Uuid::new_v4();
    let (token, digest) = new_token();
    sqlx::query(
        "INSERT INTO webhooks (id, bot_id, conversation_id, name, prompt, token_digest,
             min_interval_minutes, daily_limit, created_turn_id, created_name, request_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?4, ?10)",
    )
    .bind(id)
    .bind(caller.bot_id)
    .bind(conversation_id)
    .bind(webhook.name.as_str())
    .bind(webhook.prompt.as_str())
    .bind(&digest[..])
    .bind(webhook.pace.min_interval_minutes())
    .bind(webhook.pace.daily_limit())
    .bind(caller.turn_id)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    events::bot_updated(&mut tx, caller.bot_id).await?;
    let webhook = load(&mut tx, id).await?;
    tx.commit().await?;
    Ok((webhook, Some(token)))
}

#[derive(Debug, Default)]
pub struct Patch {
    pub name: Option<RoutineName>,
    pub prompt: Option<RoutinePrompt>,
    pub min_interval_minutes: Option<i64>,
    pub daily_limit: Option<i64>,
    /// The pace the owner approved for this call, if it was asked.
    pub approved: Option<Pace>,
    pub enabled: Option<bool>,
    pub rotate_token: bool,
}

/// Change a webhook. Disabling it drops a requested wake; a new pace
/// applies to a requested wake from now. Returns the new token when
/// rotated: the old one stops working at once.
pub async fn update(
    pool: &SqlitePool,
    by: By,
    id: Uuid,
    patch: Patch,
) -> Result<(Webhook, Option<String>), WebhookError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_by(&mut tx, by).await?;
    let Some(current) = get(&mut tx, by.bot_id(), id).await? else {
        return Err(WebhookError::NotFound);
    };
    let name = patch
        .name
        .map_or(current.name.clone(), |n| n.as_str().to_owned());
    if name != current.name {
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM webhooks
                            WHERE bot_id = ?1 AND name = ?2 AND deleted_at IS NULL AND id <> ?3)",
        )
        .bind(by.bot_id())
        .bind(&name)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Err(WebhookError::NameTaken);
        }
    }
    let prompt = patch
        .prompt
        .map_or(current.prompt.clone(), |p| p.as_str().to_owned());
    let pace = Pace::with(current.pace, patch.min_interval_minutes, patch.daily_limit)?;
    if pace.needs_approval_over(current.pace) && patch.approved != Some(pace) {
        return Err(WebhookError::Changed);
    }
    let enabled = patch.enabled.unwrap_or(current.enabled);
    let token = patch.rotate_token.then(new_token);
    let changed = sqlx::query(
        "UPDATE webhooks SET name = ?2, prompt = ?3, min_interval_minutes = ?4, daily_limit = ?5,
             enabled = ?6, token_digest = coalesce(?7, token_digest),
             requested_at = CASE WHEN ?6 THEN requested_at END, not_before = NULL,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND (name IS NOT ?2 OR prompt IS NOT ?3 OR min_interval_minutes IS NOT ?4
             OR daily_limit IS NOT ?5 OR enabled IS NOT ?6 OR ?7 IS NOT NULL)",
    )
    .bind(id)
    .bind(&name)
    .bind(&prompt)
    .bind(pace.min_interval_minutes())
    .bind(pace.daily_limit())
    .bind(enabled)
    .bind(token.as_ref().map(|(_, digest)| &digest[..]))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed > 0 {
        events::bot_updated(&mut tx, by.bot_id()).await?;
    }
    let webhook = load(&mut tx, id).await?;
    tx.commit().await?;
    Ok((webhook, token.map(|(token, _)| token)))
}

/// Delete a webhook: its token stops working, a requested wake is dropped,
/// and a wake already queued still runs. Returns whether this call deleted it.
pub async fn delete(pool: &SqlitePool, by: By, id: Uuid) -> Result<bool, WebhookError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_by(&mut tx, by).await?;
    let deleted: Option<bool> = sqlx::query_scalar(
        "SELECT deleted_at IS NOT NULL FROM webhooks WHERE id = ?1 AND bot_id = ?2",
    )
    .bind(id)
    .bind(by.bot_id())
    .fetch_optional(&mut *tx)
    .await?;
    match deleted {
        None => Err(WebhookError::NotFound),
        Some(true) => Ok(false),
        Some(false) => {
            sqlx::query(
                "UPDATE webhooks SET deleted_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                     requested_at = NULL, not_before = NULL,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            events::bot_updated(&mut tx, by.bot_id()).await?;
            tx.commit().await?;
            Ok(true)
        }
    }
}

/// What a trigger call achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fired {
    /// This event requested a wake.
    Requested {
        wakes_at: DateTime<Utc>,
    },
    /// A wake was already requested; this event coalesced into it.
    Coalesced {
        wakes_at: DateTime<Utc>,
    },
    Disabled,
}

#[derive(Debug, thiserror::Error)]
pub enum FireError {
    /// No live webhook has this ID and token.
    #[error("unknown webhook or token")]
    Unauthorized,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Request a wake for webhook `id`, authenticated by its trigger token.
pub async fn fire(
    pool: &SqlitePool,
    id: Uuid,
    token: &str,
    now: DateTime<Utc>,
) -> Result<Fired, FireError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(Vec<u8>, Uuid, bool, bool)> = sqlx::query_as(
        "SELECT w.token_digest, w.bot_id, w.enabled, w.requested_at IS NOT NULL
         FROM webhooks w JOIN bots b ON b.id = w.bot_id
         WHERE w.id = ?1 AND w.deleted_at IS NULL AND b.status = 'active'",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((digest, bot_id, enabled, requested)) = row else {
        return Err(FireError::Unauthorized);
    };
    if !bool::from(digest.as_slice().ct_eq(&token_digest(token)[..])) {
        return Err(FireError::Unauthorized);
    }
    if !enabled {
        return Ok(Fired::Disabled);
    }
    if !requested {
        sqlx::query("UPDATE webhooks SET requested_at = ?2 WHERE id = ?1")
            .bind(id)
            .bind(ts(now))
            .execute(&mut *tx)
            .await?;
        events::bot_updated(&mut tx, bot_id).await?;
    }
    let wakes_at = next_wake(&mut tx, id, now).await?;
    tx.commit().await?;
    Ok(if requested {
        Fired::Coalesced { wakes_at }
    } else {
        Fired::Requested { wakes_at }
    })
}

/// The earliest a requested wake of `id` may run: its pace, counted from
/// the last wake's admission and finish, and any retry already scheduled.
async fn next_wake(
    conn: &mut SqliteConnection,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>, sqlx::Error> {
    let (min_interval, daily, not_before): (i64, i64, Option<DateTime<Utc>>) = sqlx::query_as(
        "SELECT min_interval_minutes, daily_limit, not_before FROM webhooks WHERE id = ?1",
    )
    .bind(id)
    .fetch_one(&mut *conn)
    .await?;
    let wakes: Vec<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT woke_at FROM webhook_runs WHERE webhook_id = ?1 AND woke_at > ?2 ORDER BY woke_at",
    )
    .bind(id)
    .bind(ts(now - WINDOW))
    .fetch_all(&mut *conn)
    .await?;
    let last_finished: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT t.finished_at FROM webhook_runs r JOIN turns t ON t.id = r.turn_id
         WHERE r.webhook_id = ?1 ORDER BY r.woke_at DESC LIMIT 1",
    )
    .bind(id)
    .fetch_optional(&mut *conn)
    .await?
    .flatten();
    let pace = Pace::new(min_interval, daily).expect("the schema bounds a stored pace");
    Ok(pace
        .next_allowed(&wakes, last_finished, now)
        .max(not_before.unwrap_or(now)))
}

/// The webhook wake a turn executes, if it is one.
pub async fn run_of(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Option<WebhookRun>, sqlx::Error> {
    let row: Option<(Uuid, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT webhook_id, name, requested_at FROM webhook_runs WHERE turn_id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(|(webhook_id, name, requested_at)| WebhookRun {
        webhook_id,
        name,
        requested_at,
    }))
}

const UNFINISHED: &str = "EXISTS (SELECT 1 FROM webhook_runs r JOIN turns t ON t.id = r.turn_id
    WHERE r.webhook_id = w.id AND t.status NOT IN ('completed', 'interrupted', 'failed', 'uncertain'))";

/// Admit up to `limit` requested webhooks whose wake may run at `now`.
/// Returns how many turns were queued. The caller holds a maintenance
/// admission.
pub async fn admit_requested(
    pool: &SqlitePool,
    registry: &Registry,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let due: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT w.id FROM webhooks w JOIN bots b ON b.id = w.bot_id
         WHERE w.requested_at IS NOT NULL AND (w.not_before IS NULL OR w.not_before <= ?1)
           AND b.status = 'active' AND NOT b.cancel_pending AND NOT {UNFINISHED}
         ORDER BY w.requested_at, w.id LIMIT ?2"
    )))
    .bind(ts(now))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut queued = 0;
    for id in due {
        if admit(pool, registry, id, now).await? {
            queued += 1;
        }
    }
    Ok(queued)
}

#[derive(sqlx::FromRow)]
struct Requested {
    bot_id: Uuid,
    conversation_id: Uuid,
    name: String,
    prompt: String,
    requested_at: String,
}

/// Queue one requested wake, or record when it may run. A wake that cannot
/// queue (the bot's queue is full, no provider) stays requested and is
/// retried after [`RETRY`]: an event is never dropped, only delayed.
async fn admit(
    pool: &SqlitePool,
    registry: &Registry,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<Requested> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT w.bot_id, w.conversation_id, w.name, w.prompt, w.requested_at
         FROM webhooks w JOIN bots b ON b.id = w.bot_id
         WHERE w.id = ?1 AND w.requested_at IS NOT NULL
           AND (w.not_before IS NULL OR w.not_before <= ?2)
           AND b.status = 'active' AND NOT b.cancel_pending AND NOT {UNFINISHED}"
    )))
    .bind(id)
    .bind(ts(now))
    .fetch_optional(&mut *tx)
    .await?;
    let Some(Requested {
        bot_id,
        conversation_id,
        name,
        prompt,
        requested_at,
    }) = row
    else {
        return Ok(false);
    };
    sqlx::query("UPDATE webhooks SET not_before = NULL WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let allowed = next_wake(&mut tx, id, now).await?;
    let hold = |at: DateTime<Utc>| {
        sqlx::query("UPDATE webhooks SET not_before = ?2 WHERE id = ?1")
            .bind(id)
            .bind(ts(at))
    };
    if allowed > now {
        hold(allowed).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(false);
    }
    let run_id = Uuid::new_v4();
    let queued = super::routines::queue_saved_task(
        &mut tx,
        registry,
        bot_id,
        conversation_id,
        run_id,
        b"webhook-run\0",
        &prompt,
        super::routines::RunOn::Bot,
    )
    .await?;
    let Ok(turn_id) = queued else {
        hold(now + RETRY).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(false);
    };
    sqlx::query(
        "INSERT INTO webhook_runs (id, webhook_id, name, requested_at, woke_at, turn_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(run_id)
    .bind(id)
    .bind(&name)
    .bind(&requested_at)
    .bind(ts(now))
    .bind(turn_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE webhooks SET requested_at = NULL, not_before = NULL WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    events::turn_accepted(&mut tx, bot_id, turn_id).await?;
    events::bot_updated(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(true)
}

/// Whether `turn_id` already created a webhook named `name`: a retry of
/// that call returns it without asking the owner again.
pub async fn created_by(
    pool: &SqlitePool,
    turn_id: Uuid,
    name: &RoutineName,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM webhooks WHERE created_turn_id = ?1 AND created_name = ?2)",
    )
    .bind(turn_id)
    .bind(name.as_str())
    .fetch_one(pool)
    .await
}

/// Whether `bot_id` has a live webhook `id`, for linking a job to it.
pub async fn is_live(pool: &SqlitePool, bot_id: Uuid, id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM webhooks WHERE id = ?1 AND bot_id = ?2 AND deleted_at IS NULL)",
    )
    .bind(id)
    .bind(bot_id)
    .fetch_one(pool)
    .await
}
