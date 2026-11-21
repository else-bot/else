//! A turn's life, and the only code that writes `turns.status`.
//!
//! Each transition is a named function whose `WHERE` clause checks the
//! states it leaves. The `turns_*` triggers in the migrations are part of
//! these transitions. A turn that ends runs [`ended`], so every end has the
//! same follow-up; a caller that changed an active turn then settles the
//! bot's pending stop ([`crate::db::stop::settle`]) and joined messages
//! ([`crate::db::steers::settle`]) once, at the end of its transaction.

use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{ACTIVE_STATUSES, TurnStatus, UNFINISHED_STATUSES};
use crate::app::providers::{FailureCode, TurnOutcome};
use crate::db::events;
use crate::db::in_ids;
use crate::db::limit_cuts::Cut;

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
    Ok(())
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
        Some(TurnStatus::Interrupted) => ended(conn, target, turn).await?,
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

/// What decides how an attempt leaves its turn.
#[derive(Debug, Clone)]
pub struct Facts<'a> {
    pub outcome: &'a TurnOutcome,
    /// How the turn unwinds, when it is `cancelling`.
    pub cancel: Option<Unwind>,
    pub holds: Holds,
    /// An owner message joined the reply and was not taken yet.
    pub joined: bool,
    /// What the attempt limit did, when it cut this attempt.
    pub cut: Option<Cut>,
}

/// Counts over the turn's `turn_holds` and its delegations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Holds {
    pub pending: i64,
    /// Pending holds on the owner.
    pub blocking: i64,
    pub tasks_unseen: i64,
    pub waits_unseen: i64,
    pub owner_unseen: i64,
    /// Settled task batches ([`crate::db::delegations::READY`]).
    pub ready: i64,
}

/// Where an attempt leaves its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// It continues in a new attempt.
    Queued,
    /// It parks until its holds settle.
    Waiting,
    Ended(End),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct End {
    pub status: TurnStatus,
    pub reason: Reason,
}

/// The `error_code` and `error_message` an ended turn records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The cancel's own reason stays.
    Kept,
    Set {
        code: Option<String>,
        message: Option<String>,
    },
}

/// Whether the attempt limit decides this attempt's end: it cut the
/// attempt and the owner did not stop the turn first.
pub fn limit_cut(outcome: &TurnOutcome, cancel: Option<Unwind>) -> bool {
    is_deadline(outcome) && cancel.is_none()
}

fn is_deadline(outcome: &TurnOutcome) -> bool {
    matches!(
        outcome,
        TurnOutcome::Failed {
            code: FailureCode::TurnDeadline,
            ..
        }
    )
}

/// Decide where an attempt with `facts` leaves its turn. A waiting turn
/// continues by the rule of the `turns_continuable` view.
pub fn settle(facts: &Facts) -> Disposition {
    let Holds {
        pending,
        blocking,
        tasks_unseen,
        waits_unseen,
        owner_unseen,
        ready,
    } = facts.holds;
    let (status, code, message) = match facts.outcome {
        TurnOutcome::Completed => (TurnStatus::Completed, None, None),
        TurnOutcome::Interrupted => (TurnStatus::Interrupted, None, None),
        TurnOutcome::Failed { code, message } => (
            TurnStatus::Failed,
            Some(code.as_str()),
            Some(message.clone()),
        ),
        TurnOutcome::Uncertain { message } => (
            TurnStatus::Uncertain,
            Some("delivery_uncertain"),
            Some(message.clone()),
        ),
    };
    let calm = matches!(status, TurnStatus::Completed | TurnStatus::Interrupted);
    // The owner stopped only this attempt: the turn parks on its jobs and
    // tasks, or continues with their outcomes. What the attempt itself left
    // unseen (a withdrawn question) restarts nothing. With nothing to park
    // on, the attempt's own outcome stands.
    let kept = facts.cancel == Some(Unwind::KeepWaits)
        && (calm || pending > 0 || waits_unseen + tasks_unseen > 0);
    let owner_unseen = if kept { 0 } else { owner_unseen };
    let unseen = tasks_unseen + waits_unseen + owner_unseen;
    // An attempt the limit cut continues in a new one, unless a cap is
    // reached. The owner's stop wins: the cut is the interruption it asked
    // for.
    let stopped = is_deadline(facts.outcome) && facts.cancel.is_some();
    let continued = facts.cut == Some(Cut::Continued);
    let (status, code, message) = if kept || continued || stopped {
        (TurnStatus::Interrupted, None, None)
    } else {
        let capped = match facts.cut {
            Some(Cut::Capped { continued }) => {
                Some(crate::providers::cli::attempt::limit_message(continued))
            }
            _ => None,
        };
        (status, code, capped.or(message))
    };
    let parks = (kept || facts.cancel.is_none())
        && matches!(status, TurnStatus::Completed | TurnStatus::Interrupted);
    // Pending tasks and waits hold the turn without blocking a ready batch
    // or any other unseen outcome, as `turns_continuable` decides for a
    // waiting turn. A joined owner message continues the reply now,
    // whatever it waits on.
    let own_outcome = ready > 0 || waits_unseen + owner_unseen > 0;
    if parks && (facts.joined || (blocking == 0 && own_outcome)) {
        Disposition::Queued
    } else if parks && pending > 0 {
        Disposition::Waiting
    } else if parks && (unseen > 0 || continued) {
        Disposition::Queued
    } else if facts.cancel.is_some() && status == TurnStatus::Interrupted {
        Disposition::Ended(End {
            status,
            reason: Reason::Kept,
        })
    } else {
        Disposition::Ended(End {
            status,
            reason: Reason::Set {
                code: code.map(str::to_owned),
                message,
            },
        })
    }
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
    crate::db::stop::settle(conn, bot).await?;
    crate::db::steers::settle(conn).await
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
    let mut settled = 0;
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
            crate::db::stop::settle(&mut tx, bot_id).await?;
            crate::db::steers::settle(&mut tx).await?;
            settled += 1;
        }
        tx.commit().await?;
    }
    Ok(settled)
}

/// End the unfinished turns that restored email deliveries started, on a
/// restored database before any API serves it, and stop the tasks they
/// delegated, in one transaction. A turn that may have acted becomes
/// `uncertain`.
pub async fn end_restored_mail(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let mut tx = sqlx::Connection::begin_with(conn, "BEGIN IMMEDIATE").await?;
    let restored: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!("UPDATE turns SET status=CASE WHEN status IN ('queued','preparing') THEN 'interrupted' ELSE 'uncertain' END, effect_uncertain=effect_uncertain OR status NOT IN ('queued','preparing'), error_code='connector_restored', error_message='Restored email work will not resume automatically. Inspect action receipts for prior effects.', finished_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), updated_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE status IN {UNFINISHED_STATUSES} AND EXISTS (SELECT 1 FROM connector_deliveries d WHERE d.turn_id=turns.id) RETURNING id, bot_id")))
        .fetch_all(&mut *tx)
        .await?;
    for (turn, bot) in restored {
        ended(&mut tx, bot, turn).await?;
    }
    tx.commit().await
}

/// End the queued and waiting turns that restored social deliveries
/// started, as [`end_restored_mail`] does for email.
pub async fn end_restored_social(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE turns SET status='interrupted', error_code='social_restored', error_message='Restored social work will not resume automatically.', finished_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now'), updated_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE status IN ('queued','waiting') AND id IN (SELECT turn_id FROM social_deliveries)")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::{SqliteConnection, SqlitePool};
    use uuid::Uuid;

    use super::{Disposition, End, Facts, Holds, Reason, Steered, Step, Unwind};
    use crate::app::providers::{FailureCode, TurnOutcome};
    use crate::db::limit_cuts::Cut;
    use crate::db::turns::TurnStatus;

    const LIMIT_AFTER_TWO: &str = "Stopped at the 30-minute attempt limit after continuing 2 times. Background jobs it started may still be running.";

    fn ended(status: TurnStatus, code: Option<&str>, message: Option<&str>) -> Disposition {
        Disposition::Ended(End {
            status,
            reason: Reason::Set {
                code: code.map(str::to_owned),
                message: message.map(str::to_owned),
            },
        })
    }

    fn kept(status: TurnStatus) -> Disposition {
        Disposition::Ended(End {
            status,
            reason: Reason::Kept,
        })
    }

    #[test]
    fn settle_decides_each_attempt_end() {
        let completed = TurnOutcome::Completed;
        let interrupted = TurnOutcome::Interrupted;
        let failed = TurnOutcome::failed(FailureCode::ProviderFailed, "boom");
        let deadline = TurnOutcome::failed(FailureCode::TurnDeadline, "cut");
        let uncertain = TurnOutcome::Uncertain {
            message: "lost".into(),
        };
        let task = Holds {
            pending: 1,
            ..Holds::default()
        };
        let cases: Vec<(&str, Facts, Disposition)> = vec![
            (
                "a finished attempt with nothing held ends completed",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                ended(TurnStatus::Completed, None, None),
            ),
            (
                "a pending task parks the turn",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: task,
                    joined: false,
                    cut: None,
                },
                Disposition::Waiting,
            ),
            (
                "a settled wait continues it while a task runs",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds {
                        waits_unseen: 1,
                        ..task
                    },
                    joined: false,
                    cut: None,
                },
                Disposition::Queued,
            ),
            (
                "a pending owner card blocks that continuation",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds {
                        blocking: 1,
                        waits_unseen: 1,
                        ..task
                    },
                    joined: false,
                    cut: None,
                },
                Disposition::Waiting,
            ),
            (
                "a joined owner message continues past a pending card",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds {
                        blocking: 1,
                        ..task
                    },
                    joined: true,
                    cut: None,
                },
                Disposition::Queued,
            ),
            (
                "an unseen task outcome continues the turn",
                Facts {
                    outcome: &completed,
                    cancel: None,
                    holds: Holds {
                        tasks_unseen: 1,
                        ..Holds::default()
                    },
                    joined: false,
                    cut: None,
                },
                Disposition::Queued,
            ),
            (
                "a failure ends the turn with its own reason",
                Facts {
                    outcome: &failed,
                    cancel: None,
                    holds: task,
                    joined: false,
                    cut: None,
                },
                ended(TurnStatus::Failed, Some("provider_failed"), Some("boom")),
            ),
            (
                "an uncertain delivery ends uncertain",
                Facts {
                    outcome: &uncertain,
                    cancel: None,
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                ended(
                    TurnStatus::Uncertain,
                    Some("delivery_uncertain"),
                    Some("lost"),
                ),
            ),
            (
                "a limit cut under the cap continues",
                Facts {
                    outcome: &deadline,
                    cancel: None,
                    holds: Holds::default(),
                    joined: false,
                    cut: Some(Cut::Continued),
                },
                Disposition::Queued,
            ),
            (
                "a limit cut at the cap fails with the limit message",
                Facts {
                    outcome: &deadline,
                    cancel: None,
                    holds: Holds::default(),
                    joined: false,
                    cut: Some(Cut::Capped { continued: 2 }),
                },
                ended(
                    TurnStatus::Failed,
                    Some("turn_deadline"),
                    Some(LIMIT_AFTER_TWO),
                ),
            ),
            (
                "a limit cut during a cancel keeps the cancel's reason",
                Facts {
                    outcome: &deadline,
                    cancel: Some(Unwind::EndAll),
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                kept(TurnStatus::Interrupted),
            ),
            (
                "the owner's Stop parks the turn on its task",
                Facts {
                    outcome: &interrupted,
                    cancel: Some(Unwind::KeepWaits),
                    holds: task,
                    joined: false,
                    cut: None,
                },
                Disposition::Waiting,
            ),
            (
                "the owner's Stop with a withdrawn question only ends interrupted",
                Facts {
                    outcome: &completed,
                    cancel: Some(Unwind::KeepWaits),
                    holds: Holds {
                        owner_unseen: 1,
                        ..Holds::default()
                    },
                    joined: false,
                    cut: None,
                },
                kept(TurnStatus::Interrupted),
            ),
            (
                "a failure during the owner's Stop with nothing held stands",
                Facts {
                    outcome: &failed,
                    cancel: Some(Unwind::KeepWaits),
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                ended(TurnStatus::Failed, Some("provider_failed"), Some("boom")),
            ),
            (
                "a full cancel ends the turn whatever it waits on",
                Facts {
                    outcome: &interrupted,
                    cancel: Some(Unwind::EndAll),
                    holds: Holds {
                        tasks_unseen: 1,
                        ..task
                    },
                    joined: true,
                    cut: None,
                },
                kept(TurnStatus::Interrupted),
            ),
            (
                "a completion during a full cancel stands",
                Facts {
                    outcome: &completed,
                    cancel: Some(Unwind::EndAll),
                    holds: Holds::default(),
                    joined: false,
                    cut: None,
                },
                ended(TurnStatus::Completed, None, None),
            ),
        ];
        for (name, facts, expected) in cases {
            assert_eq!(super::settle(&facts), expected, "{name}");
        }
    }

    async fn pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::migrate(&pool).await.unwrap();
        pool
    }

    async fn bot(conn: &mut SqliteConnection) -> Uuid {
        let bot = Uuid::new_v4();
        sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Bot')")
            .bind(bot)
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (?1, ?2)")
            .bind(Uuid::new_v4())
            .bind(bot)
            .execute(&mut *conn)
            .await
            .unwrap();
        bot
    }

    async fn turn(conn: &mut SqliteConnection, bot: Uuid, turn_no: i64, status: &str) -> Uuid {
        let turn = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
                 owner_item_id, provider_id, status)
             SELECT ?1, ?2, id, ?3, zeroblob(32), ?4, ?5, 'codex', ?6
             FROM conversations WHERE bot_id = ?2",
        )
        .bind(turn)
        .bind(bot)
        .bind(Uuid::new_v4())
        .bind(turn_no)
        .bind(Uuid::new_v4())
        .bind(status)
        .execute(&mut *conn)
        .await
        .unwrap();
        turn
    }

    /// A queued task `root` of `parent` handed to a new bot. Returns its
    /// child turn and delegation.
    async fn task(conn: &mut SqliteConnection, parent: Uuid, root: Uuid) -> (Uuid, Uuid) {
        let target = bot(conn).await;
        let (conversation, child, delegation) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        sqlx::query("INSERT INTO conversations (id, bot_id, kind) VALUES (?1, ?2, 'task')")
            .bind(conversation)
            .bind(target)
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
                 owner_item_id, provider_id)
             VALUES (?1, ?2, ?3, ?4, zeroblob(32), 1, ?5, 'codex')",
        )
        .bind(child)
        .bind(target)
        .bind(conversation)
        .bind(Uuid::new_v4())
        .bind(Uuid::new_v4())
        .execute(&mut *conn)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO delegations (id, root_turn_id, parent_attempt, parent_bot_id,
                 target_bot_id, task, task_hash, conversation_id, child_turn_id)
             VALUES (?1, ?2, 1, ?3, ?4, 'Check it', zeroblob(32), ?5, ?6)",
        )
        .bind(delegation)
        .bind(root)
        .bind(parent)
        .bind(target)
        .bind(conversation)
        .bind(child)
        .execute(&mut *conn)
        .await
        .unwrap();
        (child, delegation)
    }

    async fn delegation_status(conn: &mut SqliteConnection, delegation: Uuid) -> String {
        sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
            .bind(delegation)
            .fetch_one(&mut *conn)
            .await
            .unwrap()
    }

    async fn row(conn: &mut SqliteConnection, turn: Uuid) -> (TurnStatus, Option<String>, bool) {
        sqlx::query_as(
            "SELECT status, error_code, finished_at IS NOT NULL FROM turns WHERE id = ?1",
        )
        .bind(turn)
        .fetch_one(&mut *conn)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn a_claim_starts_a_queued_turn_only_while_the_bot_is_idle() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let first = turn(&mut conn, bot, 1, "queued").await;
        let second = turn(&mut conn, bot, 2, "queued").await;
        assert!(super::claim(&mut conn, first, bot).await.unwrap());
        assert_eq!(row(&mut conn, first).await.0, TurnStatus::Preparing);
        assert!(
            !super::claim(&mut conn, second, bot).await.unwrap(),
            "the bot already has an active turn"
        );
        assert_eq!(row(&mut conn, second).await.0, TurnStatus::Queued);
    }

    #[tokio::test]
    async fn an_attempt_advances_one_step_at_a_time() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let turn = turn(&mut conn, bot, 1, "preparing").await;
        assert!(
            !super::advance(&mut conn, turn, Step::Running)
                .await
                .unwrap(),
            "running needs sending first"
        );
        assert!(
            super::advance(&mut conn, turn, Step::Sending)
                .await
                .unwrap()
        );
        assert!(
            super::advance(&mut conn, turn, Step::Running)
                .await
                .unwrap()
        );
        assert_eq!(row(&mut conn, turn).await.0, TurnStatus::Running);
    }

    #[tokio::test]
    async fn only_a_preparing_turn_fails_unstarted() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let preparing = turn(&mut conn, bot, 1, "preparing").await;
        let (child, delegation) = task(&mut conn, bot, preparing).await;
        super::fail_unstarted(&mut conn, bot, preparing, "no input")
            .await
            .unwrap();
        assert_eq!(
            row(&mut conn, preparing).await,
            (TurnStatus::Failed, Some("start_failed".into()), true)
        );
        assert_eq!(
            row(&mut conn, child).await,
            (TurnStatus::Interrupted, Some("cancelled".into()), true),
            "the task a continuation left pending stops with it"
        );
        assert_eq!(delegation_status(&mut conn, delegation).await, "cancelled");
        let running = turn(&mut conn, bot, 2, "running").await;
        super::fail_unstarted(&mut conn, bot, running, "no input")
            .await
            .unwrap();
        assert_eq!(
            row(&mut conn, running).await,
            (TurnStatus::Running, None, false)
        );
    }

    #[tokio::test]
    async fn a_waiting_turn_with_nothing_held_continues() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let waiting = turn(&mut conn, bot, 1, "waiting").await;
        let running = turn(&mut conn, bot, 2, "running").await;
        assert!(super::continue_waiting(&mut conn, waiting).await.unwrap());
        assert_eq!(row(&mut conn, waiting).await.0, TurnStatus::Queued);
        assert!(!super::continue_waiting(&mut conn, running).await.unwrap());
        assert_eq!(row(&mut conn, running).await.0, TurnStatus::Running);
    }

    #[tokio::test]
    async fn the_owners_stop_keeps_waits_until_a_full_cancel_replaces_it() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let turn = turn(&mut conn, bot, 1, "running").await;
        assert_eq!(super::unwind(&mut conn, turn).await.unwrap(), None);
        let cancelled = super::request_cancel(&mut conn, bot, 1, Unwind::KeepWaits)
            .await
            .unwrap();
        assert_eq!(cancelled, Some(turn));
        assert_eq!(
            super::unwind(&mut conn, turn).await.unwrap(),
            Some(Unwind::KeepWaits)
        );
        assert!(
            super::end_all(&mut conn, turn, "cancelled", "Deleted.")
                .await
                .unwrap()
        );
        assert_eq!(
            super::unwind(&mut conn, turn).await.unwrap(),
            Some(Unwind::EndAll)
        );
        assert!(
            !super::end_all(&mut conn, turn, "cancelled", "Deleted.")
                .await
                .unwrap(),
            "a full cancel does not widen again"
        );
        assert_eq!(
            super::request_cancel(&mut conn, bot, 1, Unwind::EndAll)
                .await
                .unwrap(),
            None,
            "a cancelling turn is not cancelled again"
        );
    }

    #[tokio::test]
    async fn a_halt_parks_or_ends_accepted_turns_up_to_its_boundary() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let queued = turn(&mut conn, bot, 1, "queued").await;
        let waiting = turn(&mut conn, bot, 2, "waiting").await;
        let (_, delegation) = task(&mut conn, bot, waiting).await;
        let later = turn(&mut conn, bot, 3, "queued").await;
        assert!(
            super::park_on_tasks(&mut conn, bot, 2)
                .await
                .unwrap()
                .is_empty()
        );
        super::interrupt_accepted(&mut conn, bot, 2, Unwind::KeepWaits, "Stopped.")
            .await
            .unwrap();
        assert_eq!(
            row(&mut conn, queued).await,
            (TurnStatus::Interrupted, Some("cancelled".into()), true)
        );
        assert_eq!(
            row(&mut conn, waiting).await.0,
            TurnStatus::Waiting,
            "the owner's Stop leaves a turn parked on jobs and tasks alone"
        );
        assert_eq!(
            delegation_status(&mut conn, delegation).await,
            "queued",
            "and its task keeps going"
        );
        super::interrupt_accepted(&mut conn, bot, 2, Unwind::EndAll, "Deleted.")
            .await
            .unwrap();
        assert_eq!(
            row(&mut conn, waiting).await,
            (TurnStatus::Interrupted, Some("cancelled".into()), true)
        );
        assert_eq!(delegation_status(&mut conn, delegation).await, "cancelled");
        assert_eq!(
            row(&mut conn, later).await.0,
            TurnStatus::Queued,
            "after the boundary"
        );
    }

    #[tokio::test]
    async fn a_task_cancel_ends_an_idle_turn_and_interrupts_a_running_one() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let queued = turn(&mut conn, bot, 1, "queued").await;
        let running = turn(&mut conn, bot, 2, "running").await;
        let done = turn(&mut conn, bot, 3, "completed").await;
        assert!(
            super::cancel_task(&mut conn, queued, bot, "cancelled", "Stop.")
                .await
                .unwrap()
        );
        assert_eq!(
            row(&mut conn, queued).await,
            (TurnStatus::Interrupted, Some("cancelled".into()), true)
        );
        assert!(
            super::cancel_task(&mut conn, running, bot, "cancelled", "Stop.")
                .await
                .unwrap()
        );
        assert_eq!(
            row(&mut conn, running).await,
            (TurnStatus::Cancelling, Some("cancelled".into()), false)
        );
        assert!(
            !super::cancel_task(&mut conn, done, bot, "cancelled", "Stop.")
                .await
                .unwrap()
        );
        assert_eq!(row(&mut conn, done).await.0, TurnStatus::Completed);
    }

    #[tokio::test]
    async fn an_attempt_end_settles_only_an_active_turn() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let running = turn(&mut conn, bot, 1, "running").await;
        super::finish_attempt(&mut conn, bot, running, 1, &TurnOutcome::Completed)
            .await
            .unwrap();
        assert_eq!(
            row(&mut conn, running).await,
            (TurnStatus::Completed, None, true)
        );
        let interrupted = turn(&mut conn, bot, 2, "interrupted").await;
        super::finish_attempt(&mut conn, bot, interrupted, 1, &TurnOutcome::Completed)
            .await
            .unwrap();
        assert_eq!(row(&mut conn, interrupted).await.0, TurnStatus::Interrupted);
    }

    #[tokio::test]
    async fn an_unconfirmed_joined_message_ends_uncertain_once() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let queued = turn(&mut conn, bot, 1, "queued").await;
        let done = turn(&mut conn, bot, 2, "completed").await;
        super::end_steered(&mut conn, bot, queued, Steered::Unconfirmed)
            .await
            .unwrap();
        assert_eq!(
            row(&mut conn, queued).await,
            (TurnStatus::Uncertain, Some("steer_uncertain".into()), true)
        );
        super::end_steered(&mut conn, bot, done, Steered::Unconfirmed)
            .await
            .unwrap();
        assert_eq!(
            row(&mut conn, done).await,
            (TurnStatus::Completed, None, false)
        );
    }

    #[tokio::test]
    async fn a_restore_ends_social_work_with_a_reason() {
        let mut conn = pool().await.acquire().await.unwrap().detach();
        let bot = bot(&mut conn).await;
        let queued = turn(&mut conn, bot, 1, "queued").await;
        let running = turn(&mut conn, bot, 2, "running").await;
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut conn)
            .await
            .unwrap();
        for (event, turn) in [("a", queued), ("b", running)] {
            sqlx::query(
                "INSERT INTO social_deliveries
                     (connection_id, event_id, bot_id, trigger_id, destination, message_id, turn_id)
                 VALUES (?1, ?2, ?3, ?1, 'chat', 'm', ?4)",
            )
            .bind(Uuid::nil())
            .bind(event)
            .bind(bot)
            .bind(turn)
            .execute(&mut conn)
            .await
            .unwrap();
        }
        super::end_restored_social(&mut conn).await.unwrap();
        assert_eq!(
            row(&mut conn, queued).await,
            (
                TurnStatus::Interrupted,
                Some("social_restored".into()),
                true
            )
        );
        assert_eq!(row(&mut conn, running).await.0, TurnStatus::Running);
    }
}
