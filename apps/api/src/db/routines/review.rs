//! Reviewing ended routine runs: forget, retry, or count each failure and
//! tell the owner.

use chrono::{DateTime, Utc};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::app::providers::FailureCode;
use crate::app::routines::{AfterCheck, RoutineModel, Schedule, run_prompt};
use crate::db::events;
use crate::db::turns::{ENDED_STATUSES, TurnStatus};
use crate::providers::Registry;

use super::checks::CheckedJob;
use super::{ActiveColumns, RunOn, queue_saved_task, ts};

/// Ended routine runs read per query of [`review_ended`].
const REVIEW_BATCH: i64 = 8;

/// Failed reviews after which [`review_ended`] gives up on a run.
pub const REVIEW_ATTEMPTS: i64 = 5;

/// What an ended, unreviewed run needs next. The stored count of failed
/// reviews decides it, so giving up survives a restart.
enum Pending {
    Review,
    Release,
}

impl Pending {
    fn of(review_failures: i64) -> Self {
        if review_failures >= REVIEW_ATTEMPTS {
            Self::Release
        } else {
            Self::Review
        }
    }
}

/// SQL condition on `turns t`: an earlier turn of its conversation ran a
/// routine that ended and is not reviewed yet. The review writes the
/// failure notice and session rewind `t` must read, so `t` is not claimed
/// until it runs.
pub(crate) fn behind_unreviewed_run() -> String {
    format!(
        "EXISTS (SELECT 1 FROM routine_runs rr JOIN turns u ON u.id = rr.turn_id
                 WHERE u.conversation_id = t.conversation_id AND u.turn_no < t.turn_no
                   AND NOT rr.reviewed AND u.status IN {ENDED_STATUSES})"
    )
}

/// Handle every routine run whose turn has ended, per
/// [`routine_failures::react`]: forget failures after a success, switch a
/// paused routine back on after the owner's retry succeeds, retry a
/// transient failure once, or count the failure and tell the owner in the
/// failed turn, pausing the routine when retrying cannot help or its runs
/// keep failing. Each run is reviewed once, in one transaction with all it
/// causes, whichever path ended its turn. All of them are handled in one
/// pass, so their retries queue before the pass's watcher wakes. A run
/// whose review fails is left for the next pass, and the others still go
/// ahead, since each holds back its conversation's next turn
/// ([`behind_unreviewed_run`]). Once reviews of a run have failed
/// [`REVIEW_ATTEMPTS`] times it is released instead ([`release`]), also
/// after a restart. Returns how many retries were queued. The
/// caller holds a maintenance admission.
pub async fn review_ended(
    pool: &SqlitePool,
    registry: &Registry,
    now: DateTime<Utc>,
) -> Result<usize, sqlx::Error> {
    let mut tried: Vec<Uuid> = Vec::new();
    let mut retried = 0;
    loop {
        let ended: Vec<(Uuid, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT rr.id, rr.review_failures FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
             WHERE NOT rr.reviewed AND rr.turn_id IS NOT NULL
               AND t.status IN {ENDED_STATUSES} AND NOT {}
             ORDER BY rr.due_at, rr.id LIMIT ?2",
            crate::db::in_ids("rr.id", "?1")
        )))
        .bind(Json(&tried))
        .bind(REVIEW_BATCH)
        .fetch_all(pool)
        .await?;
        if ended.is_empty() {
            return Ok(retried);
        }
        if tried.is_empty() {
            crate::providers::recheck_unavailable(registry).await;
        }
        for (id, failures) in ended {
            tried.push(id);
            if let Pending::Release = Pending::of(failures) {
                give_up(pool, id, None).await;
                continue;
            }
            match review(pool, registry, id, now).await {
                Ok(true) => retried += 1,
                Ok(false) => {}
                Err(err) => review_failed(pool, id, &err).await,
            }
        }
    }
}

/// Count a failed attempt to review or release run `id`. None when the
/// run is reviewed by now or the count could not be written.
async fn count_failure(pool: &SqlitePool, id: Uuid) -> Option<i64> {
    sqlx::query_scalar(
        "UPDATE routine_runs SET review_failures = review_failures + 1
         WHERE id = ?1 AND NOT reviewed RETURNING review_failures",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|err| {
        tracing::warn!(run = %id, error = %err, "counting a failed routine review failed");
        None
    })
}

/// Count a failed review of run `id`, and give up on the run once reviews
/// have failed [`REVIEW_ATTEMPTS`] times. Warns on the first failure only.
async fn review_failed(pool: &SqlitePool, id: Uuid, err: &sqlx::Error) {
    let Some(failures) = count_failure(pool, id).await else {
        return;
    };
    if let Pending::Release = Pending::of(failures) {
        give_up(pool, id, Some(err)).await;
    } else if failures == 1 {
        tracing::warn!(run = %id, error = %err, "reviewing an ended routine run failed; retrying");
    } else {
        tracing::debug!(run = %id, error = %err, failures, "reviewing an ended routine run failed");
    }
}

/// Release run `id` after its reviews kept failing with `cause`. A failed
/// release is counted too, and warns only when the count is a power of two.
async fn give_up(pool: &SqlitePool, id: Uuid, cause: Option<&sqlx::Error>) {
    let cause = cause.map(ToString::to_string);
    match release(pool, id).await {
        Ok(true) => tracing::warn!(run = %id, error = ?cause,
            "gave up reviewing an ended routine run; released its conversation"),
        Ok(false) => {}
        Err(err) => match count_failure(pool, id).await {
            Some(failures) if failures.count_ones() != 1 => tracing::debug!(run = %id,
                error = %err, failures, "releasing an ended routine run failed"),
            failures => tracing::warn!(run = %id, error = %err, ?failures,
                "releasing an ended routine run failed"),
        },
    }
}

/// Mark run `id` reviewed without reacting to its outcome, once its
/// reviews have failed [`REVIEW_ATTEMPTS`] times, then tell the owner.
/// Reads no column whose stored value could fail to decode. The notice is
/// written after the release commits and is best effort, since a notice
/// that cannot be written may be why the review failed. False when the run
/// is reviewed or not given up.
async fn release(pool: &SqlitePool, id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<Released> = sqlx::query_as(
        "SELECT rr.turn_id, t.turn_no, r.bot_id, r.conversation_id, rr.name,
                r.run_at IS NOT NULL AS reminder,
                r.deleted_at IS NULL AND b.status = 'active' AS live
         FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
         JOIN routines r ON r.id = rr.routine_id JOIN bots b ON b.id = r.bot_id
         WHERE rr.id = ?1 AND NOT rr.reviewed AND rr.review_failures >= ?2",
    )
    .bind(id)
    .bind(REVIEW_ATTEMPTS)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(run) = row else {
        return Ok(false);
    };
    let marked =
        sqlx::query("UPDATE routine_runs SET reviewed = true WHERE id = ?1 AND NOT reviewed")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    if marked != 1 {
        return Ok(false);
    }
    tx.commit().await?;
    if run.live
        && let Err(err) = announce_release(pool, id, &run).await
    {
        tracing::warn!(run = %id, error = %err, "telling the owner about a released routine run failed");
    }
    Ok(true)
}

#[derive(sqlx::FromRow)]
struct Released {
    turn_id: Uuid,
    turn_no: i64,
    bot_id: Uuid,
    conversation_id: Uuid,
    name: String,
    reminder: bool,
    live: bool,
}

/// Post the notice about released run `id`. A quiet run that reported no
/// news reports news instead, so its turn no longer folds the notice away.
async fn announce_release(pool: &SqlitePool, id: Uuid, run: &Released) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let unfolded =
        sqlx::query("UPDATE routine_runs SET report = 'news' WHERE id = ?1 AND report = 'no_news'")
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    let text = crate::app::routine_failures::review_failed_notice(&run.name, run.reminder);
    post_notice(
        &mut tx,
        run.bot_id,
        run.conversation_id,
        run.turn_id,
        run.turn_no,
        &text,
    )
    .await?;
    if unfolded > 0 {
        events::turn_changed(&mut tx, run.bot_id, run.turn_id).await?;
    } else {
        events::bot_updated(&mut tx, run.bot_id).await?;
    }
    tx.commit().await
}

/// Add the bot's notice `text` to `turn_id`, and rewind warm sessions that
/// already considered that turn so their next turn sees it.
async fn post_notice(
    tx: &mut SqliteConnection,
    bot_id: Uuid,
    conversation_id: Uuid,
    turn_id: Uuid,
    turn_no: i64,
    text: &str,
) -> Result<(), sqlx::Error> {
    let (seq,): (i64,) = sqlx::query_as(
        "UPDATE conversations SET last_item_seq = last_item_seq + 1 WHERE id = ?1
         RETURNING last_item_seq",
    )
    .bind(conversation_id)
    .fetch_one(&mut *tx)
    .await?;
    let item_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, ?3, ?4, 'assistant', ?5)",
    )
    .bind(item_id)
    .bind(conversation_id)
    .bind(seq)
    .bind(turn_id)
    .bind(text)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE provider_sessions SET considered_through_turn = ?2 - 1
         WHERE conversation_id = ?1 AND status = 'active' AND considered_through_turn >= ?2",
    )
    .bind(conversation_id)
    .bind(turn_no)
    .execute(&mut *tx)
    .await?;
    events::item_upsert(tx, bot_id, item_id).await
}

#[derive(sqlx::FromRow)]
struct EndedRun {
    routine_id: Uuid,
    turn_id: Uuid,
    turn_no: i64,
    due_at: DateTime<Utc>,
    retry: bool,
    resumes: bool,
    status: TurnStatus,
    error_code: Option<FailureCode>,
    error_message: Option<String>,
    bot_id: Uuid,
    conversation_id: Uuid,
    name: String,
    prompt: String,
    prompt_revision: i64,
    cron: String,
    timezone: String,
    #[sqlx(flatten)]
    active: ActiveColumns,
    enabled: bool,
    paused: bool,
    next_due_at: Option<DateTime<Utc>>,
    model: Option<Json<RoutineModel>>,
    failed_runs: i64,
    reminder: bool,
    live: bool,
    may_retry: bool,
    check_job_id: Option<Uuid>,
    #[sqlx(flatten)]
    check: CheckedJob,
}

async fn review(
    pool: &SqlitePool,
    registry: &Registry,
    run_id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    use crate::app::routine_failures::{self, Attempt, Ended, Notice, Pause, Reaction, Run};
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<EndedRun> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT rr.routine_id, rr.turn_id, t.turn_no, rr.due_at, rr.retry_of IS NOT NULL AS retry,
                rr.resumes, t.status, t.error_code, t.error_message,
                r.bot_id, r.conversation_id, r.name, r.prompt, r.prompt_revision, r.cron, r.timezone,
                r.active_days, r.active_start, r.active_end,
                r.enabled, NOT r.enabled AND r.paused_reason IS NOT NULL AS paused,
                r.next_due_at, r.model, r.failed_runs, r.run_at IS NOT NULL AS reminder,
                r.deleted_at IS NULL AND b.status = 'active' AS live,
                rr.check_job_id, {checked},
                NOT b.cancel_pending AND NOT EXISTS (
                    SELECT 1 FROM request_receipts s WHERE s.operation = 'cancel'
                      AND s.bot_id = r.bot_id AND s.boundary_turn_no >= t.turn_no)
                AND NOT EXISTS (
                    SELECT 1 FROM routine_runs x JOIN turns u ON u.id = x.turn_id
                    WHERE x.routine_id = r.id AND x.id <> rr.id
                      AND u.status NOT IN {ENDED_STATUSES})
                AND NOT EXISTS (
                    SELECT 1 FROM routine_runs c WHERE c.routine_id = r.id
                      AND c.turn_id IS NULL AND c.skip_reason IS NULL)
                    AS may_retry
         FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
         JOIN routines r ON r.id = rr.routine_id JOIN bots b ON b.id = r.bot_id
         LEFT JOIN guest_jobs j ON j.id = rr.check_job_id
         WHERE rr.id = ?1 AND NOT rr.reviewed",
        checked = CheckedJob::COLUMNS
    )))
    .bind(run_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(run) = row else {
        return Ok(false);
    };
    let Some(ended) = Ended::of(run.status, run.error_code.as_ref()) else {
        return Ok(false);
    };
    sqlx::query("UPDATE routine_runs SET reviewed = true WHERE id = ?1")
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
    if !run.live {
        tx.commit().await?;
        return Ok(false);
    }
    let attempt = match (run.retry, run.resumes) {
        (true, _) => Attempt::Retry,
        (false, true) => Attempt::Resume,
        (false, false) => Attempt::First,
    };
    let state = Run {
        attempt,
        enabled: run.enabled,
        paused: run.paused,
        failed_runs: run.failed_runs,
    };
    let mut reaction = routine_failures::react(ended, state);
    let note = run.check.end().and_then(|end| match end.after() {
        AfterCheck::Wake(note) => Some(note),
        AfterCheck::Skip(_) => None,
    });
    if reaction == Reaction::Retry {
        let retry_id = Uuid::new_v4();
        let run_on = RunOn::routine(run.routine_id, &run.model);
        let in_hours = run.active.hours().is_none_or(|hours| {
            run.timezone
                .parse::<chrono_tz::Tz>()
                .is_ok_and(|tz| hours.contains(&now.with_timezone(&tz)))
        });
        let queued = if run.may_retry && in_hours {
            queue_saved_task(
                &mut tx,
                registry,
                run.bot_id,
                run.conversation_id,
                retry_id,
                b"routine-retry\0",
                &run_prompt(&run.prompt, note.as_deref()),
                run_on,
            )
            .await?
            .ok()
        } else {
            None
        };
        if let Some(turn_id) = queued {
            // Later than the run it repeats, so it is the routine's last run.
            let due = now.max(run.due_at + chrono::Duration::milliseconds(1));
            sqlx::query(
                "INSERT INTO routine_runs
                     (id, routine_id, due_at, name, turn_id, prompt_revision, retry_of, check_job_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )
            .bind(retry_id)
            .bind(run.routine_id)
            .bind(ts(due))
            .bind(&run.name)
            .bind(turn_id)
            .bind(run.prompt_revision)
            .bind(run_id)
            .bind(run.check_job_id)
            .execute(&mut *tx)
            .await?;
            events::turn_accepted(&mut tx, run.bot_id, turn_id).await?;
            tx.commit().await?;
            return Ok(true);
        }
        reaction = routine_failures::react(
            ended,
            Run {
                attempt: Attempt::Retry,
                ..state
            },
        );
    }
    // Reviewing an owner's retry ends its `retrying` flag.
    let mut updated = run.resumes;
    let paused = match reaction {
        Reaction::Retry | Reaction::Ignore => None,
        Reaction::Reset | Reaction::Resume if run.reminder => {
            sqlx::query(
                "UPDATE routines SET enabled = false, next_due_at = NULL,
                     deleted_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1",
            )
            .bind(run.routine_id)
            .execute(&mut *tx)
            .await?;
            updated = true;
            None
        }
        Reaction::Reset => {
            let reset = sqlx::query(
                "UPDATE routines SET failed_runs = 0, paused_reason = NULL
                 WHERE id = ?1 AND (failed_runs <> 0 OR paused_reason IS NOT NULL)",
            )
            .bind(run.routine_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            updated |= reset > 0;
            None
        }
        Reaction::Resume => {
            let next = Schedule::stored(&run.cron, None, &run.timezone, run.active.hours())
                .ok()
                .and_then(|schedule| schedule.next_after(now));
            sqlx::query(
                "UPDATE routines SET enabled = ?2 IS NOT NULL, next_due_at = ?2, failed_runs = 0,
                     paused_reason = NULL,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1",
            )
            .bind(run.routine_id)
            .bind(next.map(ts))
            .execute(&mut *tx)
            .await?;
            updated = true;
            None
        }
        Reaction::Report => Some(None),
        Reaction::Pause(pause) | Reaction::StayPaused(pause) => Some(Some(match pause {
            Pause::NeedsAction => "needs_action",
            Pause::Repeated => "repeated",
        })),
    };
    if let Some(paused) = paused {
        let failed_runs = run.failed_runs + 1;
        let enabled = run.enabled && paused.is_none();
        sqlx::query(
            "UPDATE routines SET failed_runs = ?2, paused_reason = coalesce(?3, paused_reason),
                 enabled = ?4, next_due_at = CASE WHEN ?4 THEN next_due_at END,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1",
        )
        .bind(run.routine_id)
        .bind(failed_runs)
        .bind(paused)
        .bind(enabled)
        .execute(&mut *tx)
        .await?;
        let text = routine_failures::notice(
            &Notice {
                name: &run.name,
                ended,
                message: run.error_message.as_deref(),
                retried: attempt == Attempt::Retry,
                enabled,
                failed_runs,
                next: run
                    .next_due_at
                    .map(|at| routine_failures::local_time(at, &run.timezone)),
                reminder: run.reminder,
            },
            reaction,
        );
        post_notice(
            &mut tx,
            run.bot_id,
            run.conversation_id,
            run.turn_id,
            run.turn_no,
            &text,
        )
        .await?;
        updated = true;
    }
    if updated {
        events::bot_updated(&mut tx, run.bot_id).await?;
    }
    tx.commit().await?;
    Ok(false)
}
