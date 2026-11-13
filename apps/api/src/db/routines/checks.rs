//! A routine's check runs: admission reserves the check job, settlement
//! reads how it ended and queues the run's turn or skips the run, and Stop
//! or deletion skips the runs still checking.

use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{Admit, RunOn, Skip, UNSETTLED, queue_saved_task, skip};
use crate::app::routines::{
    AfterCheck, CHECK_DURATION, CheckEnd, CheckSkip, RoutineModel, run_prompt,
};
use crate::computers::ComputerManager;
use crate::computers::jobs::JobStatus;
use crate::db::events;
use crate::providers::Registry;

/// A reserved check job to launch with
/// [`ComputerManager::launch_reserved`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub bot_id: Uuid,
    pub job_id: Uuid,
    pub command: String,
}

#[derive(sqlx::FromRow)]
pub(super) struct CheckedJob {
    check_status: Option<JobStatus>,
    check_exit: Option<i32>,
    check_output: Option<String>,
    check_leased: Option<bool>,
}

impl CheckedJob {
    pub(super) const COLUMNS: &str = "j.status AS check_status, j.exit_code AS check_exit,
        j.output_tail AS check_output, j.lease_id IS NOT NULL AS check_leased";

    pub(super) fn end(self) -> Option<CheckEnd> {
        CheckEnd::of(
            self.check_status?,
            self.check_exit,
            self.check_output,
            self.check_leased.unwrap_or(false),
        )
    }
}

/// Reserve a due run's check job in `computers` and record the run as
/// checking, inside the admission's immediate transaction. Without
/// computers, or without a free job slot, the run is skipped as
/// `check_unavailable`.
#[allow(clippy::too_many_arguments)] // one admitted run's identity, split by column
pub(super) async fn admit_check(
    mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
    computers: Option<&ComputerManager>,
    bot_id: Uuid,
    routine_id: Uuid,
    run_id: Uuid,
    due_at: &str,
    name: &str,
    command: String,
) -> Result<Admit, sqlx::Error> {
    let reserved = match computers {
        Some(computers) => {
            computers
                .reserve_job(&mut tx, bot_id, None, &command, CHECK_DURATION, &[], None)
                .await?
        }
        None => None,
    };
    let Some(job_id) = reserved else {
        return skip(
            tx,
            bot_id,
            routine_id,
            run_id,
            due_at,
            name,
            Skip::CheckUnavailable,
        )
        .await;
    };
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, due_at, name, check_job_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(run_id)
    .bind(routine_id)
    .bind(due_at)
    .bind(name)
    .bind(job_id)
    .execute(&mut *tx)
    .await?;
    events::bot_updated(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(Admit::Checking(Check {
        bot_id,
        job_id,
        command,
    }))
}

/// Checking runs settled per scheduler scan.
pub const SETTLE_BATCH: i64 = 8;

/// Settle up to `limit` checking runs whose check job has ended, per
/// [`CheckEnd::after`]: skip the run, or queue its turn with the routine's
/// current prompt and what the check reported. A run whose bot stopped or
/// left, or whose routine was deleted, is skipped as `check_cancelled`;
/// Stop and deletion settle their runs themselves, see [`stop_checks`]. Returns how many turns were
/// queued. The caller holds a maintenance admission.
pub async fn settle_checks(
    pool: &SqlitePool,
    registry: &Registry,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let ended: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT rr.id FROM routine_runs rr JOIN guest_jobs j ON j.id = rr.check_job_id
         WHERE {UNSETTLED} AND j.status IN ('completed', 'failed', 'cancelled', 'lost')
         ORDER BY rr.due_at, rr.id LIMIT ?1"
    )))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut queued = 0;
    for id in ended {
        if settle_check(pool, registry, id).await? {
            queued += 1;
        }
    }
    Ok(queued)
}

#[derive(sqlx::FromRow)]
struct Checked {
    routine_id: Uuid,
    bot_id: Uuid,
    conversation_id: Uuid,
    prompt: String,
    prompt_revision: i64,
    model: Option<Json<RoutineModel>>,
    live: bool,
    #[sqlx(flatten)]
    check: CheckedJob,
}

async fn settle_check(
    pool: &SqlitePool,
    registry: &Registry,
    run_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<Checked> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT rr.routine_id, r.bot_id, r.conversation_id, r.prompt, r.prompt_revision, r.model,
                b.status = 'active' AND NOT b.cancel_pending AND r.deleted_at IS NULL AS live,
                {checked}
         FROM routine_runs rr JOIN routines r ON r.id = rr.routine_id
         JOIN bots b ON b.id = r.bot_id JOIN guest_jobs j ON j.id = rr.check_job_id
         WHERE rr.id = ?1 AND {UNSETTLED}",
        checked = CheckedJob::COLUMNS
    )))
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(run) = row else {
        return Ok(false);
    };
    let Some(end) = run.check.end() else {
        return Ok(false);
    };
    let after = if run.live {
        end.after()
    } else {
        AfterCheck::Skip(CheckSkip::Cancelled)
    };
    let queued = match after {
        AfterCheck::Skip(reason) => Err(Skip::from(reason)),
        AfterCheck::Wake(note) => queue_saved_task(
            &mut tx,
            registry,
            run.bot_id,
            run.conversation_id,
            run_id,
            b"routine-run\0",
            &run_prompt(&run.prompt, Some(&note)),
            RunOn::routine(run.routine_id, &run.model),
        )
        .await?
        .map_err(Skip::from),
    };
    let (turn_id, reason) = match queued {
        Ok(turn_id) => (Some(turn_id), None),
        Err(reason) => (None, Some(reason)),
    };
    let settled = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE routine_runs AS rr SET turn_id = ?2, skip_reason = ?3, prompt_revision = ?4
         WHERE rr.id = ?1 AND {UNSETTLED}"
    )))
    .bind(run_id)
    .bind(turn_id)
    .bind(reason)
    .bind(turn_id.map(|_| run.prompt_revision))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if settled != 1 {
        return Ok(false);
    }
    match turn_id {
        Some(turn_id) => events::turn_accepted(&mut tx, run.bot_id, turn_id).await?,
        None => events::bot_updated(&mut tx, run.bot_id).await?,
    }
    tx.commit().await?;
    Ok(turn_id.is_some())
}

/// Skip every unsettled check run of the bot as `check_cancelled`, inside a
/// Stop's immediate transaction, so a check that already ended cannot queue
/// its turn after the stop. Stop cancels the bot's jobs itself.
pub(crate) async fn stop_checks(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<(), sqlx::Error> {
    skip_checks(conn, bot_id, None).await.map(drop)
}

/// Skip the unsettled check runs of the bot, or of one of its routines, as
/// `check_cancelled`. Returns the check jobs of the runs it skipped.
pub(super) async fn skip_checks(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    routine_id: Option<Uuid>,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let jobs: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "UPDATE routine_runs AS rr SET skip_reason = 'check_cancelled'
         WHERE {UNSETTLED} AND rr.check_job_id IS NOT NULL
           AND rr.routine_id IN (SELECT id FROM routines WHERE bot_id = ?1)
           AND (?2 IS NULL OR rr.routine_id = ?2)
         RETURNING check_job_id"
    )))
    .bind(bot_id)
    .bind(routine_id)
    .fetch_all(&mut *conn)
    .await?;
    if !jobs.is_empty() {
        events::bot_updated(conn, bot_id).await?;
    }
    Ok(jobs)
}
