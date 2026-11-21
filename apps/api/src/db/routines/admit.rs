//! Admitting routine runs: due ones from the scheduler, and on demand.

use chrono::{DateTime, Utc};
use else_wire::routines::SkipReason;
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::app::messages::MAX_QUEUED_TURNS;
use crate::app::providers::{Availability, Profile};
use crate::app::routines::{CheckSkip, RoutineModel, Schedule};
use crate::computers::ComputerManager;
use crate::db::turns::ENDED_STATUSES;
use crate::db::{bot_profiles, events};
use crate::providers::Registry;

use super::{
    ActiveColumns, By, Caller, Check, Routine, RoutineError, UNSETTLED, authorize_by,
    authorized_owner_conversation, checks, load, ts,
};

#[derive(sqlx::FromRow)]
struct OnDemand {
    conversation_id: Uuid,
    name: String,
    prompt: String,
    prompt_revision: i64,
    model: Option<Json<RoutineModel>>,
    retryable: bool,
    queued: bool,
    busy: bool,
    last_due_at: Option<DateTime<Utc>>,
}

/// Who asks for a run now, and the request that names it.
#[derive(Debug, Clone, Copy)]
pub enum RunNow {
    /// The bot, for an owner request; one turn queues at most one run of
    /// each routine.
    Turn(Caller),
    /// The owner's Retry of a routine whose last run failed or that
    /// else.bot paused; a success switches a paused routine back on.
    Resume { bot_id: Uuid, request_id: Uuid },
}

impl RunNow {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::Turn(_) => b"routine-run-now\0",
            Self::Resume { .. } => b"routine-resume\0",
        }
    }
}

/// Queue a run of a routine now, at the owner's request, outside its
/// schedule and active hours. It leaves the schedule and the enabled state
/// alone, and its outcome is reviewed like a scheduled run, except that a
/// successful [`RunNow::Resume`] switches its paused routine back on. One
/// request queues at most one run: a retry returns that run. Returns the
/// routine and whether this call queued the run.
pub async fn run_now(
    pool: &SqlitePool,
    registry: &Registry,
    by: RunNow,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<(Routine, bool), RoutineError> {
    crate::providers::recheck_unavailable(registry).await;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (bot_id, request_id) = match by {
        RunNow::Turn(caller) => {
            authorized_owner_conversation(&mut tx, caller).await?;
            (caller.bot_id, caller.turn_id)
        }
        RunNow::Resume { bot_id, request_id } => {
            authorize_by(&mut tx, By::Owner { bot_id }).await?;
            (bot_id, request_id)
        }
    };
    let resumes = matches!(by, RunNow::Resume { .. });
    let mut hasher = Sha256::new();
    hasher.update(by.tag());
    hasher.update(request_id.as_bytes());
    hasher.update(id.as_bytes());
    let digest = hasher.finalize();
    let run_id = Uuid::from_bytes(
        digest[..16]
            .try_into()
            .expect("a SHA-256 digest has 16 bytes"),
    );
    let row: Option<OnDemand> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT r.conversation_id, r.name, r.prompt, r.prompt_revision, r.model,
                (NOT r.enabled AND r.paused_reason IS NOT NULL) OR r.failed_runs > 0 AS retryable,
                EXISTS (SELECT 1 FROM routine_runs WHERE id = ?3) AS queued,
                (EXISTS (SELECT 1 FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
                         WHERE rr.routine_id = r.id
                           AND (t.status NOT IN {ENDED_STATUSES}
                                OR NOT rr.reviewed))
                 OR EXISTS (SELECT 1 FROM routine_runs rr WHERE rr.routine_id = r.id
                              AND {UNSETTLED})) AS busy,
                (SELECT max(due_at) FROM routine_runs WHERE routine_id = r.id) AS last_due_at
         FROM routines r WHERE r.id = ?1 AND r.bot_id = ?2 AND r.deleted_at IS NULL"
    )))
    .bind(id)
    .bind(bot_id)
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(OnDemand {
        conversation_id,
        name,
        prompt,
        prompt_revision,
        model,
        retryable,
        queued,
        busy,
        last_due_at,
    }) = row
    else {
        return Err(RoutineError::NotFound);
    };
    if queued {
        let routine = load(&mut tx, id).await?;
        tx.commit().await?;
        return Ok((routine, false));
    }
    if resumes && !retryable {
        return Err(RoutineError::NothingToRetry);
    }
    // An ended run awaiting review may still be retried; wait for it too.
    if busy {
        return Err(RoutineError::AlreadyRunning);
    }
    let run_on = RunOn::routine(id, &model);
    let turn_id = match queue_saved_task(
        &mut tx,
        registry,
        bot_id,
        conversation_id,
        run_id,
        by.tag(),
        &prompt,
        run_on,
    )
    .await?
    {
        Ok(turn_id) => turn_id,
        Err(QueueRefusal::QueueFull) => return Err(RoutineError::QueueFull),
        Err(QueueRefusal::ProviderUnavailable) => return Err(RoutineError::ProviderUnavailable),
    };
    // Later than every earlier run, so it is the routine's last run.
    let due = last_due_at.map_or(now, |last| {
        now.max(last + chrono::Duration::milliseconds(1))
    });
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id, prompt_revision, resumes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(run_id)
    .bind(id)
    .bind(ts(due))
    .bind(&name)
    .bind(turn_id)
    .bind(prompt_revision)
    .bind(resumes)
    .execute(&mut *tx)
    .await?;
    if resumes {
        // The owner's copy of the routine applies only a newer one.
        sqlx::query(
            "UPDATE routines SET updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
    }
    events::turn_accepted(&mut tx, bot_id, turn_id).await?;
    let routine = load(&mut tx, id).await?;
    tx.commit().await?;
    Ok((routine, true))
}

/// What one scan of due routines did.
#[derive(Debug, Default)]
pub struct Admitted {
    /// Turns queued.
    pub queued: usize,
    /// Check jobs reserved, for the caller to launch.
    pub checks: Vec<Check>,
}

pub(super) enum Admit {
    Nothing,
    Queued,
    Checking(Check),
}

/// Admit up to `limit` routines due at `now`. A routine whose ended run
/// awaits [`review_ended`] waits for it, so a retry precedes the next
/// occurrence. A routine with a check reserves its job in `computers`;
/// without computers its run is skipped as `check_unavailable`. The caller
/// holds a maintenance admission.
pub async fn admit_due(
    pool: &SqlitePool,
    registry: &Registry,
    computers: Option<&ComputerManager>,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<Admitted, sqlx::Error> {
    let due: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT r.id FROM routines r JOIN bots b ON b.id = r.bot_id
         WHERE r.next_due_at IS NOT NULL AND r.next_due_at <= ?1
           AND b.status = 'active' AND NOT b.cancel_pending
           AND NOT EXISTS (SELECT 1 FROM routine_runs x JOIN turns u ON u.id = x.turn_id
                           WHERE x.routine_id = r.id AND NOT x.reviewed
                             AND u.status IN {ENDED_STATUSES})
         ORDER BY r.next_due_at, r.id LIMIT ?2"
    )))
    .bind(ts(now))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    if !due.is_empty() {
        crate::providers::recheck_unavailable(registry).await;
    }
    let mut admitted = Admitted::default();
    for id in due {
        match admit(pool, registry, computers, id, now).await? {
            Admit::Nothing => {}
            Admit::Queued => admitted.queued += 1,
            Admit::Checking(check) => admitted.checks.push(check),
        }
    }
    Ok(admitted)
}

/// Why a due run queued no turn, as stored in `routine_runs.skip_reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Skip {
    Overlap,
    QueueFull,
    ProviderUnavailable,
    NothingNew,
    CheckCancelled,
    CheckUnavailable,
}

impl From<Skip> for SkipReason {
    fn from(skip: Skip) -> Self {
        match skip {
            Skip::Overlap => Self::Overlap,
            Skip::QueueFull => Self::QueueFull,
            Skip::ProviderUnavailable => Self::ProviderUnavailable,
            Skip::NothingNew => Self::NothingNew,
            Skip::CheckCancelled => Self::CheckCancelled,
            Skip::CheckUnavailable => Self::CheckUnavailable,
        }
    }
}

impl From<CheckSkip> for Skip {
    fn from(skip: CheckSkip) -> Self {
        match skip {
            CheckSkip::NothingNew => Self::NothingNew,
            CheckSkip::Cancelled => Self::CheckCancelled,
            CheckSkip::Unavailable => Self::CheckUnavailable,
        }
    }
}

/// What a run's turn tells the owner, as `routine_runs.report` stores it.
/// A run reports news unless its bot ended it with no news. Migration
/// 0075's triggers turn no news back into news when the turn parks, or when
/// it completes having involved the owner or acted outside the bot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
pub enum Report {
    News,
    NoNews,
    /// A gated action ran in the run, so it reports whatever its bot says.
    Acted,
}

/// Why a run cannot end with no news.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NoNewsRefusal {
    #[error("only a scheduled routine run can end with no news")]
    NotARoutineRun,
    #[error(
        "this routine is not quiet, so its runs always report; the owner can mark it quiet in \
         Bot details > Routines"
    )]
    NotQuiet,
    #[error("an approved or allowed action ran in this run, so it reports to the owner")]
    Acted,
    #[error("the run is no longer running")]
    NotRunning,
}

/// Record that the run `turn_id` executes has nothing to report. Repeating
/// it changes nothing. Only a running run of a quiet routine moves from
/// news to no news; one that acted never does.
pub async fn report_no_news(
    pool: &SqlitePool,
    turn_id: Uuid,
) -> Result<Result<Report, NoNewsRefusal>, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let run: Option<(bool, Report)> = sqlx::query_as(
        "SELECT r.quiet, rr.report FROM routine_runs rr
         JOIN routines r ON r.id = rr.routine_id WHERE rr.turn_id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *tx)
    .await?;
    let running = crate::db::turns::asking_caller(&mut tx, turn_id)
        .await?
        .is_some();
    let refusal = match run {
        None => NoNewsRefusal::NotARoutineRun,
        Some((false, _)) => NoNewsRefusal::NotQuiet,
        Some((_, Report::Acted)) => NoNewsRefusal::Acted,
        Some(_) if !running => NoNewsRefusal::NotRunning,
        Some((true, Report::News | Report::NoNews)) => {
            let report: Report = sqlx::query_scalar(
                "UPDATE routine_runs SET report = 'no_news'
                 WHERE turn_id = ?1 AND report IN ('news', 'no_news') RETURNING report",
            )
            .bind(turn_id)
            .fetch_one(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(Ok(report));
        }
    };
    Ok(Err(refusal))
}

/// Record that a gated action is about to run in the turn, if it executes a
/// routine run: the run then reports whatever its bot says. Repeating it
/// changes nothing.
pub async fn report_acted(pool: &SqlitePool, turn_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE routine_runs SET report = 'acted' WHERE turn_id = ?1 AND report <> 'acted'",
    )
    .bind(turn_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Why [`queue_saved_task`] queued no turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueueRefusal {
    QueueFull,
    ProviderUnavailable,
}

impl From<QueueRefusal> for Skip {
    fn from(refusal: QueueRefusal) -> Self {
        match refusal {
            QueueRefusal::QueueFull => Self::QueueFull,
            QueueRefusal::ProviderUnavailable => Self::ProviderUnavailable,
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
    run_at: Option<DateTime<Utc>>,
    timezone: String,
    #[sqlx(flatten)]
    active: ActiveColumns,
    model: Option<Json<RoutineModel>>,
    check_command: Option<String>,
    due_at: String,
}

struct Chosen {
    provider_id: &'static str,
    profile: Option<(i64, Json<Profile>)>,
}

/// A run on the routine's own model, if its provider can run it now. An
/// unavailable choice skips the run rather than switching models; the
/// profile revision 0 marks a routine's own profile, not a bot revision.
fn own_model(registry: &Registry, own: &RoutineModel) -> Option<Chosen> {
    let provider = registry
        .get(&own.provider_id)
        .filter(|p| p.uses_profile() && p.availability() == Availability::Available)?;
    provider.validate_profile(&own.profile).ok()?;
    Some(Chosen {
        provider_id: provider.id(),
        profile: Some((0, Json(own.profile.clone()))),
    })
}

/// Where a saved task's turn runs.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RunOn<'a> {
    /// A webhook wake: the bot's selected provider and profile, in its chat
    /// session.
    Chat,
    /// A routine run, on its own model if it has one and otherwise the
    /// bot's, in that routine's session lane.
    Routine(Uuid, Option<&'a RoutineModel>),
}

impl<'a> RunOn<'a> {
    pub(crate) fn routine(id: Uuid, model: &'a Option<Json<RoutineModel>>) -> Self {
        RunOn::Routine(id, model.as_ref().map(|Json(own)| own))
    }

    /// The provider session lane; '' is the bot's chat.
    fn lane(self) -> String {
        match self {
            RunOn::Chat => String::new(),
            RunOn::Routine(id, _) => format!("routine:{id}"),
        }
    }
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
) -> Result<Result<Uuid, QueueRefusal>, sqlx::Error> {
    let queued: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {}",
        crate::db::steers::queued_runs("?1")
    )))
    .bind(bot_id)
    .fetch_one(&mut *conn)
    .await?;
    if queued >= MAX_QUEUED_TURNS {
        return Ok(Err(QueueRefusal::QueueFull));
    }
    let chosen = match run_on {
        RunOn::Routine(_, Some(own)) => own_model(registry, own),
        RunOn::Chat | RunOn::Routine(_, None) => {
            bot_profiles::for_admission(&mut *conn, registry, bot_id)
                .await?
                .map(|chosen| Chosen {
                    provider_id: chosen.provider_id,
                    profile: chosen.profile,
                })
        }
    };
    let Some(Chosen {
        provider_id,
        profile,
    }) = chosen
    else {
        return Ok(Err(QueueRefusal::ProviderUnavailable));
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
    .bind(run_on.lane())
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
    computers: Option<&ComputerManager>,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<Admit, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<Due> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT r.bot_id, r.conversation_id, r.name, r.prompt, r.prompt_revision, r.cron, r.run_at,
                r.timezone, r.active_days, r.active_start, r.active_end, r.model, r.check_command,
                r.next_due_at AS due_at
         FROM routines r JOIN bots b ON b.id = r.bot_id
         WHERE r.id = ?1 AND r.next_due_at IS NOT NULL AND r.next_due_at <= ?2
           AND b.status = 'active' AND NOT b.cancel_pending
           AND NOT EXISTS (SELECT 1 FROM routine_runs x JOIN turns u ON u.id = x.turn_id
                           WHERE x.routine_id = r.id AND NOT x.reviewed
                             AND u.status IN {ENDED_STATUSES})"
    )))
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
        run_at,
        timezone,
        active,
        model,
        check_command,
        due_at,
    }) = row
    else {
        return Ok(Admit::Nothing);
    };
    let schedule = Schedule::stored(&cron, run_at, &timezone, active.hours()).ok();
    let next = schedule.as_ref().and_then(|s| s.next_after(now));
    sqlx::query("UPDATE routines SET next_due_at = ?2 WHERE id = ?1")
        .bind(id)
        .bind(next.map(ts))
        .execute(&mut *tx)
        .await?;
    // Admitted late (the server was down or busy) after its active hours
    // closed: the next occurrence inside them stands in for it.
    if schedule.is_some_and(|s| !s.allows(now)) {
        tx.commit().await?;
        return Ok(Admit::Nothing);
    }
    let recorded: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM routine_runs WHERE routine_id = ?1 AND due_at = ?2)",
    )
    .bind(id)
    .bind(&due_at)
    .fetch_one(&mut *tx)
    .await?;
    if recorded {
        tx.commit().await?;
        return Ok(Admit::Nothing);
    }
    let unfinished: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM routine_runs rr LEFT JOIN turns t ON t.id = rr.turn_id
                        WHERE rr.routine_id = ?1
                          AND (t.status NOT IN {ENDED_STATUSES} OR {UNSETTLED}))"
    )))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let run_id = Uuid::new_v4();
    let queued = if unfinished {
        Err(Skip::Overlap)
    } else if let Some(command) = check_command {
        return checks::admit_check(tx, computers, bot_id, id, run_id, &due_at, &name, command)
            .await;
    } else {
        let run_on = RunOn::routine(id, &model);
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
        .map_err(Skip::from)
    };
    let turn_id = match queued {
        Ok(turn_id) => turn_id,
        Err(reason) => return skip(tx, bot_id, id, run_id, &due_at, &name, reason).await,
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
    Ok(Admit::Queued)
}

pub(super) async fn skip(
    mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    bot_id: Uuid,
    routine_id: Uuid,
    run_id: Uuid,
    due_at: &str,
    name: &str,
    reason: Skip,
) -> Result<Admit, sqlx::Error> {
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, due_at, name, skip_reason)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(run_id)
    .bind(routine_id)
    .bind(due_at)
    .bind(name)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    events::bot_updated(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(Admit::Nothing)
}
