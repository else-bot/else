//! Turns no live provider owns after a restart, an orphaned task, or a
//! restore.

use std::collections::BTreeSet;

use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{KEEPS_WAITS, continue_waiting, ended, settled};
use crate::db::events;
use crate::db::in_ids;
use crate::db::turns::{ACTIVE_STATUSES, UNFINISHED_STATUSES};

/// Park each turn the owner's Stop left `cancelling` whose provider is
/// gone (a restart or an orphan): it keeps waiting on its jobs and tasks,
/// or continues with their outcomes, and is never replayed. `turn` limits
/// it to one turn. Returns the parked turns with their bots.
async fn park_unwound(
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
        continue_waiting(conn, turn_id).await?;
        events::turn_changed(conn, bot_id, turn_id).await?;
    }
    Ok(parked)
}

/// Startup reconciliation, run before the scheduler admits work. A turn
/// claimed but never delivered returns to the queue. A turn that may have
/// reached its provider is never replayed: it becomes `uncertain`, keeping
/// any partial text. Returns (requeued, uncertain).
pub async fn recover_after_restart(pool: &SqlitePool) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let requeued: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'queued', started_at = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status = 'preparing'
           AND bot_id IN (SELECT id FROM bots WHERE status = 'active' AND NOT cancel_pending)
         RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    for &(turn_id, bot_id) in &requeued {
        events::turn_changed(&mut tx, bot_id, turn_id).await?;
    }
    park_unwound(&mut tx, None).await?;
    let mut uncertain: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'uncertain', effect_uncertain = true,
             error_code = 'api_restarted',
             error_message = 'The API restarted while this message was being processed. \
It was not resent; send a follow-up to continue.',
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status IN ('sending', 'running', 'cancelling') RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let halted: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'interrupted', error_code = 'cancelled',
             error_message = 'Stopped before it was sent.', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status = 'preparing' RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    uncertain.extend(halted);
    for &(turn_id, bot_id) in &uncertain {
        ended(&mut tx, bot_id, turn_id).await?;
    }
    // A restore can bring back a turn that ended mid-handoff.
    crate::db::handoffs::settle_ended(&mut tx, None).await?;
    crate::db::steers::settle(&mut tx).await?;
    tx.commit().await?;
    crate::db::stop::settle_all(pool).await?;
    Ok((requeued.len() as u64, uncertain.len() as u64))
}

/// Turns in an active status that no provider task in this process owns.
/// The singleton lock means no other process runs them, so they are settled:
/// one never started fails, one that may have reached its provider becomes
/// `uncertain` (never resent). Pending stops and deletions then complete.
pub async fn settle_orphans(pool: &SqlitePool, live: &[Uuid]) -> Result<u64, sqlx::Error> {
    let orphans: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, bot_id FROM turns WHERE status IN {ACTIVE_STATUSES} AND NOT ({})",
        in_ids("id", "?1")
    )))
    .bind(sqlx::types::Json(live))
    .fetch_all(pool)
    .await?;
    let mut count = 0;
    for (turn_id, bot_id) in orphans {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("SELECT 1 FROM bots WHERE id = ?1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        crate::db::delegations::lock_targets_of_root(&mut tx, turn_id).await?;
        let parked = !park_unwound(&mut tx, Some(turn_id)).await?.is_empty();
        let updated = !parked
            && sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE turns SET
                     status = CASE WHEN status = 'preparing' THEN 'failed' ELSE 'uncertain' END,
                     effect_uncertain = effect_uncertain OR status <> 'preparing',
                     error_code = CASE WHEN status = 'preparing' THEN 'start_failed' ELSE 'orphaned' END,
                     error_message = CASE WHEN status = 'preparing'
                         THEN 'The message could not be started. Send it again.'
                         ELSE 'Processing stopped unexpectedly. It was not resent; send a follow-up to continue.'
                     END,
                     finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1 AND status IN {ACTIVE_STATUSES}"
            )))
            .bind(turn_id)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                == 1;
        if updated {
            ended(&mut tx, bot_id, turn_id).await?;
        }
        if parked || updated {
            settled(&mut tx, bot_id).await?;
            count += 1;
        }
        tx.commit().await?;
    }
    Ok(count)
}

/// The deliveries a restore stops, each recording why its turns ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Restored {
    /// Email deliveries, in `connector_deliveries`.
    Mail,
    /// Chat deliveries, in `social_deliveries`.
    Social,
}

impl Restored {
    fn deliveries(self) -> &'static str {
        match self {
            Self::Mail => "connector_deliveries",
            Self::Social => "social_deliveries",
        }
    }

    fn code(self) -> &'static str {
        match self {
            Self::Mail => "connector_restored",
            Self::Social => "social_restored",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Mail => {
                "Restored email work will not resume automatically. Inspect action receipts for prior effects."
            }
            Self::Social => "Restored social work will not resume automatically.",
        }
    }
}

/// End the unfinished turns that `restored` deliveries started, on a
/// restored database before any API serves it, and stop the tasks they
/// delegated, in one transaction. A turn that never reached its provider is
/// interrupted; one that may have acted becomes `uncertain`.
pub async fn end_restored(
    conn: &mut SqliteConnection,
    restored: Restored,
) -> Result<(), sqlx::Error> {
    let mut tx = sqlx::Connection::begin_with(conn, "BEGIN IMMEDIATE").await?;
    let ended_turns: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE turns SET
             status = CASE WHEN status IN ('queued', 'preparing') THEN 'interrupted' ELSE 'uncertain' END,
             effect_uncertain = effect_uncertain OR status NOT IN ('queued', 'preparing'),
             error_code = ?1, error_message = ?2,
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status IN {UNFINISHED_STATUSES} AND id IN (SELECT turn_id FROM {})
         RETURNING id, bot_id",
        restored.deliveries()
    )))
    .bind(restored.code())
    .bind(restored.message())
    .fetch_all(&mut *tx)
    .await?;
    for &(turn, bot) in &ended_turns {
        ended(&mut tx, bot, turn).await?;
    }
    let bots: BTreeSet<Uuid> = ended_turns.iter().map(|&(_, bot)| bot).collect();
    for bot in bots {
        crate::db::stop::settle(&mut tx, bot).await?;
    }
    crate::db::steers::settle(&mut tx).await?;
    tx.commit().await
}
