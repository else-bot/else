//! A bot's wait on one of its background computer jobs (`computer_job` with
//! `wait`).
//!
//! The attempt parks and its turn waits with no provider process. Whatever
//! ends the job settles the wait and requeues the turn as a continuation
//! that receives the job's outcome: an exit, a cancel, the job deadline, or
//! a lost computer. Both transitions are triggers on `guest_jobs.status`,
//! so no code path that ends a job can miss one. A new owner message joins
//! the waiting turn and keeps the wait on it. The owner may stop
//! the wait, which also cancels its job ([`stop_by_owner`]); the bot
//! receives that like an outcome. Any other end of the turn first withdraws
//! a pending wait (also a trigger), except the owner's Stop of the attempt
//! alone.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use crate::computers::jobs::JobStatus;
use crate::db::in_ids;

/// A wait's lifecycle, as stored in `computer_job_waits.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum Status {
    Pending,
    /// Its job ended.
    Settled,
    /// Withdrawn with its turn; never delivered.
    Cancelled,
    /// The owner stopped it; delivered like an outcome.
    Stopped,
}

/// What [`create`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    Waiting(Uuid),
    /// A pending wait from an earlier attempt of the turn already watches
    /// the job; the turn keeps it after this attempt ends. This attempt
    /// holds nothing, so it is not parked.
    Carried(Uuid),
    /// The job had already ended, so nothing waits.
    Ended,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CreateError {
    #[error("this attempt already waits or asked; end it and wait for the result")]
    AttemptParked,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("no such job")]
    NoSuchJob,
    #[error("database error")]
    Db,
}

impl From<sqlx::Error> for CreateError {
    fn from(err: sqlx::Error) -> Self {
        tracing::warn!(error = %err, "computer job wait failed");
        CreateError::Db
    }
}

/// Park `attempt` of `turn_id` on its bot's job `job_id`. A retry of the
/// same wait in this attempt returns its id, a pending wait on the job from
/// an earlier attempt returns [`Created::Carried`], and a job that already
/// ended returns [`Created::Ended`].
pub async fn create(
    pool: &SqlitePool,
    turn_id: Uuid,
    attempt: i32,
    job_id: Uuid,
) -> Result<Created, CreateError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = super::turns::asking_caller(&mut tx, turn_id)
        .await?
        .filter(|caller| caller.attempt == attempt)
        .ok_or(CreateError::NotRunning)?;
    let job: Option<JobStatus> = sqlx::query_scalar(
        "SELECT status FROM guest_jobs WHERE id = ?1 AND bot_id = ?2
             AND id NOT IN (SELECT job_id FROM watcher_checks)",
    )
    .bind(job_id)
    .bind(caller.bot_id)
    .fetch_optional(&mut *tx)
    .await?;
    if job.ok_or(CreateError::NoSuchJob)?.is_ended() {
        return Ok(Created::Ended);
    }
    let existing: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, job_id FROM computer_job_waits WHERE turn_id = ?1 AND attempt = ?2",
    )
    .bind(turn_id)
    .bind(attempt)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, waited)) = existing {
        return if waited == job_id {
            Ok(Created::Waiting(id))
        } else {
            Err(CreateError::AttemptParked)
        };
    }
    if super::turns::attempt_parked(&mut tx, turn_id, attempt).await? {
        return Err(CreateError::AttemptParked);
    }
    let carried: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM computer_job_waits
         WHERE turn_id = ?1 AND job_id = ?2 AND status = 'pending'",
    )
    .bind(turn_id)
    .bind(job_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(id) = carried {
        return Ok(Created::Carried(id));
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO computer_job_waits (id, bot_id, turn_id, attempt, job_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(id)
    .bind(caller.bot_id)
    .bind(turn_id)
    .bind(attempt)
    .bind(job_id)
    .execute(&mut *tx)
    .await?;
    events::turn_changed(&mut tx, caller.bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(Created::Waiting(id))
}

/// Publish each turn whose wait on `job_id` the job's end just settled, in
/// the transaction that ended it, so the owner sees it queued even while
/// dispatch is blocked; the status trigger requeued it.
pub async fn publish_settled(conn: &mut SqliteConnection, job_id: Uuid) -> Result<(), sqlx::Error> {
    let turns: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT bot_id, turn_id FROM computer_job_waits
         WHERE job_id = ?1 AND status = 'settled' AND delivered_attempt IS NULL",
    )
    .bind(job_id)
    .fetch_all(&mut *conn)
    .await?;
    for (bot_id, turn_id) in turns {
        events::turn_changed(conn, bot_id, turn_id).await?;
    }
    Ok(())
}

/// A wait whose job still runs, as the owner's turn view shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Pending {
    pub id: Uuid,
    #[serde(skip)]
    pub turn_id: Uuid,
    pub job_id: Uuid,
    /// The start of the job's command.
    pub command: String,
    /// The bot's label for the job; `None` for jobs started without one.
    pub purpose: Option<String>,
    pub job_status: JobStatus,
    pub created_at: String,
}

/// The pending waits of these turns.
pub async fn pending_for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Pending>>, sqlx::Error> {
    let rows: Vec<Pending> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT w.id, w.turn_id, w.job_id, substr(j.command, 1, 500) AS command,
             j.purpose, j.status AS job_status, w.created_at
         FROM computer_job_waits w JOIN guest_jobs j ON j.id = w.job_id
         WHERE w.status = 'pending'
           AND {}
         ORDER BY w.created_at, w.id",
        in_ids("w.turn_id", "?1")
    )))
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    let mut by_turn: HashMap<Uuid, Vec<Pending>> = HashMap::new();
    for wait in rows {
        by_turn.entry(wait.turn_id).or_default().push(wait);
    }
    Ok(by_turn)
}

/// How a waited-on job ended, given to the continuation as task data.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Outcome {
    pub id: Uuid,
    pub job_id: Uuid,
    pub command: String,
    pub status: JobStatus,
    pub exit_code: Option<i32>,
    pub output_tail: String,
    /// Why else.bot ended it, such as `deadline exceeded`.
    pub error: Option<String>,
    /// It likely ran out of its computer's memory, which has this many MiB.
    pub out_of_memory: bool,
    pub memory_mib: i32,
    /// `Settled` when the job ended, `Stopped` when the owner stopped the
    /// wait and cancelled the job.
    pub wait: Status,
}

/// Settled waits of `turn_id` that no attempt has received yet.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    sqlx::query_as(
        "SELECT w.id, w.job_id, j.command, j.status, j.exit_code,
             coalesce(j.output_tail, '') AS output_tail, j.error, w.status AS wait,
             j.out_of_memory, coalesce(c.memory_mib, 0) AS memory_mib
         FROM computer_job_waits w JOIN guest_jobs j ON j.id = w.job_id
         LEFT JOIN bot_computers c ON c.bot_id = j.bot_id
         WHERE w.turn_id = ?1 AND w.delivered_attempt IS NULL
           AND w.status IN ('settled', 'stopped')
         ORDER BY w.created_at, w.id",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await
}

/// The owner stops wait `id` of `bot_id`. A pending wait becomes
/// `stopped`, an outcome its turn receives, and its job is asked to stop;
/// the turn continues if nothing else holds it. `None` when the bot has no
/// such wait.
pub async fn stop_by_owner(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    id: Uuid,
) -> Result<Option<super::waits::Stop>, sqlx::Error> {
    use super::waits::Stop;
    let found: Option<(Status, Uuid, Uuid)> = sqlx::query_as(
        "SELECT status, turn_id, job_id FROM computer_job_waits WHERE id = ?1 AND bot_id = ?2",
    )
    .bind(id)
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((status, turn_id, job_id)) = found else {
        return Ok(None);
    };
    Ok(Some(match status {
        Status::Stopped => Stop::Repeated { turn_id },
        Status::Pending => {
            sqlx::query(
                "UPDATE computer_job_waits SET status = 'stopped',
                     settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1 AND status = 'pending'",
            )
            .bind(id)
            .execute(&mut *conn)
            .await?;
            crate::computers::jobs::request_cancel(&mut *conn, bot_id, Some(job_id)).await?;
            Stop::Stopped { turn_id }
        }
        Status::Settled | Status::Cancelled => Stop::Ended,
    }))
}

/// Withdraw the pending waits of `turn_id`, whose turn ends without a
/// status change that the trigger would see.
pub async fn withdraw_for_turn(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE computer_job_waits SET status = 'cancelled',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE turn_id = ?1 AND status = 'pending'",
    )
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Record that `attempt` received these outcomes.
pub async fn mark_delivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
    ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE computer_job_waits SET delivered_attempt = ?3
         WHERE turn_id = ?1 AND delivered_attempt IS NULL
           AND {}",
        in_ids("id", "?2")
    )))
    .bind(turn_id)
    .bind(Json(ids))
    .bind(attempt)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
