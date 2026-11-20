//! The owner's stop of one thing a bot waits on: an MCP job wait, a
//! computer job wait, or a task it delegated. Each wait ID names one row in
//! one of those tables. Stopping one never touches the bot's other waits or
//! the attempt that may be generating.

use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;

/// What a stop of one wait did, from its owning module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The wait was pending and is now stopped.
    Stopped { turn_id: Uuid },
    /// The owner already stopped it.
    Repeated { turn_id: Uuid },
    /// It ended before the stop.
    Ended,
}

/// Which table the wait came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Mcp,
    ComputerJob,
    Task,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped {
    pub kind: Kind,
    /// False when the owner had already stopped it.
    pub created: bool,
    /// The turn now continues with the stop as its outcome.
    pub requeued: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error("the bot has no such wait")]
    NotFound,
    #[error("the wait already ended")]
    Ended,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The owner stops `wait_id` of `bot_id`. The bot receives the stop as that
/// wait's outcome; a delegated task's sender reads it as cancelled by the
/// owner. Stopping a stopped wait, or a task already cancelled or ending,
/// returns `created: false`.
pub async fn stop(pool: &SqlitePool, bot_id: Uuid, wait_id: Uuid) -> Result<Stopped, StopError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let active: Option<bool> =
        sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = ?1")
            .bind(bot_id)
            .fetch_optional(&mut *tx)
            .await?;
    if active != Some(true) {
        return Err(StopError::NotFound);
    }
    let stopped = if let Some(stop) =
        super::mcp_waits::stop_by_owner(&mut tx, bot_id, wait_id).await?
    {
        settle(&mut tx, bot_id, Kind::Mcp, stop).await?
    } else if let Some(stop) =
        super::computer_job_waits::stop_by_owner(&mut tx, bot_id, wait_id).await?
    {
        settle(&mut tx, bot_id, Kind::ComputerJob, stop).await?
    } else {
        use super::delegations::OwnerStop;
        match super::delegations::stop_by_owner(&mut tx, bot_id, wait_id).await? {
            None => return Err(StopError::NotFound),
            Some(OwnerStop::Ended) => return Err(StopError::Ended),
            Some(OwnerStop::Repeated) => Stopped {
                kind: Kind::Task,
                created: false,
                requeued: false,
            },
            // The task's turn ending requeues its sender (a trigger).
            Some(OwnerStop::Stopped { root_turn_id }) => Stopped {
                kind: Kind::Task,
                created: true,
                requeued: sqlx::query_scalar("SELECT status = 'queued' FROM turns WHERE id = ?1")
                    .bind(root_turn_id)
                    .fetch_one(&mut *tx)
                    .await?,
            },
        }
    };
    tx.commit().await?;
    Ok(stopped)
}

/// Continue the turn of a wait its owning module just stopped.
async fn settle(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    kind: Kind,
    stop: Stop,
) -> Result<Stopped, StopError> {
    match stop {
        Stop::Ended => Err(StopError::Ended),
        Stop::Repeated { .. } => Ok(Stopped {
            kind,
            created: false,
            requeued: false,
        }),
        Stop::Stopped { turn_id } => {
            let requeued = super::turns::lifecycle::continue_waiting(conn, turn_id).await?;
            events::turn_changed(conn, bot_id, turn_id).await?;
            Ok(Stopped {
                kind,
                created: true,
                requeued,
            })
        }
    }
}
