//! A turn's life, and the only code that writes `turns.status`.
//!
//! Each transition is a named function whose `WHERE` clause checks the
//! states it leaves. The `turns_*` triggers in the migrations are part of
//! these transitions. A turn that ends runs [`ended`], so every end has the
//! same follow-up, and the transition that ended it then settles the
//! messages joined to it ([`crate::db::steers::settle`]) and, when it left
//! the bot with nothing active, the bot's pending stop
//! ([`crate::db::stop::settle`]). A halt settles its stop once, after all of
//! its steps ([`crate::db::stop::halt_accepted`]).

use sqlx::SqliteConnection;
use uuid::Uuid;

use super::{ACTIVE_STATUSES, TurnStatus};
use crate::app::providers::{FailureCode, TurnOutcome};
use crate::db::events;

mod recovery;
mod settle;
#[cfg(test)]
mod tests;

pub use recovery::{Restored, end_restored, recover_after_restart, settle_orphans};
use settle::{Disposition, End, Facts, Holds, Reason, limit_cut, settle};

/// The error code of a turn the owner's Stop moved to `cancelling`. The
/// `turns_end_*_waits` triggers spell it too: they keep its pending waits.
const KEEPS_WAITS: &str = "stopped";

/// How a `cancelling` turn ends once its provider stops. Stored in the
/// turn's `error_code`, which only this module reads for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unwind {
    /// The owner's Stop of the attempt alone: the turn parks on its jobs
    /// and tasks, or continues with their outcomes.
    KeepWaits,
    /// Any other cancel: the turn ends and its waits were withdrawn.
    EndAll,
}

impl Unwind {
    fn decode(error_code: Option<&str>) -> Self {
        if error_code == Some(KEEPS_WAITS) {
            Self::KeepWaits
        } else {
            Self::EndAll
        }
    }

    fn code(self) -> Option<&'static str> {
        match self {
            Self::KeepWaits => Some(KEEPS_WAITS),
            Self::EndAll => None,
        }
    }
}

/// How `turn` unwinds, or `None` when it is not `cancelling`.
pub async fn unwind(
    conn: &mut SqliteConnection,
    turn: Uuid,
) -> Result<Option<Unwind>, sqlx::Error> {
    let row: Option<(TurnStatus, Option<String>)> =
        sqlx::query_as("SELECT status, error_code FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(row
        .filter(|(status, _)| *status == TurnStatus::Cancelling)
        .map(|(_, code)| Unwind::decode(code.as_deref())))
}

/// What follows every end: publish the turn, log its reply to memory, stop
/// the tasks it handed on, and mark a handoff it never confirmed uncertain.
async fn ended(conn: &mut SqliteConnection, bot: Uuid, turn: Uuid) -> Result<(), sqlx::Error> {
    events::turn_changed(conn, bot, turn).await?;
    crate::db::memory::log_turn_end(conn, turn).await?;
    Box::pin(crate::db::delegations::cancel_for_root(conn, turn)).await?;
    crate::db::handoffs::settle_ended(conn, Some(turn)).await
}

/// What follows a transition that ended a turn of `bot` or took one out of
/// its active statuses: messages joined to an ended turn run on their own
/// or end with it, and the bot's pending stop completes once nothing is
/// active.
async fn settled(conn: &mut SqliteConnection, bot: Uuid) -> Result<(), sqlx::Error> {
    crate::db::stop::settle(conn, bot).await?;
    crate::db::steers::settle(conn).await
}

/// Claim queued `turn` of `bot` for a new attempt: it moves to `preparing`
/// unless it is deferred, joined another reply, waits for the review of an
/// earlier routine run in its conversation, or the bot already has an
/// active turn. True when it did.
pub async fn claim(
    conn: &mut SqliteConnection,
    turn: Uuid,
    bot: Uuid,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE turns AS t SET status = 'preparing', started_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             attempt = attempt + 1
         WHERE id = ?1 AND status = 'queued' AND NOT deferred
           AND NOT EXISTS (SELECT 1 FROM turn_steers s WHERE s.turn_id = ?1)
           AND NOT EXISTS (SELECT 1 FROM turns a
                           WHERE a.bot_id = ?2 AND a.status IN {ACTIVE_STATUSES})
           AND NOT {}",
        crate::db::routines::behind_unreviewed_run()
    )))
    .bind(turn)
    .bind(bot)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 1 {
        events::turn_changed(conn, bot, turn).await?;
    }
    Ok(updated == 1)
}

/// The next step of an attempt toward its provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// `preparing` to `sending`: the request goes out.
    Sending,
    /// `sending` to `running`: the provider accepted it.
    Running,
}

impl Step {
    pub fn source(self) -> TurnStatus {
        match self {
            Self::Sending => TurnStatus::Preparing,
            Self::Running => TurnStatus::Sending,
        }
    }

    fn target(self) -> TurnStatus {
        match self {
            Self::Sending => TurnStatus::Sending,
            Self::Running => TurnStatus::Running,
        }
    }
}

/// Move `turn` one [`Step`] on. False when it is no longer in the step's
/// source state.
pub async fn advance(
    conn: &mut SqliteConnection,
    turn: Uuid,
    step: Step,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE turns SET status = ?2, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = ?3",
    )
    .bind(turn)
    .bind(step.target())
    .bind(step.source())
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// Fail `turn` of `bot` claimed but never started: nothing reached a
/// provider.
pub async fn fail_unstarted(
    conn: &mut SqliteConnection,
    bot: Uuid,
    turn: Uuid,
    message: &str,
) -> Result<(), sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE turns SET status = 'failed', error_code = 'start_failed', error_message = ?2,
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'preparing'",
    )
    .bind(turn)
    .bind(message)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 1 {
        ended(conn, bot, turn).await?;
        settled(conn, bot).await?;
    }
    Ok(())
}

/// Queue a waiting turn again as a continuation when the
/// `turns_continuable` view, which the triggers that requeue a turn share,
/// lists it. True when it did.
pub async fn continue_waiting(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let requeued = sqlx::query(
        "UPDATE turns SET status = 'queued', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND id IN (SELECT id FROM turns_continuable WHERE id = ?1)",
    )
    .bind(turn_id)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    Ok(requeued == 1)
}

/// A turn whose every pending hold is a job or a task: the owner's Stop
/// leaves it parked on them. One that also waits on the owner ends.
const ONLY_JOBS_AND_TASKS: &str = "NOT EXISTS (SELECT 1 FROM turn_holds h
         WHERE h.turn_id = turns.id AND h.pending AND h.kind = 'owner')";

/// Park `bot`'s queued turns up to `boundary` that wait only on jobs and
/// tasks, for the owner's Stop. Returns them.
pub async fn park_on_tasks(
    conn: &mut SqliteConnection,
    bot: Uuid,
    boundary: i64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let parked: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "UPDATE turns SET status = 'waiting', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND turn_no <= ?2 AND status = 'queued'
           AND (SELECT coalesce(sum(h.pending), 0) FROM turn_holds h
                WHERE h.turn_id = turns.id) > 0
           AND {ONLY_JOBS_AND_TASKS}
         RETURNING id"
    )))
    .bind(bot)
    .bind(boundary)
    .fetch_all(&mut *conn)
    .await?;
    for &turn in &parked {
        events::turn_changed(conn, bot, turn).await?;
    }
    Ok(parked)
}

/// End `bot`'s queued and waiting turns up to `boundary` with `cancelled`
/// and `message`. With [`Unwind::KeepWaits`] a waiting turn that waits only
/// on jobs and tasks stays parked on them.
pub async fn interrupt_accepted(
    conn: &mut SqliteConnection,
    bot: Uuid,
    boundary: i64,
    unwind: Unwind,
    message: &str,
) -> Result<(), sqlx::Error> {
    let ended_turns: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "UPDATE turns SET status = 'interrupted', error_code = 'cancelled', error_message = ?3,
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND turn_no <= ?2
           AND (status = 'queued' OR (status = 'waiting' AND NOT (?4 AND {ONLY_JOBS_AND_TASKS})))
         RETURNING id"
    )))
    .bind(bot)
    .bind(boundary)
    .bind(message)
    .bind(unwind == Unwind::KeepWaits)
    .fetch_all(&mut *conn)
    .await?;
    for turn in ended_turns {
        ended(conn, bot, turn).await?;
    }
    crate::db::steers::settle(conn).await
}

/// Move `bot`'s active turn up to `boundary` to `cancelling`, unwinding as
/// `unwind` says once its provider stops. Returns it.
pub async fn request_cancel(
    conn: &mut SqliteConnection,
    bot: Uuid,
    boundary: i64,
    unwind: Unwind,
) -> Result<Option<Uuid>, sqlx::Error> {
    let active: Option<Uuid> = sqlx::query_scalar(
        "UPDATE turns SET status = 'cancelling', error_code = ?3,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND turn_no <= ?2 AND status IN ('preparing', 'sending', 'running')
         RETURNING id",
    )
    .bind(bot)
    .bind(boundary)
    .bind(unwind.code())
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(turn) = active {
        events::turn_changed(conn, bot, turn).await?;
    }
    Ok(active)
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

/// End task turn `turn` of `target` with `code`: one that has not started,
/// or waits on its own tasks, is interrupted at once; one in progress moves
/// to `cancelling` for the scheduler to interrupt. Either way the tasks it
/// handed on stop in the same transaction. False when the turn had already
/// ended or was already being fully cancelled.
pub async fn cancel_task(
    conn: &mut SqliteConnection,
    turn: Uuid,
    target: Uuid,
    code: &str,
    message: &str,
) -> Result<bool, sqlx::Error> {
    let status: Option<TurnStatus> = sqlx::query_scalar(
        "UPDATE turns SET
             status = CASE WHEN status IN ('queued', 'waiting') THEN 'interrupted' ELSE 'cancelling' END,
             error_code = ?2,
             error_message = ?3,
             finished_at = CASE WHEN status IN ('queued', 'waiting') THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') ELSE finished_at END,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status IN ('queued', 'waiting', 'preparing', 'sending', 'running')
         RETURNING status",
    )
    .bind(turn)
    .bind(code)
    .bind(message)
    .fetch_optional(&mut *conn)
    .await?;
    match status {
        // An idle turn leaves no active turn behind, so the target's stop
        // stays as it was; only the messages joined to it settle.
        Some(TurnStatus::Interrupted) => {
            ended(conn, target, turn).await?;
            crate::db::steers::settle(conn).await?;
        }
        // An owner's Stop of its attempt alone becomes a full cancel.
        Some(_) => cancelled(conn, target, turn).await?,
        None if end_all(conn, turn, code, message).await? => cancelled(conn, target, turn).await?,
        None => return Ok(false),
    }
    Ok(true)
}

/// Publish a turn that now ends with a cancel, and stop its tasks now, not
/// when its provider stops, so none starts meanwhile.
async fn cancelled(conn: &mut SqliteConnection, bot: Uuid, turn: Uuid) -> Result<(), sqlx::Error> {
    events::turn_changed(conn, bot, turn).await?;
    Box::pin(crate::db::delegations::cancel_for_root(conn, turn)).await
}
/// Record how attempt `attempt` of `turn` (of `bot`) ended with `outcome`,
/// inside the caller's immediate transaction. The turn ends, parks, or
/// queues its next attempt as [`settle`] decides.
pub async fn finish_attempt(
    conn: &mut SqliteConnection,
    bot: Uuid,
    turn: Uuid,
    attempt: i32,
    outcome: &TurnOutcome,
) -> Result<(), sqlx::Error> {
    crate::db::delegations::lock_targets_of_root(conn, turn).await?;
    sqlx::query("SELECT 1 FROM turns WHERE id = ?1")
        .bind(turn)
        .execute(&mut *conn)
        .await?;
    let (current, error_code, pending, blocking, tasks_unseen, waits_unseen, owner_unseen): (
        TurnStatus,
        Option<String>,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = sqlx::query_as(
        "SELECT t.status, t.error_code, coalesce(sum(h.pending), 0),
             coalesce(sum(h.pending AND h.kind = 'owner'), 0),
             coalesce(sum(h.unseen AND h.kind = 'task'), 0),
             coalesce(sum(h.unseen AND h.kind = 'wait'), 0),
             coalesce(sum(h.unseen AND h.kind = 'owner'), 0)
         FROM turns t LEFT JOIN turn_holds h ON h.turn_id = t.id
         WHERE t.id = ?1 GROUP BY t.id",
    )
    .bind(turn)
    .fetch_one(&mut *conn)
    .await?;
    let ready: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM delegations d WHERE d.root_turn_id = ?1 AND {}",
        crate::db::delegations::READY
    )))
    .bind(turn)
    .fetch_one(&mut *conn)
    .await?;
    let cancel = (current == TurnStatus::Cancelling).then(|| Unwind::decode(error_code.as_deref()));
    let cut = if limit_cut(outcome, cancel) {
        crate::db::limit_cuts::cut(conn, turn, bot, i64::from(attempt)).await?
    } else {
        None
    };
    let joined = crate::db::steers::waiting_for(conn, turn).await? > 0;
    let disposition = settle(&Facts {
        outcome,
        cancel,
        holds: Holds {
            pending,
            blocking,
            tasks_unseen,
            waits_unseen,
            owner_unseen,
            ready,
        },
        joined,
        cut,
    });
    crate::db::owner_notices::supersede_by_card(conn, turn).await?;
    let (status, keeps_reason, code, message) = match &disposition {
        Disposition::Queued => (TurnStatus::Queued, false, None, None),
        Disposition::Waiting => (TurnStatus::Waiting, false, None, None),
        Disposition::Ended(End {
            status,
            reason: Reason::Kept,
        }) => (*status, true, None, None),
        Disposition::Ended(End {
            status,
            reason: Reason::Set { code, message },
        }) => (*status, false, code.as_deref(), message.as_deref()),
    };
    let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE turns SET status = ?2,
             error_code = CASE WHEN ?7 THEN error_code ELSE ?3 END,
             error_message = CASE WHEN ?7 THEN error_message ELSE ?4 END,
             effect_uncertain = effect_uncertain OR ?5,
             finished_at = CASE WHEN ?6 THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status IN {ACTIVE_STATUSES}"
    )))
    .bind(turn)
    .bind(status)
    .bind(code)
    .bind(message)
    .bind(matches!(outcome, TurnOutcome::Uncertain { .. }))
    .bind(status.is_ended())
    .bind(keeps_reason)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    let refused = matches!(
        outcome,
        TurnOutcome::Failed {
            code: FailureCode::ProviderRefused,
            ..
        }
    );
    // Only a handoff in flight changes. One still prepared never left, and
    // an accepted or uncertain one already holds its outcome, so a refusal
    // or a turn without a handoff needs nothing here.
    crate::db::handoffs::unacknowledged(conn, turn, attempt, refused).await?;
    if updated == 1 {
        if status.is_ended() {
            ended(conn, bot, turn).await?;
        } else {
            events::turn_changed(conn, bot, turn).await?;
            crate::db::memory::log_turn_end(conn, turn).await?;
        }
    }
    settled(conn, bot).await
}

/// How a joined owner message ends with the reply it joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Steered {
    /// It was sent into the reply, which did not confirm it.
    Unconfirmed,
    /// The reply took it and ended: it ends the same way.
    WithReply { into: Uuid },
}

/// End queued message `turn` of `bot` that joined another reply.
pub async fn end_steered(
    conn: &mut SqliteConnection,
    bot: Uuid,
    turn: Uuid,
    steered: Steered,
) -> Result<(), sqlx::Error> {
    let updated = match steered {
        Steered::Unconfirmed => sqlx::query(
            "UPDATE turns SET status = 'uncertain', effect_uncertain = true,
                 error_code = 'steer_uncertain',
                 error_message = 'This message was sent into the reply in progress, which \
did not confirm it. It was not resent; send a follow-up to continue.',
                 finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status = 'queued'",
        )
        .bind(turn),
        Steered::WithReply { into } => sqlx::query(
            "UPDATE turns SET status = a.status, error_code = a.error_code,
                 error_message = a.error_message,
                 effect_uncertain = turns.effect_uncertain OR a.effect_uncertain,
                 finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             FROM turns a
             WHERE turns.id = ?1 AND turns.status = 'queued' AND a.id = ?2",
        )
        .bind(turn)
        .bind(into),
    }
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if updated == 1 {
        ended(conn, bot, turn).await?;
    }
    Ok(())
}
