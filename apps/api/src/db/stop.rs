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

use super::bots::{self, BotError, ReceiptOperation};
use super::events;
use super::turns::ACTIVE_STATUSES;
use super::turns::lifecycle::{self, Unwind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReceipt {
    pub request_id: Uuid,
    pub bot_id: Uuid,
    pub state: ReceiptState,
    /// The active turn to interrupt, when this call started the stop.
    pub interrupt: Option<Uuid>,
    /// True when this call started the stop (not a retry).
    pub created: bool,
}

/// A Stop receipt's `request_receipts.state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, serde::Serialize)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ReceiptState {
    /// The provider has not stopped yet.
    Pending,
    Done,
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

    fn unwind(self) -> Unwind {
        match self {
            Self::Stop => Unwind::KeepWaits,
            Self::Delete => Unwind::EndAll,
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
    match halt {
        Halt::Stop => {
            lifecycle::park_on_tasks(conn, bot_id, boundary).await?;
        }
        // Their tasks stop with the bot's message before the turns end.
        Halt::Delete => crate::db::delegations::cancel_for_bot(conn, bot_id, boundary).await?,
    }
    lifecycle::interrupt_accepted(conn, bot_id, boundary, halt.unwind(), halt.message()).await?;
    crate::db::routines::stop_checks(conn, bot_id).await?;
    match halt {
        Halt::Stop => crate::computers::jobs::request_cancel_unwaited(&mut *conn, bot_id).await?,
        Halt::Delete => {
            // A turn an earlier Stop left `cancelling` ends with the bot.
            let stopped: Vec<Uuid> = sqlx::query_scalar(
                "SELECT id FROM turns WHERE bot_id = ?1 AND status = 'cancelling'",
            )
            .bind(bot_id)
            .fetch_all(&mut *conn)
            .await?;
            for turn in stopped {
                lifecycle::end_all(conn, turn, "cancelled", halt.message()).await?;
            }
            crate::computers::jobs::request_cancel(&mut *conn, bot_id, None).await?
        }
    };
    let active = lifecycle::request_cancel(conn, bot_id, boundary, halt.unwind()).await?;
    crate::db::steers::settle(&mut *conn).await?;
    if active.is_some() {
        return Ok(active);
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
    let Some(status) = bots::status(&mut *tx, bot_id).await? else {
        return Err(BotError::NotFound.into());
    };
    let last_turn_no: i64 = sqlx::query_scalar(
        "SELECT last_turn_no FROM conversations WHERE bot_id = ?1 AND kind = 'owner'",
    )
    .bind(bot_id)
    .fetch_one(&mut *tx)
    .await?;
    let receipt: Option<(ReceiptOperation, Uuid, Vec<u8>, Option<ReceiptState>)> = sqlx::query_as(
        "SELECT operation, bot_id, payload_hash, state FROM request_receipts WHERE request_id = ?1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((operation, saved_bot, hash, state)) = receipt {
        if operation != ReceiptOperation::Cancel || saved_bot != bot_id || hash != stop_hash(bot_id)
        {
            return Err(BotError::RequestConflict.into());
        }
        let state =
            state.ok_or_else(|| sqlx::Error::Decode("cancel receipt without a state".into()))?;
        tx.commit().await?;
        return Ok(StopReceipt {
            request_id,
            bot_id,
            state,
            interrupt: None,
            created: false,
        });
    }
    if !status.is_active() {
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
        state: if settled {
            ReceiptState::Done
        } else {
            ReceiptState::Pending
        },
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
