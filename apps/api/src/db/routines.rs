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
//! A routine with a check admits a run as checking instead: the same
//! transaction reserves the check's job in the bot's computer, and the
//! caller launches it. Once the job ends, [`settle_checks`] skips the run
//! when the check found nothing new, or queues its turn with what the
//! check reported (see [`CheckEnd`]).
//!
//! A reminder is a routine due once, at `run_at`. Admitting it leaves it
//! with no due time; a run that completes deletes it, and one that fails or
//! is skipped stays listed so the owner sees what happened.
//!
//! A routine either inherits the bot's live model or has its own. A run on
//! its own model snapshots that profile into its turn and leaves the bot's
//! profile alone. Every run keeps its native session in the routine's own
//! lane and starts a fresh one there, so it neither resets nor replays the
//! bot's chat session or earlier runs.

use chrono::{DateTime, SecondsFormat, Utc};
use else_wire::routines::{PausedReason, RoutineRun, RunStatus};
use else_wire::turns::TurnError;
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use super::turns::{Phase, TurnStatus};
use crate::app::providers::Origin;
use crate::app::routines::{
    ActiveHours, InvalidRoutine, MAX_PER_BOT, ModelSetting, RoutineCheck, RoutineModel,
    RoutineName, RoutinePrompt, Schedule,
};
use crate::db::bots::{self, BotStatus};

mod admit;
mod checks;
mod review;

pub use admit::{
    Admitted, NoNewsRefusal, Report, RunNow, Skip, admit_due, report_acted, report_no_news, run_now,
};
pub(crate) use admit::{RunOn, queue_saved_task};
pub(crate) use checks::stop_checks;
pub use checks::{Check, SETTLE_BATCH, settle_checks};
pub(crate) use review::behind_unreviewed_run;
pub use review::{REVIEW_ATTEMPTS, review_ended};

use checks::skip_checks;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct StoredRoutineUpdate {
    routine_id: Uuid,
    name: String,
}

impl From<&StoredRoutineUpdate> for else_wire::items::RoutineUpdate {
    fn from(value: &StoredRoutineUpdate) -> Self {
        Self {
            routine_id: value.routine_id,
            name: value.name.clone(),
        }
    }
}

/// Due routines admitted per scheduler scan.
pub const ADMIT_BATCH: i64 = 8;

const UNSETTLED: &str = "rr.turn_id IS NULL AND rr.skip_reason IS NULL";

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
    #[error(
        "a run of this routine is already queued or in progress; it reports to the owner when it \
         ends"
    )]
    AlreadyRunning,
    #[error("the bot's queue is full; run the routine again once queued requests finish")]
    QueueFull,
    #[error(
        "no provider can run this routine now; the owner can check it in Settings or choose \
         another model for the routine"
    )]
    ProviderUnavailable,
    #[error(
        "the routine's last run did not fail and it is not paused, so there is nothing to retry"
    )]
    NothingToRetry,
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
    pub state: RunState,
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
    /// The command run before each run, if any.
    pub check: Option<String>,
    pub cron: String,
    /// When a reminder runs; `None` for a routine on a cron schedule.
    pub run_at: Option<DateTime<Utc>>,
    pub timezone: String,
    /// When runs may start; `None` is any time.
    pub active_hours: Option<ActiveHours>,
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
    /// The last run is the owner's retry of the paused routine, not yet
    /// reviewed.
    pub retrying: bool,
    /// Its runs may end with no news, which alerts no one.
    pub quiet: bool,
}

impl From<&LastRun> for else_wire::routines::LastRun {
    fn from(run: &LastRun) -> Self {
        Self {
            due_at: run.due_at,
            status: run.state.into(),
            skip_reason: match run.state {
                RunState::Skipped(skip) => Some(skip.into()),
                _ => None,
            },
            turn_id: run.turn_id,
            error: run.error_code.as_ref().map(|code| TurnError {
                code: code.clone(),
                message: run.error_message.clone(),
            }),
            finished_at: run.finished_at,
        }
    }
}

impl From<&Routine> for else_wire::routines::Routine {
    fn from(routine: &Routine) -> Self {
        let (model, requested_model) = match &routine.model {
            ModelSetting::Own(model) => (Some(model.into()), None),
            ModelSetting::Inherit { requested } => (None, requested.clone()),
        };
        Self {
            id: routine.id,
            bot_id: routine.bot_id,
            name: routine.name.clone(),
            prompt: routine.prompt.clone(),
            prompt_revision: routine.prompt_revision,
            check: routine.check.clone(),
            cron: routine.cron.clone(),
            run_at: routine.run_at,
            timezone: routine.timezone.clone(),
            active_hours: routine.active_hours.as_ref().map(Into::into),
            enabled: routine.enabled,
            model,
            requested_model,
            next_run_at: routine.next_run_at,
            created_at: routine.created_at,
            updated_at: routine.updated_at,
            last_run: routine.last_run.as_ref().map(Into::into),
            failed_runs: routine.failed_runs,
            paused_reason: routine.paused_reason.clone().map(PausedReason::from),
            retrying: routine.retrying,
            quiet: routine.quiet,
        }
    }
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    bot_id: Uuid,
    name: String,
    prompt: String,
    prompt_revision: i64,
    check_command: Option<String>,
    cron: String,
    run_at: Option<DateTime<Utc>>,
    timezone: String,
    #[sqlx(flatten)]
    active: ActiveColumns,
    enabled: bool,
    model: Option<Json<RoutineModel>>,
    requested_model: Option<String>,
    next_due_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
    failed_runs: i64,
    paused_reason: Option<String>,
    retrying: bool,
    quiet: bool,
    run_due_at: Option<DateTime<Utc>>,
    skip_reason: Option<Skip>,
    report: Option<Report>,
    turn_id: Option<Uuid>,
    turn_status: Option<TurnStatus>,
    error_code: Option<String>,
    error_message: Option<String>,
    finished_at: Option<DateTime<Utc>>,
}

/// Where a run is, decoded from its row; the table's checks keep a turn
/// and a skip reason exclusive, and a run with neither awaits its check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Checking,
    Skipped(Skip),
    Turn(TurnStatus),
    /// Its turn completed with nothing to report.
    NoNews,
}

impl RunState {
    fn of(turn: Option<TurnStatus>, skip: Option<Skip>, report: Option<Report>) -> Self {
        match (turn, skip, report) {
            (Some(TurnStatus::Completed), _, Some(Report::NoNews)) => Self::NoNews,
            (Some(status), _, _) => Self::Turn(status),
            (None, Some(skip), _) => Self::Skipped(skip),
            (None, None, _) => Self::Checking,
        }
    }
}

impl From<RunState> for RunStatus {
    fn from(state: RunState) -> Self {
        match state {
            RunState::Checking => Self::Checking,
            RunState::Skipped(_) => Self::Skipped,
            RunState::NoNews => Self::NoNews,
            RunState::Turn(status) => match status.phase() {
                Phase::Active | Phase::Waiting => Self::Running,
                Phase::Queued | Phase::Ended => Self::from(status.as_str().to_owned()),
            },
        }
    }
}

impl From<Row> for Routine {
    fn from(row: Row) -> Self {
        let last_run = row.run_due_at.map(|due_at| LastRun {
            due_at,
            state: RunState::of(row.turn_status, row.skip_reason, row.report),
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
            check: row.check_command,
            cron: row.cron,
            run_at: row.run_at,
            timezone: row.timezone,
            active_hours: row.active.hours(),
            enabled: row.enabled,
            model: setting(row.model, row.requested_model),
            next_run_at: row.next_due_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            deleted: row.deleted_at.is_some(),
            last_run,
            failed_runs: row.failed_runs,
            paused_reason: row.paused_reason,
            retrying: row.retrying,
            quiet: row.quiet,
        }
    }
}

/// A routine's stored active hours.
#[derive(sqlx::FromRow, Default)]
struct ActiveColumns {
    active_days: Option<i64>,
    active_start: Option<i64>,
    active_end: Option<i64>,
}

impl ActiveColumns {
    fn hours(&self) -> Option<ActiveHours> {
        ActiveHours::from_columns(self.active_days?, self.active_start?, self.active_end?)
    }

    fn from_hours(hours: Option<ActiveHours>) -> Self {
        hours.map_or_else(Self::default, |hours| {
            let (days, start, end) = hours.columns();
            Self {
                active_days: Some(days),
                active_start: Some(start),
                active_end: Some(end),
            }
        })
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
    "SELECT r.id, r.bot_id, r.name, r.prompt, r.prompt_revision, r.check_command, r.cron, r.run_at, r.timezone,
        r.active_days, r.active_start, r.active_end, r.enabled,
        r.model, r.requested_model, r.next_due_at, r.created_at, r.updated_at, r.deleted_at,
        r.failed_runs, r.paused_reason, r.quiet,
        EXISTS (SELECT 1 FROM routine_runs x WHERE x.routine_id = r.id
                  AND x.resumes AND NOT x.reviewed) AS retrying,
        lr.due_at AS run_due_at, lr.skip_reason,
        lr.report, lr.turn_id, t.status AS turn_status,
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
    let Some(turn) = super::turns::caller(conn, caller.turn_id).await? else {
        return Err(RoutineError::NotRunning);
    };
    let Some((owner_conversation, _)) =
        super::conversation::owner_conversation(conn, turn.bot_id).await?
    else {
        return Err(RoutineError::NotRunning);
    };
    if turn.bot_id != caller.bot_id || turn.conversation_id != caller.conversation_id {
        return Err(RoutineError::NotAllowed);
    }
    if super::delegations::origin(conn, caller.turn_id).await? != Origin::OwnerRequest
        && !super::delegations::carries_owner_answer(conn, caller.turn_id).await?
    {
        return Err(RoutineError::NotAllowed);
    }
    if !turn.running {
        return Err(RoutineError::NotRunning);
    }
    Ok(owner_conversation)
}

pub(crate) async fn authorize_by(conn: &mut SqliteConnection, by: By) -> Result<(), RoutineError> {
    match by {
        By::Turn(caller) => authorized_owner_conversation(conn, caller).await.map(drop),
        By::Owner { bot_id } => match bots::status(&mut *conn, bot_id).await? {
            None => Err(RoutineError::NotFound),
            Some(BotStatus::Active) => Ok(()),
            Some(_) => Err(RoutineError::BotDeleted),
        },
    }
}

pub struct NewRoutine<'a> {
    pub name: &'a RoutineName,
    pub prompt: &'a RoutinePrompt,
    pub check: Option<&'a RoutineCheck>,
    pub schedule: &'a Schedule,
    /// The time zone as the bot sent it; `None` when it took the owner's.
    /// A retry matches on this, not on the zone it resolved to.
    pub requested_timezone: Option<&'a str>,
    pub model: &'a ModelSetting,
    /// The model arguments as the bot sent them, if any: a retry with the
    /// same request returns the same routine even if resolving would now
    /// differ.
    pub model_request: Option<&'a str>,
    /// A reminder's time as the bot sent it (`in_minutes` resolves against
    /// the clock, so a retry matches on the request).
    pub when_request: Option<&'a str>,
    pub quiet: bool,
}

fn create_hash(routine: &NewRoutine<'_>) -> Vec<u8> {
    let hours_key = routine
        .schedule
        .hours()
        .map(|hours| format!("hours {hours}"));
    let check_key = routine
        .check
        .map(|check| format!("check {}", check.as_str()));
    let mut hasher = Sha256::new();
    for part in [
        "routine",
        routine.prompt.as_str(),
        routine.schedule.cron(),
        routine.requested_timezone.unwrap_or("owner"),
    ]
    .into_iter()
    .chain(hours_key.as_deref())
    .chain(check_key.as_deref())
    .chain(routine.model_request)
    .chain(routine.when_request)
    .chain(routine.quiet.then_some("quiet"))
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
    routine.schedule.check_due(now)?;
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
    let active = ActiveColumns::from_hours(routine.schedule.hours());
    sqlx::query(
        "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
             next_due_at, created_turn_id, created_name, request_hash, model, requested_model,
             active_days, active_start, active_end, run_at, check_command, quiet)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?4, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
    .bind(active.active_days)
    .bind(active.active_start)
    .bind(active.active_end)
    .bind(routine.schedule.run_at().map(ts))
    .bind(routine.check.map(RoutineCheck::as_str))
    .bind(routine.quiet)
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
    /// `Some(None)` removes the check.
    pub check: Option<Option<RoutineCheck>>,
    pub cron: Option<String>,
    pub timezone: Option<String>,
    /// `Some(None)` lets runs start at any time.
    pub active_hours: Option<Option<ActiveHours>>,
    pub enabled: Option<bool>,
    pub model: Option<ModelSetting>,
    pub quiet: Option<bool>,
    /// Forget the routine's failed runs (the owner saw them).
    pub dismiss_failures: bool,
}

#[derive(sqlx::FromRow)]
struct RoutineSettings {
    name: String,
    prompt: String,
    check_command: Option<String>,
    cron: String,
    run_at: Option<DateTime<Utc>>,
    timezone: String,
    #[sqlx(flatten)]
    active: ActiveColumns,
    enabled: bool,
    next_due_at: Option<String>,
    model: Option<Json<RoutineModel>>,
    requested_model: Option<String>,
    quiet: bool,
}

/// Apply `patch`. Setting values a routine already has changes nothing, so
/// a retry never postpones its schedule. A new prompt or check is a new
/// prompt revision, which ends the routine's destructive grant. Disabling stops
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
        "SELECT name, prompt, check_command, cron, run_at, timezone, active_days, active_start, active_end,
             enabled, next_due_at, model, requested_model, quiet
         FROM routines WHERE id = ?1 AND bot_id = ?2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(by.bot_id())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(RoutineSettings {
        name,
        prompt,
        check_command,
        cron,
        run_at,
        timezone,
        active,
        enabled,
        next_due_at: next_due,
        model,
        requested_model: requested,
        quiet,
    }) = current
    else {
        return Err(RoutineError::NotFound);
    };
    let hours = active.hours();
    let reschedules =
        patch.cron.is_some() || patch.timezone.is_some() || patch.active_hours.is_some();
    let schedule = match run_at {
        Some(_) if reschedules => return Err(InvalidRoutine::FixedTime.into()),
        Some(_) => Schedule::stored(&cron, run_at, &timezone, None)?,
        None => Schedule::parse_due(
            patch.cron.as_deref().unwrap_or(&cron),
            Some(patch.timezone.as_deref().unwrap_or(&timezone)),
            patch.active_hours.unwrap_or(hours),
            now,
        )?,
    };
    let new_name = patch.name.map_or(name.clone(), |n| n.as_str().to_owned());
    let new_prompt = patch
        .prompt
        .map_or(prompt.clone(), |p| p.as_str().to_owned());
    let new_check = patch
        .check
        .map_or(check_command, |c| c.map(|c| c.as_str().to_owned()));
    let new_enabled = patch.enabled.unwrap_or(enabled);
    let new_quiet = patch.quiet.unwrap_or(quiet);
    if new_quiet && run_at.is_some() {
        return Err(InvalidRoutine::QuietReminder.into());
    }
    let new_model = patch.model.unwrap_or_else(|| setting(model, requested));
    let (model, requested_model) = columns(&new_model);
    let rescheduled =
        schedule.cron() != cron || schedule.timezone() != timezone || schedule.hours() != hours;
    let active = ActiveColumns::from_hours(schedule.hours());
    let next_due = if !new_enabled {
        None
    } else if rescheduled || !enabled {
        let next = schedule.next_after(now);
        if next.is_none() && run_at.is_some() {
            return Err(InvalidRoutine::Passed.into());
        }
        next.map(ts)
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
             next_due_at = ?7,
             prompt_revision = prompt_revision + (prompt IS NOT ?3 OR check_command IS NOT ?14),
             check_command = ?14,
             model = ?8, requested_model = ?9,
             active_days = ?11, active_start = ?12, active_end = ?13, quiet = ?15,
             failed_runs = CASE WHEN ?10 OR (?6 AND NOT enabled) THEN 0 ELSE failed_runs END,
             paused_reason = CASE WHEN ?10 OR ?6 THEN NULL ELSE paused_reason END,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND (name IS NOT ?2 OR prompt IS NOT ?3 OR cron IS NOT ?4
             OR timezone IS NOT ?5 OR enabled IS NOT ?6 OR next_due_at IS NOT ?7
             OR model IS NOT ?8 OR requested_model IS NOT ?9
             OR active_days IS NOT ?11 OR active_start IS NOT ?12 OR active_end IS NOT ?13
             OR check_command IS NOT ?14 OR quiet IS NOT ?15
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
    .bind(active.active_days)
    .bind(active.active_start)
    .bind(active.active_end)
    .bind(&new_check)
    .bind(new_quiet)
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
            let update = StoredRoutineUpdate {
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
            .bind(if run_at.is_some() {
                format!("Updated reminder: {new_name}")
            } else {
                format!("Updated routine: {new_name}")
            })
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
            // A check still running for this routine must not queue a turn
            // for it; its job is asked to stop.
            for job in skip_checks(&mut tx, by.bot_id(), Some(id)).await? {
                crate::computers::jobs::request_cancel(&mut *tx, by.bot_id(), Some(job)).await?;
            }
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
    let row: Option<(Uuid, String, DateTime<Utc>, bool, bool)> = sqlx::query_as(
        "SELECT rr.routine_id, rr.name, rr.due_at, r.run_at IS NOT NULL, r.quiet
         FROM routine_runs rr JOIN routines r ON r.id = rr.routine_id WHERE rr.turn_id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(conn)
    .await?;
    Ok(
        row.map(|(routine_id, name, due_at, reminder, quiet)| RoutineRun {
            routine_id,
            name,
            due_at,
            reminder,
            quiet,
            no_news: false,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_update_reads_existing_json() {
        let json = r#"{"routine_id":"37c15814-56b0-4a52-836c-70b9ab580bdc","name":"Daily brief"}"#;
        let stored: StoredRoutineUpdate = serde_json::from_str(json).unwrap();
        let wire: else_wire::items::RoutineUpdate = (&stored).into();
        assert_eq!(
            wire,
            else_wire::items::RoutineUpdate {
                routine_id: "37c15814-56b0-4a52-836c-70b9ab580bdc".parse().unwrap(),
                name: "Daily brief".into(),
            }
        );
        assert_eq!(serde_json::to_string(&stored).unwrap(), json);
    }
}
