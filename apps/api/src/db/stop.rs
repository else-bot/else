//! Bot-wide Stop work and deletion while work is in progress.
//!
//! Both halt everything accepted up to a boundary under the bot row lock:
//! queued turns end as interrupted without running, and the active turn moves
//! to `cancelling` so the scheduler interrupts its provider. The bot stays
//! blocked (`cancel_pending` or `deleting`) until the active turn records a
//! terminal outcome; [`settle`] then completes receipts and tombstones in the
//! same transaction. Nothing claims that a stopped action was undone.

use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::bots::{BotError, lock_request};
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

/// End queued turns with `turn_no <= boundary` and move the active one to
/// `cancelling`. Returns the active turn, if any. The caller holds the bot
/// row lock.
pub async fn halt_accepted(
    conn: &mut PgConnection,
    bot_id: Uuid,
    boundary: i64,
    message: &str,
) -> Result<Option<Uuid>, sqlx::Error> {
    let queued: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE turns SET status = 'interrupted', error_code = 'cancelled', error_message = $3,
             finished_at = now(), updated_at = now()
         WHERE bot_id = $1 AND status = 'queued' AND turn_no <= $2
         RETURNING id",
    )
    .bind(bot_id)
    .bind(boundary)
    .bind(message)
    .fetch_all(&mut *conn)
    .await?;
    for turn_id in queued {
        events::turn_changed(conn, bot_id, turn_id).await?;
    }
    let active: Option<Uuid> = sqlx::query_scalar(
        "UPDATE turns SET status = 'cancelling', updated_at = now()
         WHERE bot_id = $1 AND turn_no <= $2 AND status IN ('preparing', 'sending', 'running')
         RETURNING id",
    )
    .bind(bot_id)
    .bind(boundary)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(turn_id) = active {
        events::turn_changed(conn, bot_id, turn_id).await?;
        return Ok(Some(turn_id));
    }
    // Already cancelling from an earlier stop.
    sqlx::query_scalar("SELECT id FROM turns WHERE bot_id = $1 AND status = 'cancelling'")
        .bind(bot_id)
        .fetch_optional(&mut *conn)
        .await
}

/// Complete pending stops and deletion once no turn is active. Runs in the
/// caller's transaction under the bot row lock. Returns true if it changed
/// anything.
pub async fn settle(conn: &mut PgConnection, bot_id: Uuid) -> Result<bool, sqlx::Error> {
    let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM turns WHERE bot_id = $1 AND status IN {ACTIVE_STATUSES})"
    )))
    .bind(bot_id)
    .fetch_one(&mut *conn)
    .await?;
    if active {
        return Ok(false);
    }
    let receipts = sqlx::query(
        "UPDATE request_receipts SET state = 'done', completed_at = now()
         WHERE bot_id = $1 AND operation = 'cancel' AND state = 'pending'",
    )
    .bind(bot_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let bot = sqlx::query(
        "UPDATE bots SET cancel_pending = false,
             status = CASE WHEN status = 'deleting' THEN 'deleted' ELSE status END
         WHERE id = $1 AND (cancel_pending OR status = 'deleting')",
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

/// Stop all work accepted for this bot so far. A retry with the same request
/// ID returns the original receipt and never affects later work.
pub async fn stop_work(
    pool: &PgPool,
    bot_id: Uuid,
    request_id: Uuid,
) -> Result<StopReceipt, StopError> {
    let mut tx = pool.begin().await?;
    lock_request(&mut tx, request_id).await?;
    // Lock first, then read the boundary in a new statement: a send that
    // held the lock has committed by then, so its turn is inside the stop.
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM bots WHERE id = $1 FOR UPDATE")
            .bind(bot_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(status) = status else {
        return Err(BotError::NotFound.into());
    };
    let last_turn_no: i64 =
        sqlx::query_scalar("SELECT last_turn_no FROM conversations WHERE bot_id = $1")
            .bind(bot_id)
            .fetch_one(&mut *tx)
            .await?;
    let receipt: Option<(String, Uuid, Vec<u8>, Option<String>)> = sqlx::query_as(
        "SELECT operation, bot_id, payload_hash, state FROM request_receipts WHERE request_id = $1",
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
        });
    }
    if status != "active" {
        return Err(StopError::Deleted);
    }
    sqlx::query(
        "INSERT INTO request_receipts
             (request_id, operation, bot_id, payload_hash, state, boundary_turn_no)
         VALUES ($1, 'cancel', $2, $3, 'pending', $4)",
    )
    .bind(request_id)
    .bind(bot_id)
    .bind(stop_hash(bot_id))
    .bind(last_turn_no)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE bots SET cancel_pending = true WHERE id = $1")
        .bind(bot_id)
        .execute(&mut *tx)
        .await?;
    let interrupt = halt_accepted(&mut tx, bot_id, last_turn_no, "Stopped by the owner.").await?;
    let settled = settle(&mut tx, bot_id).await?;
    if !settled {
        events::bot_updated(&mut tx, bot_id).await?;
    }
    tx.commit().await?;
    Ok(StopReceipt {
        request_id,
        bot_id,
        state: if interrupt.is_some() {
            "pending"
        } else {
            "done"
        }
        .to_owned(),
        interrupt,
    })
}

/// Settle every bot whose stop or deletion was waiting on work that is no
/// longer active, e.g. after startup reconciliation.
pub async fn settle_all(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let bots: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM bots WHERE cancel_pending OR status = 'deleting'")
            .fetch_all(pool)
            .await?;
    let mut settled = 0;
    for bot_id in bots {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT 1 FROM bots WHERE id = $1 FOR UPDATE")
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
