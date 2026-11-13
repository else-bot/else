//! A bot's wait on one of its background computer jobs (`computer_job` with
//! `wait`).
//!
//! The attempt parks and its turn waits with no provider process. Whatever
//! ends the job settles the wait and requeues the turn as a continuation
//! that receives the job's outcome: an exit, a cancel, the job deadline, or
//! a lost computer. Both transitions are triggers on `guest_jobs.status`,
//! so no code path that ends a job can miss one. Ending the turn first
//! withdraws a pending wait (also a trigger).

use std::collections::HashMap;

use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use crate::computers::jobs::JobStatus;

/// What [`create`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    Waiting(Uuid),
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
/// same wait in this attempt returns its id; a job that already ended
/// returns [`Created::Ended`].
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
    let job: Option<JobStatus> =
        sqlx::query_scalar("SELECT status FROM guest_jobs WHERE id = ?1 AND bot_id = ?2")
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
}

/// The pending waits of these turns.
pub async fn pending_for_turns(
    conn: &mut SqliteConnection,
    turn_ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Pending>>, sqlx::Error> {
    let rows: Vec<Pending> = sqlx::query_as(
        "SELECT id, turn_id, job_id FROM computer_job_waits
         WHERE status = 'pending'
           AND lower(hex(turn_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1))
         ORDER BY created_at, id",
    )
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
}

/// Settled waits of `turn_id` that no attempt has received yet.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Vec<Outcome>, sqlx::Error> {
    sqlx::query_as(
        "SELECT w.id, w.job_id, j.command, j.status, j.exit_code,
             coalesce(j.output_tail, '') AS output_tail, j.error
         FROM computer_job_waits w JOIN guest_jobs j ON j.id = w.job_id
         WHERE w.turn_id = ?1 AND w.delivered_attempt IS NULL AND w.status = 'settled'
         ORDER BY w.created_at, w.id",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await
}

/// Record that `attempt` received these outcomes.
pub async fn mark_delivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
    ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE computer_job_waits SET delivered_attempt = ?3
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
