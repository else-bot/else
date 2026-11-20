//! Watchers and their lifecycle (see [`crate::app::watchers`]). This module
//! owns every write to `watchers.status`:
//!
//! - `waiting` → `checking`: [`admit_due`] reserves the check job.
//! - `checking` → `waiting` or `waking`: [`settle_checks`] reads how the
//!   check job ended.
//! - `waiting` or `checking` → `waking`: the deadline passes
//!   ([`admit_due`]) or the owner stops it ([`stop`]).
//! - `waiting` or `checking` → `ended`: the bot stops it ([`stop`]); it
//!   needs no wake.
//! - `waking` → `ended`: [`deliver_wakes`] queues the bot's wake turn.
//!
//! Every transition names its source states in its `WHERE` and journals
//! `bot.updated`.

use chrono::{DateTime, Duration, Utc};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use super::routines::{Check, RunOn, queue_saved_task, ts};
use crate::app::providers::WatcherRun;
use crate::app::watchers::{CHECK_DURATION, MAX_PER_BOT, Outcome, Status, checked, wake_text};
use crate::computers::ComputerManager;
use crate::computers::jobs::JobStatus;
use crate::providers::Registry;

/// Due watchers admitted, checks settled, and wakes queued per scan.
pub const BATCH: i64 = 8;
/// How long a check waits for a free job slot before it is tried again.
const RETRY: Duration = Duration::minutes(1);

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Watcher {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub created_turn_id: Uuid,
    pub purpose: String,
    pub check_command: String,
    pub every_minutes: i64,
    pub watch_until: DateTime<Utc>,
    pub status: Status,
    pub outcome: Option<Outcome>,
    pub next_check_at: Option<DateTime<Utc>>,
    pub checks: i64,
    pub failures: i64,
    pub last_status: Option<String>,
    pub last_output: Option<String>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

const COLUMNS: &str = "id, bot_id, created_turn_id, purpose, check_command, every_minutes,
    watch_until, status, outcome, next_check_at, checks, failures, last_status, last_output,
    last_checked_at, created_at";

/// A watcher a bot asks for, already validated.
#[derive(Debug, Clone)]
pub struct NewWatcher<'a> {
    pub bot_id: Uuid,
    pub turn_id: Uuid,
    pub purpose: &'a str,
    pub check: &'a str,
    pub prompt: &'a str,
    pub every_minutes: i64,
    pub until: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    Created(Watcher),
    /// A retry of a call that already started this watcher.
    Existing(Watcher),
    TooMany(Vec<Watcher>),
}

/// Start a watcher whose first check is due now, in the bot's owner
/// conversation. The same turn asking again for the same check and purpose
/// gets the watcher it started.
pub async fn start(pool: &SqlitePool, new: NewWatcher<'_>) -> Result<Started, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let existing: Option<Watcher> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM watchers
         WHERE created_turn_id = ?1 AND check_command = ?2 AND purpose = ?3"
    )))
    .bind(new.turn_id)
    .bind(new.check)
    .bind(new.purpose)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(existing) = existing {
        return Ok(Started::Existing(existing));
    }
    let active = list_active(&mut tx, new.bot_id).await?;
    if active.len() as i64 >= MAX_PER_BOT {
        return Ok(Started::TooMany(active));
    }
    let id = Uuid::new_v4();
    let watcher: Watcher = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO watchers (id, bot_id, conversation_id, created_turn_id, purpose, check_command,
             prompt, every_minutes, watch_until, next_check_at)
         SELECT ?1, ?2, c.id, ?3, ?4, ?5, ?6, ?7, ?8, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         FROM conversations c WHERE c.bot_id = ?2 AND c.kind = 'owner'
         RETURNING {COLUMNS}"
    )))
    .bind(id)
    .bind(new.bot_id)
    .bind(new.turn_id)
    .bind(new.purpose)
    .bind(new.check)
    .bind(new.prompt)
    .bind(new.every_minutes)
    .bind(ts(new.until))
    .fetch_one(&mut *tx)
    .await?;
    events::bot_updated(&mut tx, new.bot_id).await?;
    tx.commit().await?;
    Ok(Started::Created(watcher))
}

/// The bot's active watchers, newest first.
pub async fn list_active(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<Vec<Watcher>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM watchers
         WHERE bot_id = ?1 AND status IN ('waiting', 'checking')
         ORDER BY created_at DESC, id"
    )))
    .bind(bot_id)
    .fetch_all(conn)
    .await
}

pub async fn find(
    pool: &SqlitePool,
    bot_id: Uuid,
    id: Uuid,
) -> Result<Option<Watcher>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM watchers WHERE id = ?1 AND bot_id = ?2"
    )))
    .bind(id)
    .bind(bot_id)
    .fetch_optional(pool)
    .await
}

/// Who stops a watcher. The owner's stop wakes the bot; the bot's own
/// stop does not, since it is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopBy {
    Owner,
    Bot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stopped {
    Stopped(Watcher),
    /// It had already ended.
    NotActive(Watcher),
    NotFound,
}

/// Stop an active watcher and request the cancel of its running check.
pub async fn stop(
    pool: &SqlitePool,
    bot_id: Uuid,
    id: Uuid,
    by: StopBy,
) -> Result<Stopped, sqlx::Error> {
    let to = match by {
        StopBy::Owner => Status::Waking,
        StopBy::Bot => Status::Ended,
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let job: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT check_job_id FROM watchers
         WHERE id = ?1 AND bot_id = ?2 AND status IN ('waiting', 'checking')",
    )
    .bind(id)
    .bind(bot_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(job) = job else {
        drop(tx);
        return Ok(match find(pool, bot_id, id).await? {
            Some(watcher) => Stopped::NotActive(watcher),
            None => Stopped::NotFound,
        });
    };
    let watcher = end(
        &mut tx,
        id,
        &[Status::Waiting, Status::Checking],
        to,
        Outcome::Stopped,
    )
    .await?
    .expect("the immediate transaction read it active");
    if let Some(job) = job {
        crate::computers::jobs::request_cancel(&mut *tx, bot_id, Some(job)).await?;
    }
    events::bot_updated(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(Stopped::Stopped(watcher))
}

/// End a watcher in one of `from` as `outcome`. Returns it ended, or
/// `None` when it was in another state.
async fn end(
    conn: &mut SqliteConnection,
    id: Uuid,
    from: &[Status],
    to: Status,
    outcome: Outcome,
) -> Result<Option<Watcher>, sqlx::Error> {
    let from: Vec<&str> = from.iter().map(|s| s.into()).collect();
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE watchers SET status = ?2, outcome = ?3, next_check_at = NULL, check_job_id = NULL,
             ended_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status IN (SELECT value FROM json_each(?4))
         RETURNING {COLUMNS}"
    )))
    .bind(id)
    .bind(to)
    .bind(outcome)
    .bind(sqlx::types::Json(from))
    .fetch_optional(conn)
    .await
}

/// End the watchers whose deadline passed as expired, then reserve a check
/// job for each due watcher of an active bot, up to `limit`. Returns the
/// checks to launch. A check without a free job slot is retried after
/// [`RETRY`]; without computers, at its next interval. The caller holds a
/// maintenance admission.
pub async fn admit_due(
    pool: &SqlitePool,
    computers: Option<&ComputerManager>,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<Vec<Check>, sqlx::Error> {
    let overdue: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM watchers
         WHERE status IN ('waiting', 'checking') AND watch_until <= ?1 LIMIT ?2",
    )
    .bind(ts(now))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for id in overdue {
        expire(pool, id).await?;
    }
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT w.id FROM watchers w JOIN bots b ON b.id = w.bot_id
         WHERE w.status = 'waiting' AND w.next_check_at <= ?1
           AND b.status = 'active' AND NOT b.cancel_pending
         ORDER BY w.next_check_at, w.id LIMIT ?2",
    )
    .bind(ts(now))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut checks = Vec::new();
    for id in due {
        if let Some(check) = admit(pool, computers, id, now).await? {
            checks.push(check);
        }
    }
    Ok(checks)
}

async fn expire(pool: &SqlitePool, id: Uuid) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let job: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT check_job_id FROM watchers WHERE id = ?1 AND status IN ('waiting', 'checking')",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(job) = job else {
        return Ok(());
    };
    let watcher = end(
        &mut tx,
        id,
        &[Status::Waiting, Status::Checking],
        Status::Waking,
        Outcome::Expired,
    )
    .await?
    .expect("the immediate transaction read it active");
    if let Some(job) = job {
        crate::computers::jobs::request_cancel(&mut *tx, watcher.bot_id, Some(job)).await?;
    }
    events::bot_updated(&mut tx, watcher.bot_id).await?;
    tx.commit().await
}

async fn admit(
    pool: &SqlitePool,
    computers: Option<&ComputerManager>,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<Option<Check>, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT w.bot_id, w.check_command, w.every_minutes FROM watchers w
         JOIN bots b ON b.id = w.bot_id
         WHERE w.id = ?1 AND w.status = 'waiting' AND w.next_check_at <= ?2
           AND b.status = 'active' AND NOT b.cancel_pending",
    )
    .bind(id)
    .bind(ts(now))
    .fetch_optional(&mut *tx)
    .await?;
    let Some((bot_id, command, every)) = row else {
        return Ok(None);
    };
    let reserved = match computers {
        Some(computers) => computers
            .reserve_job(
                &mut tx,
                bot_id,
                None,
                &command,
                None,
                CHECK_DURATION,
                &[],
                None,
            )
            .await?
            .ok_or(RETRY),
        None => Err(Duration::minutes(every)),
    };
    let job_id = match reserved {
        Ok(job_id) => job_id,
        Err(delay) => {
            sqlx::query(
                "UPDATE watchers SET next_check_at = ?2 WHERE id = ?1 AND status = 'waiting'",
            )
            .bind(id)
            .bind(ts(now + delay))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(None);
        }
    };
    sqlx::query(
        "UPDATE watchers SET status = 'checking', check_job_id = ?2, next_check_at = NULL
         WHERE id = ?1 AND status = 'waiting'",
    )
    .bind(id)
    .bind(job_id)
    .execute(&mut *tx)
    .await?;
    events::bot_updated(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(Some(Check {
        bot_id,
        job_id,
        command,
    }))
}

#[derive(sqlx::FromRow)]
struct EndedCheck {
    bot_id: Uuid,
    check_job_id: Uuid,
    every_minutes: i64,
    failures: i64,
    job_status: JobStatus,
    exit_code: Option<i32>,
    output_tail: Option<String>,
    leased: bool,
}

/// Settle up to `limit` checking watchers whose check job ended, per
/// [`checked`]: wait for the next check, or end the watcher.
pub async fn settle_checks(
    pool: &SqlitePool,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<(), sqlx::Error> {
    let ended: Vec<Uuid> = sqlx::query_scalar(
        "SELECT w.id FROM watchers w JOIN guest_jobs j ON j.id = w.check_job_id
         WHERE w.status = 'checking' AND j.status IN ('completed', 'failed', 'cancelled', 'lost')
         LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    for id in ended {
        settle(pool, id, now).await?;
    }
    Ok(())
}

async fn settle(pool: &SqlitePool, id: Uuid, now: DateTime<Utc>) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<EndedCheck> = sqlx::query_as(
        "SELECT w.bot_id, w.check_job_id, w.every_minutes, w.failures, j.status AS job_status,
                j.exit_code, j.output_tail, j.lease_id IS NOT NULL AS leased
         FROM watchers w JOIN guest_jobs j ON j.id = w.check_job_id
         WHERE w.id = ?1 AND w.status = 'checking'",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(());
    };
    let Some(check) = checked(
        row.job_status,
        row.exit_code,
        row.output_tail,
        row.leased,
        row.failures,
    ) else {
        return Ok(());
    };
    let (status, next) = match check.ends {
        Some(_) => (Status::Waking, None),
        None => (
            Status::Waiting,
            Some(ts(now + Duration::minutes(row.every_minutes))),
        ),
    };
    let settled = sqlx::query(
        "UPDATE watchers SET status = ?3, outcome = ?4, next_check_at = ?5, check_job_id = NULL,
             failures = ?6, checks = checks + ?7, last_status = ?8, last_output = ?9,
             last_checked_at = CASE WHEN ?7 THEN ?10 ELSE last_checked_at END,
             ended_at = CASE WHEN ?4 IS NULL THEN NULL ELSE ?10 END
         WHERE id = ?1 AND status = 'checking' AND check_job_id = ?2",
    )
    .bind(id)
    .bind(row.check_job_id)
    .bind(status)
    .bind(check.ends)
    .bind(next)
    .bind(check.failures)
    .bind(check.ran)
    .bind(&check.status)
    .bind(&check.output)
    .bind(ts(now))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if settled == 1 {
        events::bot_updated(&mut tx, row.bot_id).await?;
    }
    tx.commit().await
}

#[derive(sqlx::FromRow)]
struct Waking {
    bot_id: Uuid,
    conversation_id: Uuid,
    prompt: String,
    last_status: Option<String>,
    last_output: Option<String>,
}

/// Queue the wake turn of up to `limit` ended watchers of active bots.
/// A wake that cannot queue (full queue, no provider) stays `waking` and
/// is retried next scan. Returns how many turns were queued.
pub async fn deliver_wakes(
    pool: &SqlitePool,
    registry: &Registry,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let waking: Vec<Uuid> = sqlx::query_scalar(
        "SELECT w.id FROM watchers w JOIN bots b ON b.id = w.bot_id
         WHERE w.status = 'waking' AND b.status = 'active' AND NOT b.cancel_pending
         ORDER BY w.ended_at, w.id LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut queued = 0;
    for id in waking {
        if wake(pool, registry, id).await? {
            queued += 1;
        }
    }
    Ok(queued)
}

async fn wake(pool: &SqlitePool, registry: &Registry, id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<Waking> = sqlx::query_as(
        "SELECT w.bot_id, w.conversation_id, w.prompt, w.last_status, w.last_output
         FROM watchers w JOIN bots b ON b.id = w.bot_id
         WHERE w.id = ?1 AND w.status = 'waking' AND b.status = 'active' AND NOT b.cancel_pending",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let text = wake_text(
        &row.prompt,
        row.last_status.as_deref(),
        row.last_output.as_deref(),
    );
    let Ok(turn_id) = queue_saved_task(
        &mut tx,
        registry,
        row.bot_id,
        row.conversation_id,
        id,
        b"watcher-wake\0",
        &text,
        RunOn::Chat,
    )
    .await?
    else {
        return Ok(false);
    };
    sqlx::query(
        "UPDATE watchers SET status = 'ended', wake_turn_id = ?2 WHERE id = ?1 AND status = 'waking'",
    )
    .bind(id)
    .bind(turn_id)
    .execute(&mut *tx)
    .await?;
    events::turn_accepted(&mut tx, row.bot_id, turn_id).await?;
    events::bot_updated(&mut tx, row.bot_id).await?;
    tx.commit().await?;
    Ok(true)
}

/// The watcher wake a turn executes, if it is one.
pub async fn run_of(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Option<WatcherRun>, sqlx::Error> {
    let row: Option<(Uuid, String, Outcome)> =
        sqlx::query_as("SELECT id, purpose, outcome FROM watchers WHERE wake_turn_id = ?1")
            .bind(turn_id)
            .fetch_optional(conn)
            .await?;
    Ok(row.map(|(watcher_id, purpose, outcome)| WatcherRun {
        watcher_id,
        purpose,
        outcome,
    }))
}
