//! Routines and their runs.
//!
//! A routine belongs to its bot's owner conversation. Only that bot, from a
//! running turn whose work is rooted in an owner message (in its owner
//! conversation, or a task another bot delegated for one), may manage it:
//! every mutation rechecks the turn inside its own immediate transaction,
//! so a routine run, a task delegated by one, or a stopped turn is refused
//! even with a grant. The owner may switch a routine on or off, or delete it.
//!
//! The scheduler admits due routines (see [`admit_due`]). Each admission is
//! one transaction that records the occurrence in `routine_runs`, advances
//! `next_due_at` past now (missed occurrences coalesce into this one run),
//! and either queues a turn whose source is a hidden `routine` item or
//! records why it was skipped. Run status is the turn's own status.
//!
//! A routine either inherits the bot's live model or has its own. A run on
//! its own model snapshots that profile into its turn, leaves the bot's
//! profile alone, and keeps its native sessions in a lane of its own, so it
//! neither resets the bot's chat session nor triggers provider handoffs.

use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{bot_profiles, events};
use crate::app::messages::MAX_QUEUED_TURNS;
use crate::app::providers::{Availability, Origin, Profile, RoutineRun};
use crate::app::routines::{
    InvalidRoutine, MAX_PER_BOT, ModelSetting, RoutineModel, RoutineName, RoutinePrompt, Schedule,
};
use crate::providers::Registry;

/// Due routines admitted per scheduler scan.
pub const ADMIT_BATCH: i64 = 8;

#[derive(Debug, thiserror::Error)]
pub enum RoutineError {
    #[error(
        "a bot manages its own routines only for an owner request; routine runs and tasks they \
         delegated cannot"
    )]
    NotAllowed,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("no such routine")]
    NotFound,
    #[error("the bot was deleted")]
    BotDeleted,
    #[error("another routine already has that name; update it or choose another name")]
    NameTaken,
    #[error(
        "this request already created a routine with that name and different settings; retry \
         with the original settings or use routine_update"
    )]
    Conflict,
    #[error("a bot can have at most {MAX_PER_BOT} routines; delete one first")]
    Limit,
    #[error(transparent)]
    Invalid(#[from] InvalidRoutine),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The turn a routine tool call comes from, as its grant records it.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub turn_id: Uuid,
}

#[derive(Debug, Clone, Copy)]
pub enum By {
    Turn(Caller),
    Owner { bot_id: Uuid },
}

impl By {
    pub(crate) fn bot_id(&self) -> Uuid {
        match self {
            By::Turn(caller) => caller.bot_id,
            By::Owner { bot_id } => *bot_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastRun {
    pub due_at: DateTime<Utc>,
    /// `queued`, `running`, `skipped`, or the turn's terminal status.
    pub status: String,
    pub skip_reason: Option<String>,
    pub turn_id: Option<Uuid>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routine {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub name: String,
    pub prompt: String,
    pub prompt_revision: i64,
    pub cron: String,
    pub timezone: String,
    pub enabled: bool,
    pub model: ModelSetting,
    pub next_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted: bool,
    pub last_run: Option<LastRun>,
    /// Consecutive failed occurrences since the last success, re-enable, or
    /// dismissal.
    pub failed_runs: i64,
    /// Why else.bot switched the routine off, if it did.
    pub paused_reason: Option<String>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    bot_id: Uuid,
    name: String,
    prompt: String,
    prompt_revision: i64,
    cron: String,
    timezone: String,
    enabled: bool,
    model: Option<Json<RoutineModel>>,
    requested_model: Option<String>,
    next_due_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
    failed_runs: i64,
    paused_reason: Option<String>,
    run_due_at: Option<DateTime<Utc>>,
    skip_reason: Option<String>,
    turn_id: Option<Uuid>,
    turn_status: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    finished_at: Option<DateTime<Utc>>,
}

impl From<Row> for Routine {
    fn from(row: Row) -> Self {
        let last_run = row.run_due_at.map(|due_at| LastRun {
            due_at,
            status: match row.turn_status.as_deref() {
                None => "skipped".to_owned(),
                Some("queued") => "queued".to_owned(),
                Some("preparing" | "sending" | "running" | "cancelling" | "waiting") => {
                    "running".to_owned()
                }
                Some(other) => other.to_owned(),
            },
            skip_reason: row.skip_reason,
            turn_id: row.turn_id,
            error_code: row.error_code,
            error_message: row.error_message,
            finished_at: row.finished_at,
        });
        Self {
            id: row.id,
            bot_id: row.bot_id,
            name: row.name,
            prompt: row.prompt,
            prompt_revision: row.prompt_revision,
            cron: row.cron,
            timezone: row.timezone,
            enabled: row.enabled,
            model: setting(row.model, row.requested_model),
            next_run_at: row.next_due_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            deleted: row.deleted_at.is_some(),
            last_run,
            failed_runs: row.failed_runs,
            paused_reason: row.paused_reason,
        }
    }
}

fn setting(model: Option<Json<RoutineModel>>, requested: Option<String>) -> ModelSetting {
    match model {
        Some(Json(model)) => ModelSetting::Own(model),
        None => ModelSetting::Inherit { requested },
    }
}

fn columns(model: &ModelSetting) -> (Option<Json<&RoutineModel>>, Option<&str>) {
    match model {
        ModelSetting::Own(model) => (Some(Json(model)), None),
        ModelSetting::Inherit { requested } => (None, requested.as_deref()),
    }
}

const SELECT: &str =
    "SELECT r.id, r.bot_id, r.name, r.prompt, r.prompt_revision, r.cron, r.timezone, r.enabled,
        r.model, r.requested_model, r.next_due_at, r.created_at, r.updated_at, r.deleted_at,
        r.failed_runs, r.paused_reason, lr.due_at AS run_due_at, lr.skip_reason, lr.turn_id, t.status AS turn_status,
        t.error_code, t.error_message, t.finished_at
    FROM routines r
    LEFT JOIN routine_runs lr ON lr.id = (SELECT x.id FROM routine_runs x
                                          WHERE x.routine_id = r.id ORDER BY x.due_at DESC LIMIT 1)
    LEFT JOIN turns t ON t.id = lr.turn_id";

/// The same text form SQLite's own timestamps use, so stored instants
/// compare correctly as text.
pub(crate) fn ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, false)
}

/// A bot's current routines, oldest first.
pub async fn list(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<Vec<Routine>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE r.bot_id = ?1 AND r.deleted_at IS NULL ORDER BY r.created_at, r.id"
    )))
    .bind(bot_id)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(Routine::from).collect())
}

async fn load(conn: &mut SqliteConnection, id: Uuid) -> Result<Routine, sqlx::Error> {
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE r.id = ?1")))
        .bind(id)
        .fetch_one(conn)
        .await?;
    Ok(row.into())
}

pub(crate) async fn authorized_owner_conversation(
    conn: &mut SqliteConnection,
    caller: Caller,
) -> Result<Uuid, RoutineError> {
    let row: Option<(Uuid, Uuid, String, String, Uuid)> = sqlx::query_as(
        "SELECT t.bot_id, t.conversation_id, t.status, b.status, o.id
         FROM turns t JOIN bots b ON b.id = t.bot_id
         JOIN conversations o ON o.bot_id = t.bot_id AND o.kind = 'owner'
         WHERE t.id = ?1",
    )
    .bind(caller.turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((bot, conversation, status, bot_status, owner_conversation)) = row else {
        return Err(RoutineError::NotRunning);
    };
    if bot != caller.bot_id || conversation != caller.conversation_id {
        return Err(RoutineError::NotAllowed);
    }
    if super::delegations::origin(conn, caller.turn_id).await? != Origin::OwnerRequest {
        return Err(RoutineError::NotAllowed);
    }
    if !matches!(status.as_str(), "sending" | "running") || bot_status != "active" {
        return Err(RoutineError::NotRunning);
    }
    Ok(owner_conversation)
}

pub(crate) async fn authorize_by(conn: &mut SqliteConnection, by: By) -> Result<(), RoutineError> {
    match by {
        By::Turn(caller) => authorized_owner_conversation(conn, caller).await.map(drop),
        By::Owner { bot_id } => {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
                    .bind(bot_id)
                    .fetch_optional(&mut *conn)
                    .await?;
            match status.as_deref() {
                None => Err(RoutineError::NotFound),
                Some("active") => Ok(()),
                Some(_) => Err(RoutineError::BotDeleted),
            }
        }
    }
}

pub struct NewRoutine<'a> {
    pub name: &'a RoutineName,
    pub prompt: &'a RoutinePrompt,
    pub schedule: &'a Schedule,
    pub model: &'a ModelSetting,
    /// The model arguments as the bot sent them, if any: a retry with the
    /// same request returns the same routine even if resolving would now
    /// differ.
    pub model_request: Option<&'a str>,
}

fn create_hash(routine: &NewRoutine<'_>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for part in [
        "routine",
        routine.prompt.as_str(),
        routine.schedule.cron(),
        routine.schedule.timezone(),
    ]
    .into_iter()
    .chain(routine.model_request)
    {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().to_vec()
}

/// Create a routine, enabled, first due at its next occurrence after `now`.
/// A retry with the same name from the same turn returns the routine it
/// created (as it is now); changed settings conflict. Returns whether it
/// was created by this call.
pub async fn create(
    pool: &SqlitePool,
    caller: Caller,
    routine: NewRoutine<'_>,
    now: DateTime<Utc>,
) -> Result<(Routine, bool), RoutineError> {
    let hash = create_hash(&routine);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let conversation_id = authorized_owner_conversation(&mut tx, caller).await?;
    let existing: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT id, request_hash FROM routines WHERE created_turn_id = ?1 AND created_name = ?2",
    )
    .bind(caller.turn_id)
    .bind(routine.name.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, saved)) = existing {
        if saved != hash {
            return Err(RoutineError::Conflict);
        }
        let routine = load(&mut tx, id).await?;
        tx.commit().await?;
        return Ok((routine, false));
    }
    let (count, taken): (i64, bool) = sqlx::query_as(
        "SELECT count(*), coalesce(max(name = ?2), false) FROM routines
         WHERE bot_id = ?1 AND deleted_at IS NULL",
    )
    .bind(caller.bot_id)
    .bind(routine.name.as_str())
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(RoutineError::NameTaken);
    }
    if count >= MAX_PER_BOT {
        return Err(RoutineError::Limit);
    }
    let id = Uuid::new_v4();
    let (model, requested_model) = columns(routine.model);
    sqlx::query(
        "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
             next_due_at, created_turn_id, created_name, request_hash, model, requested_model)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?4, ?10, ?11, ?12)",
    )
    .bind(id)
    .bind(caller.bot_id)
    .bind(conversation_id)
    .bind(routine.name.as_str())
    .bind(routine.prompt.as_str())
    .bind(routine.schedule.cron())
    .bind(routine.schedule.timezone())
    .bind(routine.schedule.next_after(now).map(ts))
    .bind(caller.turn_id)
    .bind(&hash)
    .bind(model)
    .bind(requested_model)
    .execute(&mut *tx)
    .await?;
    events::bot_updated(&mut tx, caller.bot_id).await?;
    let created = load(&mut tx, id).await?;
    tx.commit().await?;
    Ok((created, true))
}

/// Absolute values to set; `None` keeps the current one.
#[derive(Debug, Default)]
pub struct Patch {
    pub name: Option<RoutineName>,
    pub prompt: Option<RoutinePrompt>,
    pub cron: Option<String>,
    pub timezone: Option<String>,
    pub enabled: Option<bool>,
    pub model: Option<ModelSetting>,
    /// Forget the routine's failed runs (the owner saw them).
    pub dismiss_failures: bool,
}

#[derive(sqlx::FromRow)]
struct RoutineSettings {
    name: String,
    prompt: String,
    cron: String,
    timezone: String,
    enabled: bool,
    next_due_at: Option<String>,
    model: Option<Json<RoutineModel>>,
    requested_model: Option<String>,
}

/// Apply `patch`. Setting values a routine already has changes nothing, so
/// a retry never postpones its schedule. A new prompt is a new prompt
/// revision, which ends the routine's destructive grant. Disabling stops
/// future runs (one already admitted finishes); enabling or a new schedule
/// counts from `now`, and forgets earlier failed runs.
pub async fn update(
    pool: &SqlitePool,
    by: By,
    id: Uuid,
    patch: Patch,
    now: DateTime<Utc>,
) -> Result<Routine, RoutineError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_by(&mut tx, by).await?;
    let current: Option<RoutineSettings> = sqlx::query_as(
        "SELECT name, prompt, cron, timezone, enabled, next_due_at, model, requested_model
         FROM routines WHERE id = ?1 AND bot_id = ?2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(by.bot_id())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(RoutineSettings {
        name,
        prompt,
        cron,
        timezone,
        enabled,
        next_due_at: next_due,
        model,
        requested_model: requested,
    }) = current
    else {
        return Err(RoutineError::NotFound);
    };
    let schedule = Schedule::parse_due(
        patch.cron.as_deref().unwrap_or(&cron),
        Some(patch.timezone.as_deref().unwrap_or(&timezone)),
        now,
    )?;
    let new_name = patch.name.map_or(name.clone(), |n| n.as_str().to_owned());
    let new_prompt = patch
        .prompt
        .map_or(prompt.clone(), |p| p.as_str().to_owned());
    let new_enabled = patch.enabled.unwrap_or(enabled);
    let new_model = patch.model.unwrap_or_else(|| setting(model, requested));
    let (model, requested_model) = columns(&new_model);
    let rescheduled = schedule.cron() != cron || schedule.timezone() != timezone;
    let next_due = if !new_enabled {
        None
    } else if rescheduled || !enabled {
        schedule.next_after(now).map(ts)
    } else {
        next_due
    };
    if new_name != name {
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM routines
                            WHERE bot_id = ?1 AND name = ?2 AND deleted_at IS NULL AND id <> ?3)",
        )
        .bind(by.bot_id())
        .bind(&new_name)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Err(RoutineError::NameTaken);
        }
    }
    let changed = sqlx::query(
        "UPDATE routines SET name = ?2, prompt = ?3, cron = ?4, timezone = ?5, enabled = ?6,
             next_due_at = ?7, prompt_revision = prompt_revision + (prompt IS NOT ?3),
             model = ?8, requested_model = ?9,
             failed_runs = CASE WHEN ?10 OR (?6 AND NOT enabled) THEN 0 ELSE failed_runs END,
             paused_reason = CASE WHEN ?10 OR ?6 THEN NULL ELSE paused_reason END,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND (name IS NOT ?2 OR prompt IS NOT ?3 OR cron IS NOT ?4
             OR timezone IS NOT ?5 OR enabled IS NOT ?6 OR next_due_at IS NOT ?7
             OR model IS NOT ?8 OR requested_model IS NOT ?9
             OR (?10 AND (failed_runs <> 0 OR paused_reason IS NOT NULL)))",
    )
    .bind(id)
    .bind(&new_name)
    .bind(&new_prompt)
    .bind(schedule.cron())
    .bind(schedule.timezone())
    .bind(new_enabled)
    .bind(&next_due)
    .bind(model)
    .bind(requested_model)
    .bind(patch.dismiss_failures)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed > 0 {
        if let By::Turn(caller) = by {
            let (conversation, seq): (Uuid, i64) = sqlx::query_as(
                "UPDATE conversations SET last_item_seq = last_item_seq + 1
                 WHERE bot_id = ?1 AND kind = 'owner' RETURNING id, last_item_seq",
            )
            .bind(caller.bot_id)
            .fetch_one(&mut *tx)
            .await?;
            let item_id = Uuid::new_v4();
            let update = super::turns::RoutineUpdate {
                routine_id: id,
                name: new_name.clone(),
            };
            sqlx::query(
                "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text, routine_update)
                 VALUES (?1, ?2, ?3, ?4, 'status', ?5, ?6)",
            )
            .bind(item_id)
            .bind(conversation)
            .bind(seq)
            .bind(caller.turn_id)
            .bind(format!("Updated routine: {new_name}"))
            .bind(sqlx::types::Json(update))
            .execute(&mut *tx)
            .await?;
            events::item_upsert(&mut tx, caller.bot_id, item_id).await?;
        }
        events::bot_updated(&mut tx, by.bot_id()).await?;
    }
    let routine = load(&mut tx, id).await?;
    tx.commit().await?;
    Ok(routine)
}

/// Delete a routine: it never runs again and leaves the list; a run already
/// admitted finishes. Deleting it again returns `false`.
pub async fn delete(pool: &SqlitePool, by: By, id: Uuid) -> Result<bool, RoutineError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_by(&mut tx, by).await?;
    let deleted: Option<bool> = sqlx::query_scalar(
        "SELECT deleted_at IS NOT NULL FROM routines WHERE id = ?1 AND bot_id = ?2",
    )
    .bind(id)
    .bind(by.bot_id())
    .fetch_optional(&mut *tx)
    .await?;
    match deleted {
        None => Err(RoutineError::NotFound),
        Some(true) => Ok(false),
        Some(false) => {
            sqlx::query(
                "UPDATE routines SET enabled = false, next_due_at = NULL,
                     deleted_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
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

/// The routine run a turn executes, if it is one.
pub async fn run_of(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Option<RoutineRun>, sqlx::Error> {
    let row: Option<(Uuid, String, DateTime<Utc>)> =
        sqlx::query_as("SELECT routine_id, name, due_at FROM routine_runs WHERE turn_id = ?1")
            .bind(turn_id)
            .fetch_optional(conn)
            .await?;
    Ok(row.map(|(routine_id, name, due_at)| RoutineRun {
        routine_id,
        name,
        due_at,
    }))
}

/// Ended routine runs reviewed per scheduler scan.
pub const REVIEW_BATCH: i64 = 8;

/// Handle up to `limit` routine runs whose turns have ended, per
/// [`routine_failures::react`]: forget failures after a success, retry a
/// transient failure once, or count the failure and tell the owner in the
/// failed turn, pausing the routine when retrying cannot help or its runs
/// keep failing. Each run is reviewed once, in one transaction with all it
/// causes, whichever path ended its turn. Returns how many retries were
/// queued. The caller holds a maintenance admission.
pub async fn review_ended(
    pool: &SqlitePool,
    registry: &Registry,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let ended: Vec<Uuid> = sqlx::query_scalar(
        "SELECT rr.id FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
         WHERE NOT rr.reviewed AND rr.turn_id IS NOT NULL
           AND t.status IN ('completed', 'interrupted', 'failed', 'uncertain')
         ORDER BY rr.due_at, rr.id LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut retried = 0;
    for id in ended {
        if review(pool, registry, id, now).await? {
            retried += 1;
        }
    }
    Ok(retried)
}

#[derive(sqlx::FromRow)]
struct EndedRun {
    routine_id: Uuid,
    turn_id: Uuid,
    due_at: DateTime<Utc>,
    retry: bool,
    status: String,
    error_code: Option<String>,
    error_message: Option<String>,
    bot_id: Uuid,
    conversation_id: Uuid,
    name: String,
    prompt: String,
    prompt_revision: i64,
    timezone: String,
    enabled: bool,
    next_due_at: Option<DateTime<Utc>>,
    model: Option<Json<RoutineModel>>,
    failed_runs: i64,
    live: bool,
    may_retry: bool,
}

async fn review(
    pool: &SqlitePool,
    registry: &Registry,
    run_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    use crate::app::routine_failures::{self, Ended, Notice, Pause, Reaction, Run};
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<EndedRun> = sqlx::query_as(
        "SELECT rr.routine_id, rr.turn_id, rr.due_at, rr.retry_of IS NOT NULL AS retry,
                t.status, t.error_code, t.error_message,
                r.bot_id, r.conversation_id, r.name, r.prompt, r.prompt_revision, r.timezone,
                r.enabled, r.next_due_at, r.model, r.failed_runs,
                r.deleted_at IS NULL AND b.status = 'active' AS live,
                NOT b.cancel_pending AS may_retry
         FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
         JOIN routines r ON r.id = rr.routine_id JOIN bots b ON b.id = r.bot_id
         WHERE rr.id = ?1 AND NOT rr.reviewed",
    )
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(run) = row else {
        return Ok(false);
    };
    let Some(ended) = Ended::of(&run.status, run.error_code.as_deref()) else {
        return Ok(false);
    };
    sqlx::query("UPDATE routine_runs SET reviewed = true WHERE id = ?1")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
    if !run.live {
        tx.commit().await?;
        return Ok(false);
    }
    let state = Run {
        retry: run.retry,
        enabled: run.enabled,
        failed_runs: run.failed_runs,
    };
    let mut reaction = routine_failures::react(ended, state);
    if reaction == Reaction::Retry {
        let retry_id = Uuid::new_v4();
        let run_on = match &run.model {
            Some(Json(own)) => RunOn::Own(own, run.routine_id),
            None => RunOn::Bot,
        };
        let queued = if run.may_retry {
            queue_saved_task(
                &mut tx,
                registry,
                run.bot_id,
                run.conversation_id,
                retry_id,
                b"routine-retry\0",
                &run.prompt,
                run_on,
            )
            .await?
            .ok()
        } else {
            None
        };
        if let Some(turn_id) = queued {
            // Later than the run it repeats, so it is the routine's last run.
            let due = now.max(run.due_at + chrono::Duration::milliseconds(1));
            sqlx::query(
                "INSERT INTO routine_runs
                     (id, routine_id, due_at, name, turn_id, prompt_revision, retry_of)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )
            .bind(retry_id)
            .bind(run.routine_id)
            .bind(ts(due))
            .bind(&run.name)
            .bind(turn_id)
            .bind(run.prompt_revision)
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
            events::turn_accepted(&mut tx, run.bot_id, turn_id).await?;
            tx.commit().await?;
            return Ok(true);
        }
        reaction = routine_failures::react(
            ended,
            Run {
                retry: true,
                ..state
            },
        );
    }
    let paused = match reaction {
        Reaction::Retry | Reaction::Ignore => None,
        Reaction::Reset => {
            let reset = sqlx::query(
                "UPDATE routines SET failed_runs = 0, paused_reason = NULL
                 WHERE id = ?1 AND (failed_runs <> 0 OR paused_reason IS NOT NULL)",
            )
            .bind(run.routine_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if reset > 0 {
                events::bot_updated(&mut tx, run.bot_id).await?;
            }
            None
        }
        Reaction::Report => Some(None),
        Reaction::Pause(Pause::NeedsAction) => Some(Some("needs_action")),
        Reaction::Pause(Pause::Repeated) => Some(Some("repeated")),
    };
    if let Some(paused) = paused {
        let failed_runs = run.failed_runs + 1;
        let enabled = run.enabled && paused.is_none();
        sqlx::query(
            "UPDATE routines SET failed_runs = ?2, paused_reason = coalesce(?3, paused_reason),
                 enabled = ?4, next_due_at = CASE WHEN ?4 THEN next_due_at END,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1",
        )
        .bind(run.routine_id)
        .bind(failed_runs)
        .bind(paused)
        .bind(enabled)
        .execute(&mut *tx)
        .await?;
        let text = routine_failures::notice(
            &Notice {
                name: &run.name,
                ended,
                message: run.error_message.as_deref(),
                retried: run.retry,
                enabled,
                failed_runs,
                next: run
                    .next_due_at
                    .map(|at| routine_failures::local_time(at, &run.timezone)),
            },
            reaction,
        );
        let (seq,): (i64,) = sqlx::query_as(
            "UPDATE conversations SET last_item_seq = last_item_seq + 1 WHERE id = ?1
             RETURNING last_item_seq",
        )
        .bind(run.conversation_id)
        .fetch_one(&mut *tx)
        .await?;
        let item_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
             VALUES (?1, ?2, ?3, ?4, 'assistant', ?5)",
        )
        .bind(item_id)
        .bind(run.conversation_id)
        .bind(seq)
        .bind(run.turn_id)
        .bind(&text)
        .execute(&mut *tx)
        .await?;
        events::item_upsert(&mut tx, run.bot_id, item_id).await?;
        events::bot_updated(&mut tx, run.bot_id).await?;
    }
    tx.commit().await?;
    Ok(false)
}

/// Admit up to `limit` routines due at `now`. Returns how many turns were
/// queued. The caller holds a maintenance admission.
pub async fn admit_due(
    pool: &SqlitePool,
    registry: &Registry,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT r.id FROM routines r JOIN bots b ON b.id = r.bot_id
         WHERE r.next_due_at IS NOT NULL AND r.next_due_at <= ?1
           AND b.status = 'active' AND NOT b.cancel_pending
         ORDER BY r.next_due_at, r.id LIMIT ?2",
    )
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

/// Why a due run queued no turn.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Skip {
    Overlap,
    QueueFull,
    ProviderUnavailable,
}

impl Skip {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Skip::Overlap => "overlap",
            Skip::QueueFull => "queue_full",
            Skip::ProviderUnavailable => "provider_unavailable",
        }
    }
}

#[derive(sqlx::FromRow)]
struct Due {
    bot_id: Uuid,
    conversation_id: Uuid,
    name: String,
    prompt: String,
    prompt_revision: i64,
    cron: String,
    timezone: String,
    model: Option<Json<RoutineModel>>,
    due_at: String,
}

struct Chosen {
    provider_id: &'static str,
    profile: Option<(i64, Json<Profile>)>,
    lane: String,
}

/// A run on the routine's own model, if its provider can run it now. An
/// unavailable choice skips the run rather than switching models; the
/// profile revision 0 marks a routine's own profile, not a bot revision.
fn own_model(registry: &Registry, own: &RoutineModel, routine_id: Uuid) -> Option<Chosen> {
    let provider = registry
        .get(&own.provider_id)
        .filter(|p| p.uses_profile() && p.availability() == Availability::Available)?;
    provider.validate_profile(&own.profile).ok()?;
    Some(Chosen {
        provider_id: provider.id(),
        profile: Some((0, Json(own.profile.clone()))),
        lane: format!("routine:{routine_id}"),
    })
}

/// Where a saved task's turn runs.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RunOn<'a> {
    /// The bot's selected provider and profile.
    Bot,
    /// A routine's own model, in that routine's session lane.
    Own(&'a RoutineModel, Uuid),
}

/// Queue a turn in `conversation_id` whose source is a hidden `routine`
/// item holding `prompt`: a saved task the bot runs, never an owner
/// message. `request_id` names the run; `tag` separates run kinds in the
/// turn's request hash. Skips when the bot's queue is full or no provider
/// can run it. The caller holds an immediate transaction, records its run,
/// then emits `turn_accepted` (its snapshot includes the run).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn queue_saved_task(
    conn: &mut SqliteConnection,
    registry: &Registry,
    bot_id: Uuid,
    conversation_id: Uuid,
    request_id: Uuid,
    tag: &[u8],
    prompt: &str,
    run_on: RunOn<'_>,
) -> Result<Result<Uuid, Skip>, sqlx::Error> {
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turns WHERE bot_id = ?1 AND status = 'queued'")
            .bind(bot_id)
            .fetch_one(&mut *conn)
            .await?;
    if queued >= MAX_QUEUED_TURNS {
        return Ok(Err(Skip::QueueFull));
    }
    let chosen = match run_on {
        RunOn::Own(own, routine_id) => own_model(registry, own, routine_id),
        RunOn::Bot => bot_profiles::for_admission(&mut *conn, registry, bot_id)
            .await?
            .map(|chosen| Chosen {
                provider_id: chosen.provider_id,
                profile: chosen.profile,
                lane: String::new(),
            }),
    };
    let Some(Chosen {
        provider_id,
        profile,
        lane,
    }) = chosen
    else {
        return Ok(Err(Skip::ProviderUnavailable));
    };
    let (last_turn_no, last_item_seq): (i64, i64) =
        sqlx::query_as("SELECT last_turn_no, last_item_seq FROM conversations WHERE id = ?1")
            .bind(conversation_id)
            .fetch_one(&mut *conn)
            .await?;
    let turn_id = Uuid::new_v4();
    let item_id = Uuid::new_v4();
    let turn_no = last_turn_no + 1;
    let seq = last_item_seq + 1;
    let mut hasher = Sha256::new();
    hasher.update(tag);
    hasher.update(request_id.as_bytes());
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, profile_revision, profile, session_lane)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )
    .bind(turn_id)
    .bind(bot_id)
    .bind(conversation_id)
    .bind(request_id)
    .bind(hasher.finalize().to_vec())
    .bind(turn_no)
    .bind(item_id)
    .bind(provider_id)
    .bind(profile.as_ref().map(|(revision, _)| *revision))
    .bind(profile.as_ref().map(|(_, profile)| profile))
    .bind(&lane)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, ?3, ?4, 'routine', ?5)",
    )
    .bind(item_id)
    .bind(conversation_id)
    .bind(seq)
    .bind(turn_id)
    .bind(prompt)
    .execute(&mut *conn)
    .await?;
    sqlx::query("UPDATE conversations SET last_turn_no = ?2, last_item_seq = ?3 WHERE id = ?1")
        .bind(conversation_id)
        .bind(turn_no)
        .bind(seq)
        .execute(&mut *conn)
        .await?;
    Ok(Ok(turn_id))
}

async fn admit(
    pool: &SqlitePool,
    registry: &Registry,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<Due> = sqlx::query_as(
        "SELECT r.bot_id, r.conversation_id, r.name, r.prompt, r.prompt_revision, r.cron, r.timezone,
                r.model, r.next_due_at AS due_at
         FROM routines r JOIN bots b ON b.id = r.bot_id
         WHERE r.id = ?1 AND r.next_due_at IS NOT NULL AND r.next_due_at <= ?2
           AND b.status = 'active' AND NOT b.cancel_pending",
    )
    .bind(id)
    .bind(ts(now))
    .fetch_optional(&mut *tx)
    .await?;
    let Some(Due {
        bot_id,
        conversation_id,
        name,
        prompt,
        prompt_revision,
        cron,
        timezone,
        model,
        due_at,
    }) = row
    else {
        return Ok(false);
    };
    let next = Schedule::parse(&cron, Some(&timezone))
        .ok()
        .and_then(|schedule| schedule.next_after(now));
    sqlx::query("UPDATE routines SET next_due_at = ?2 WHERE id = ?1")
        .bind(id)
        .bind(next.map(ts))
        .execute(&mut *tx)
        .await?;
    let recorded: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM routine_runs WHERE routine_id = ?1 AND due_at = ?2)",
    )
    .bind(id)
    .bind(&due_at)
    .fetch_one(&mut *tx)
    .await?;
    if recorded {
        tx.commit().await?;
        return Ok(false);
    }
    let unfinished: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
                        WHERE rr.routine_id = ?1
                          AND t.status NOT IN ('completed', 'interrupted', 'failed', 'uncertain'))",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let run_id = Uuid::new_v4();
    let queued = if unfinished {
        Err(Skip::Overlap)
    } else {
        let run_on = match &model {
            Some(Json(own)) => RunOn::Own(own, id),
            None => RunOn::Bot,
        };
        queue_saved_task(
            &mut tx,
            registry,
            bot_id,
            conversation_id,
            run_id,
            b"routine-run\0",
            &prompt,
            run_on,
        )
        .await?
    };
    let turn_id = match queued {
        Ok(turn_id) => turn_id,
        Err(skip) => {
            sqlx::query(
                "INSERT INTO routine_runs (id, routine_id, due_at, name, skip_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(run_id)
            .bind(id)
            .bind(&due_at)
            .bind(&name)
            .bind(skip.as_str())
            .execute(&mut *tx)
            .await?;
            events::bot_updated(&mut tx, bot_id).await?;
            tx.commit().await?;
            return Ok(false);
        }
    };
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id, prompt_revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(run_id)
    .bind(id)
    .bind(&due_at)
    .bind(&name)
    .bind(turn_id)
    .bind(prompt_revision)
    .execute(&mut *tx)
    .await?;
    events::turn_accepted(&mut tx, bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(true)
}
