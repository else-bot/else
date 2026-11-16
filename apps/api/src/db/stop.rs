//! Bot-wide Stop work and deletion while work is in progress.
//!
//! Both halt the work accepted up to a boundary inside the immediate
//! transaction: queued turns do not run, and the active turn moves to
//! `cancelling` so the scheduler interrupts its provider. The bot stays
//! blocked (`cancel_pending` or `deleting`) until no turn is active; [`settle`]
//! then completes receipts and tombstones in the same transaction. Nothing
//! claims that a stopped action was undone.
//!
//! Stop ends generation only. The jobs and tasks a turn waits on keep
//! running, and the turn parks on them; the owner stops each one on its own
//! ([`super::waits::stop`]). Deletion ends them all.

use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::bots::BotError;
use super::events;
use super::turns::ACTIVE_STATUSES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReceipt {
    pub request_id: Uuid,
    pub bot_id: Uuid,
    /// `pending` until the provider has stopped, then `done`.
    pub state: String,
    /// The active turn to interrupt, when this call started the stop.
    pub interrupt: Option<Uuid>,
    /// True when this call started the stop (not a retry).
    pub created: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum StopError {
    #[error(transparent)]
    Bot(#[from] BotError),
    #[error("bot deleted")]
    Deleted,
}

impl From<sqlx::Error> for StopError {
    fn from(err: sqlx::Error) -> Self {
        Self::Bot(BotError::Db(err))
    }
}

fn stop_hash(bot_id: Uuid) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"cancel\0");
    hasher.update(bot_id.as_bytes());
    hasher.finalize().to_vec()
}

/// The error code of a turn the owner's Stop moved to `cancelling`. Its
/// pending waits stay (the `turns_end_*_waits` triggers skip it), and once
/// the attempt ends the turn parks on what it still waits for.
pub const KEEPS_WAITS: &str = "stopped";

/// How a `cancelling` turn ends once its provider stops, decoded from its
/// error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unwind {
    /// The owner's Stop of the attempt alone: the turn parks on its jobs
    /// and tasks, or continues with their outcomes.
    KeepWaits,
    /// Any other cancel: the turn ends and its waits were withdrawn.
    EndAll,
}

impl Unwind {
    pub fn of(error_code: Option<&str>) -> Self {
        if error_code == Some(KEEPS_WAITS) {
            Self::KeepWaits
        } else {
            Self::EndAll
        }
    }
}

/// Turn a Stop of `turn`'s attempt into a full cancel with `code` and
/// `message`: its pending waits are withdrawn now. False when `turn` is not
/// a turn the owner's Stop left `cancelling`.
pub async fn end_all(
    conn: &mut SqliteConnection,
    turn: Uuid,
    code: &str,
    message: &str,
) -> Result<bool, sqlx::Error> {
    let changed = sqlx::query(
        "UPDATE turns SET error_code = ?2, error_message = ?3,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'cancelling' AND error_code IS ?4",
    )
    .bind(turn)
    .bind(code)
    .bind(message)
    .bind(KEEPS_WAITS)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if changed == 0 {
        return Ok(false);
    }
    crate::db::mcp_waits::withdraw_for_turn(conn, turn).await?;
    crate::db::computer_job_waits::withdraw_for_turn(conn, turn).await?;
    Ok(true)
}

/// Park each turn the owner's Stop left `cancelling` whose provider is
/// gone (a restart or an orphan): it keeps waiting on its jobs and tasks,
/// or continues with their outcomes, and is never replayed. `turn` limits
/// it to one turn. Returns the parked turns with their bots.
pub async fn park_unwound(
    conn: &mut SqliteConnection,
    turn: Option<Uuid>,
) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
    let parked: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'waiting', error_code = NULL, error_message = NULL,
             effect_uncertain = true,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status = 'cancelling' AND error_code IS ?1 AND (?2 IS NULL OR id = ?2)
           AND ((SELECT coalesce(sum(h.pending), 0) FROM turn_holds h
                 WHERE h.turn_id = turns.id) > 0
                OR EXISTS (SELECT 1 FROM mcp_waits w WHERE w.turn_id = turns.id
                           AND w.delivered_attempt IS NULL
                           AND w.status IN ('finished', 'failed', 'expired', 'stopped'))
                OR EXISTS (SELECT 1 FROM computer_job_waits w WHERE w.turn_id = turns.id
                           AND w.delivered_attempt IS NULL AND w.status IN ('settled', 'stopped'))
                OR EXISTS (SELECT 1 FROM delegations d WHERE d.root_turn_id = turns.id
                           AND d.delivered_attempt IS NULL
                           AND d.status IN ('completed', 'failed', 'cancelled')))
         RETURNING id, bot_id",
    )
    .bind(KEEPS_WAITS)
    .bind(turn)
    .fetch_all(&mut *conn)
    .await?;
    for &(turn_id, bot_id) in &parked {
        crate::db::turns::continue_waiting(conn, turn_id).await?;
        events::turn_changed(conn, bot_id, turn_id).await?;
    }
    Ok(parked)
}

/// What halts the accepted work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Halt {
    /// The owner's Stop: generation ends, waits and tasks stay.
    Stop,
    /// Bot deletion: everything ends.
    Delete,
}

impl Halt {
    fn message(self) -> &'static str {
        match self {
            Self::Stop => "Stopped by the owner.",
            Self::Delete => "The bot was deleted.",
        }
    }
}

/// Halt the turns with `turn_no <= boundary`: queued ones never run, and
/// the active one moves to `cancelling`. On [`Halt::Stop`] a turn that
/// waits only on jobs or tasks parks on them instead of ending, and those
/// jobs and tasks keep running; a turn that also waits on the owner ends
/// with its tasks, as before. Returns the active turn, if any. The caller
/// holds the bot immediate transaction.
pub async fn halt_accepted(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    boundary: i64,
    halt: Halt,
) -> Result<Option<Uuid>, sqlx::Error> {
    crate::db::delegations::lock_targets_of_bot(conn, bot_id).await?;
    let stop = halt == Halt::Stop;
    // A turn whose every pending hold is a job or a task: Stop leaves it
    // parked on them. One that also waits on the owner ends as before.
    const ONLY_JOBS_AND_TASKS: &str = "(SELECT coalesce(sum(h.pending), 0) FROM turn_holds h
             WHERE h.turn_id = turns.id)
         = (SELECT count(*) FROM delegations d
            WHERE d.root_turn_id = turns.id AND d.status IN ('queued', 'running'))
           + (SELECT count(*) FROM mcp_waits w WHERE w.turn_id = turns.id AND w.status = 'pending')
           + (SELECT count(*) FROM computer_job_waits w
              WHERE w.turn_id = turns.id AND w.status = 'pending')";
    let parked: Vec<Uuid> = if stop {
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "UPDATE turns SET status = 'waiting', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE bot_id = ?1 AND turn_no <= ?2 AND status = 'queued'
               AND (SELECT coalesce(sum(h.pending), 0) FROM turn_holds h
                    WHERE h.turn_id = turns.id) > 0
               AND {ONLY_JOBS_AND_TASKS}
             RETURNING id"
        )))
        .bind(bot_id)
        .bind(boundary)
        .fetch_all(&mut *conn)
        .await?
    } else {
        Vec::new()
    };
    let ended: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "UPDATE turns SET status = 'interrupted', error_code = 'cancelled', error_message = ?3,
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND turn_no <= ?2
           AND (status = 'queued' OR (status = 'waiting' AND NOT (?4 AND {ONLY_JOBS_AND_TASKS})))
         RETURNING id"
    )))
    .bind(bot_id)
    .bind(boundary)
    .bind(halt.message())
    .bind(stop)
    .fetch_all(&mut *conn)
    .await?;
    if stop {
        for turn_id in &ended {
            crate::db::delegations::cancel_for_root(conn, *turn_id).await?;
        }
    }
    let halted = parked.iter().chain(&ended);
    for turn_id in halted {
        events::turn_changed(conn, bot_id, *turn_id).await?;
    }
    crate::db::routines::stop_checks(conn, bot_id).await?;
    match halt {
        Halt::Stop => crate::computers::jobs::request_cancel_unwaited(&mut *conn, bot_id).await?,
        Halt::Delete => {
            crate::db::delegations::cancel_for_bot(conn, bot_id, boundary).await?;
            // A turn an earlier Stop left `cancelling` ends with the bot.
            let stopped: Vec<Uuid> = sqlx::query_scalar(
                "SELECT id FROM turns WHERE bot_id = ?1 AND status = 'cancelling'",
            )
            .bind(bot_id)
            .fetch_all(&mut *conn)
            .await?;
            for turn in stopped {
                end_all(conn, turn, "cancelled", halt.message()).await?;
            }
            crate::computers::jobs::request_cancel(&mut *conn, bot_id, None).await?
        }
    };
    let active: Option<Uuid> = sqlx::query_scalar(
        "UPDATE turns SET status = 'cancelling', error_code = CASE WHEN ?3 THEN ?4 END,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND turn_no <= ?2 AND status IN ('preparing', 'sending', 'running')
         RETURNING id",
    )
    .bind(bot_id)
    .bind(boundary)
    .bind(stop)
    .bind(KEEPS_WAITS)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(turn_id) = active {
        events::turn_changed(conn, bot_id, turn_id).await?;
        return Ok(Some(turn_id));
    }
    sqlx::query_scalar("SELECT id FROM turns WHERE bot_id = ?1 AND status = 'cancelling'")
        .bind(bot_id)
        .fetch_optional(&mut *conn)
        .await
}

/// Complete pending stops and deletion once no turn, no cancelling guest
/// job, and no foreground command remains (and, for deletion, the computer is shut down). Runs in the
/// caller's transaction inside the immediate transaction. Returns true if it changed
/// anything.
pub async fn settle(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<bool, sqlx::Error> {
    let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM turns WHERE bot_id = ?1 AND status IN {ACTIVE_STATUSES})
             OR EXISTS (SELECT 1 FROM guest_jobs WHERE bot_id = ?1 AND status = 'cancelling')
             OR EXISTS (SELECT 1 FROM guest_runs WHERE bot_id = ?1 AND ended_at IS NULL)"
    )))
    .bind(bot_id)
    .fetch_one(&mut *conn)
    .await?;
    if active {
        return Ok(false);
    }
    let receipts = sqlx::query(
        "UPDATE request_receipts SET state = 'done', completed_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND operation = 'cancel' AND state = 'pending'",
    )
    .bind(bot_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let bot = sqlx::query(
        "WITH down AS (SELECT NOT EXISTS (
                 SELECT 1 FROM bot_computers WHERE bot_id = ?1
                   AND observed_state NOT IN ('absent', 'stopped', 'missing')) AS ok)
         UPDATE bots SET cancel_pending = false,
             status = CASE WHEN status = 'deleting' AND (SELECT ok FROM down)
                 THEN 'deleted' ELSE status END
         WHERE id = ?1 AND (cancel_pending OR (status = 'deleting' AND (SELECT ok FROM down)))",
    )
    .bind(bot_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let changed = receipts > 0 || bot > 0;
    if changed {
        events::bot_updated(conn, bot_id).await?;
    }
    Ok(changed)
}

/// Stop the generation accepted for this bot so far. A retry with the same request
/// ID returns the original receipt and never affects later work.
pub async fn stop_work(
    pool: &SqlitePool,
    bot_id: Uuid,
    request_id: Uuid,
) -> Result<StopReceipt, StopError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    if crate::db::categories::used_by_category(&mut tx, request_id).await? {
        return Err(BotError::RequestConflict.into());
    }
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(status) = status else {
        return Err(BotError::NotFound.into());
    };
    let last_turn_no: i64 = sqlx::query_scalar(
        "SELECT last_turn_no FROM conversations WHERE bot_id = ?1 AND kind = 'owner'",
    )
    .bind(bot_id)
    .fetch_one(&mut *tx)
    .await?;
    let receipt: Option<(String, Uuid, Vec<u8>, Option<String>)> = sqlx::query_as(
        "SELECT operation, bot_id, payload_hash, state FROM request_receipts WHERE request_id = ?1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((operation, saved_bot, hash, state)) = receipt {
        if operation != "cancel" || saved_bot != bot_id || hash != stop_hash(bot_id) {
            return Err(BotError::RequestConflict.into());
        }
        tx.commit().await?;
        return Ok(StopReceipt {
            request_id,
            bot_id,
            state: state.unwrap_or_default(),
            interrupt: None,
            created: false,
        });
    }
    if status != "active" {
        return Err(StopError::Deleted);
    }
    sqlx::query(
        "INSERT INTO request_receipts
             (request_id, operation, bot_id, payload_hash, state, boundary_turn_no)
         VALUES (?1, 'cancel', ?2, ?3, 'pending', ?4)",
    )
    .bind(request_id)
    .bind(bot_id)
    .bind(stop_hash(bot_id))
    .bind(last_turn_no)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE bots SET cancel_pending = true WHERE id = ?1")
        .bind(bot_id)
        .execute(&mut *tx)
        .await?;
    let interrupt = halt_accepted(&mut tx, bot_id, last_turn_no, Halt::Stop).await?;
    let settled = settle(&mut tx, bot_id).await?;
    if !settled {
        events::bot_updated(&mut tx, bot_id).await?;
    }
    tx.commit().await?;
    Ok(StopReceipt {
        request_id,
        bot_id,
        state: if settled { "done" } else { "pending" }.to_owned(),
        interrupt,
        created: true,
    })
}

/// Settle every bot whose stop or deletion was waiting on work that is no
/// longer active, e.g. after startup reconciliation.
pub async fn settle_all(pool: &SqlitePool) -> Result<u64, sqlx::Error> {
    let bots: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM bots WHERE cancel_pending OR status = 'deleting'")
            .fetch_all(pool)
            .await?;
    let mut settled = 0;
    for bot_id in bots {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("SELECT 1 FROM bots WHERE id = ?1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        if settle(&mut tx, bot_id).await? {
            settled += 1;
        }
        tx.commit().await?;
    }
    Ok(settled)
}
