//! Routines and their runs.
//!
//! A routine belongs to its bot's owner conversation. Only that bot, from a
//! running turn whose work is rooted in an owner message (in its owner
//! conversation, or a task another bot delegated for one), may manage it:
//! every mutation rechecks the turn inside its own immediate transaction,
//! so a routine run, a task delegated by one, or a stopped turn is refused
//! even with a grant. The owner may switch a routine on or off, or delete it.
//!
//! The scheduler admits due routines (see [`admit_due`]). Each admission is
//! one transaction that records the occurrence in `routine_runs`, advances
//! `next_due_at` past now (missed occurrences coalesce into this one run),
//! and either queues a turn whose source is a hidden `routine` item or
//! records why it was skipped. Run status is the turn's own status.

use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{bot_profiles, events};
use crate::app::messages::MAX_QUEUED_TURNS;
use crate::app::providers::{Origin, RoutineRun};
use crate::app::routines::{InvalidRoutine, MAX_PER_BOT, RoutineName, RoutinePrompt, Schedule};
use crate::providers::Registry;

/// Due routines admitted per scheduler scan.
pub const ADMIT_BATCH: i64 = 8;

#[derive(Debug, thiserror::Error)]
pub enum RoutineError {
    #[error(
        "a bot manages its own routines only for an owner request; routine runs and tasks they \
         delegated cannot"
    )]
    NotAllowed,
    #[error("the request is no longer running")]
    NotRunning,
    #[error("no such routine")]
    NotFound,
    #[error("the bot was deleted")]
    BotDeleted,
    #[error("another routine already has that name; update it or choose another name")]
    NameTaken,
    #[error(
        "this request already created a routine with that name and different settings; retry \
         with the original settings or use routine_update"
    )]
    Conflict,
    #[error("a bot can have at most {MAX_PER_BOT} routines; delete one first")]
    Limit,
    #[error(transparent)]
    Invalid(#[from] InvalidRoutine),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// The turn a routine tool call comes from, as its grant records it.
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub turn_id: Uuid,
}

#[derive(Debug, Clone, Copy)]
pub enum By {
    Turn(Caller),
    Owner { bot_id: Uuid },
}

impl By {
    fn bot_id(&self) -> Uuid {
        match self {
            By::Turn(caller) => caller.bot_id,
            By::Owner { bot_id } => *bot_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastRun {
    pub due_at: DateTime<Utc>,
    /// `queued`, `running`, `skipped`, or the turn's terminal status.
    pub status: String,
    pub skip_reason: Option<String>,
    pub turn_id: Option<Uuid>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routine {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub name: String,
    pub prompt: String,
    pub prompt_revision: i64,
    pub cron: String,
    pub timezone: String,
    pub enabled: bool,
    pub next_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted: bool,
    pub last_run: Option<LastRun>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: Uuid,
    bot_id: Uuid,
    name: String,
    prompt: String,
    prompt_revision: i64,
    cron: String,
    timezone: String,
    enabled: bool,
    next_due_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    deleted_at: Option<DateTime<Utc>>,
    run_due_at: Option<DateTime<Utc>>,
    skip_reason: Option<String>,
    turn_id: Option<Uuid>,
    turn_status: Option<String>,
    error_code: Option<String>,
    error_message: Option<String>,
    finished_at: Option<DateTime<Utc>>,
}

impl From<Row> for Routine {
    fn from(row: Row) -> Self {
        let last_run = row.run_due_at.map(|due_at| LastRun {
            due_at,
            status: match row.turn_status.as_deref() {
                None => "skipped".to_owned(),
                Some("queued") => "queued".to_owned(),
                Some("preparing" | "sending" | "running" | "cancelling" | "waiting") => {
                    "running".to_owned()
                }
                Some(other) => other.to_owned(),
            },
            skip_reason: row.skip_reason,
            turn_id: row.turn_id,
            error_code: row.error_code,
            error_message: row.error_message,
            finished_at: row.finished_at,
        });
        Self {
            id: row.id,
            bot_id: row.bot_id,
            name: row.name,
            prompt: row.prompt,
            prompt_revision: row.prompt_revision,
            cron: row.cron,
            timezone: row.timezone,
            enabled: row.enabled,
            next_run_at: row.next_due_at,
            created_at: row.created_at,
            updated_at: row.updated_at,
            deleted: row.deleted_at.is_some(),
            last_run,
        }
    }
}

const SELECT: &str =
    "SELECT r.id, r.bot_id, r.name, r.prompt, r.prompt_revision, r.cron, r.timezone, r.enabled,
        r.next_due_at, r.created_at, r.updated_at, r.deleted_at,
        lr.due_at AS run_due_at, lr.skip_reason, lr.turn_id, t.status AS turn_status,
        t.error_code, t.error_message, t.finished_at
    FROM routines r
    LEFT JOIN routine_runs lr ON lr.id = (SELECT x.id FROM routine_runs x
                                          WHERE x.routine_id = r.id ORDER BY x.due_at DESC LIMIT 1)
    LEFT JOIN turns t ON t.id = lr.turn_id";

/// The same text form SQLite's own timestamps use, so stored instants
/// compare correctly as text.
fn ts(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, false)
}

/// A bot's current routines, oldest first.
pub async fn list(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<Vec<Routine>, sqlx::Error> {
    let rows: Vec<Row> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SELECT} WHERE r.bot_id = ?1 AND r.deleted_at IS NULL ORDER BY r.created_at, r.id"
    )))
    .bind(bot_id)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(Routine::from).collect())
}

async fn load(conn: &mut SqliteConnection, id: Uuid) -> Result<Routine, sqlx::Error> {
    let row: Row = sqlx::query_as(sqlx::AssertSqlSafe(format!("{SELECT} WHERE r.id = ?1")))
        .bind(id)
        .fetch_one(conn)
        .await?;
    Ok(row.into())
}

async fn authorized_owner_conversation(
    conn: &mut SqliteConnection,
    caller: Caller,
) -> Result<Uuid, RoutineError> {
    let row: Option<(Uuid, Uuid, String, String, Uuid)> = sqlx::query_as(
        "SELECT t.bot_id, t.conversation_id, t.status, b.status, o.id
         FROM turns t JOIN bots b ON b.id = t.bot_id
         JOIN conversations o ON o.bot_id = t.bot_id AND o.kind = 'owner'
         WHERE t.id = ?1",
    )
    .bind(caller.turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((bot, conversation, status, bot_status, owner_conversation)) = row else {
        return Err(RoutineError::NotRunning);
    };
    if bot != caller.bot_id || conversation != caller.conversation_id {
        return Err(RoutineError::NotAllowed);
    }
    if super::delegations::origin(conn, caller.turn_id).await? != Origin::OwnerRequest {
        return Err(RoutineError::NotAllowed);
    }
    if !matches!(status.as_str(), "sending" | "running") || bot_status != "active" {
        return Err(RoutineError::NotRunning);
    }
    Ok(owner_conversation)
}

async fn authorize_by(conn: &mut SqliteConnection, by: By) -> Result<(), RoutineError> {
    match by {
        By::Turn(caller) => authorized_owner_conversation(conn, caller).await.map(drop),
        By::Owner { bot_id } => {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
                    .bind(bot_id)
                    .fetch_optional(&mut *conn)
                    .await?;
            match status.as_deref() {
                None => Err(RoutineError::NotFound),
                Some("active") => Ok(()),
                Some(_) => Err(RoutineError::BotDeleted),
            }
        }
    }
}

pub struct NewRoutine<'a> {
    pub name: &'a RoutineName,
    pub prompt: &'a RoutinePrompt,
    pub schedule: &'a Schedule,
}

fn create_hash(routine: &NewRoutine<'_>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for part in [
        "routine",
        routine.prompt.as_str(),
        routine.schedule.cron(),
        routine.schedule.timezone(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hasher.finalize().to_vec()
}

/// Create a routine, enabled, first due at its next occurrence after `now`.
/// A retry with the same name from the same turn returns the routine it
/// created (as it is now); changed settings conflict. Returns whether it
/// was created by this call.
pub async fn create(
    pool: &SqlitePool,
    caller: Caller,
    routine: NewRoutine<'_>,
    now: DateTime<Utc>,
) -> Result<(Routine, bool), RoutineError> {
    let hash = create_hash(&routine);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let conversation_id = authorized_owner_conversation(&mut tx, caller).await?;
    let existing: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT id, request_hash FROM routines WHERE created_turn_id = ?1 AND created_name = ?2",
    )
    .bind(caller.turn_id)
    .bind(routine.name.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((id, saved)) = existing {
        if saved != hash {
            return Err(RoutineError::Conflict);
        }
        let routine = load(&mut tx, id).await?;
        tx.commit().await?;
        return Ok((routine, false));
    }
    let (count, taken): (i64, bool) = sqlx::query_as(
        "SELECT count(*), coalesce(max(name = ?2), false) FROM routines
         WHERE bot_id = ?1 AND deleted_at IS NULL",
    )
    .bind(caller.bot_id)
    .bind(routine.name.as_str())
    .fetch_one(&mut *tx)
    .await?;
    if taken {
        return Err(RoutineError::NameTaken);
    }
    if count >= MAX_PER_BOT {
        return Err(RoutineError::Limit);
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
             next_due_at, created_turn_id, created_name, request_hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?4, ?10)",
    )
    .bind(id)
    .bind(caller.bot_id)
    .bind(conversation_id)
    .bind(routine.name.as_str())
    .bind(routine.prompt.as_str())
    .bind(routine.schedule.cron())
    .bind(routine.schedule.timezone())
    .bind(routine.schedule.next_after(now).map(ts))
    .bind(caller.turn_id)
    .bind(&hash)
    .execute(&mut *tx)
    .await?;
    events::bot_updated(&mut tx, caller.bot_id).await?;
    let created = load(&mut tx, id).await?;
    tx.commit().await?;
    Ok((created, true))
}

/// Absolute values to set; `None` keeps the current one.
#[derive(Debug, Default)]
pub struct Patch {
    pub name: Option<RoutineName>,
    pub prompt: Option<RoutinePrompt>,
    pub cron: Option<String>,
    pub timezone: Option<String>,
    pub enabled: Option<bool>,
}

/// Apply `patch`. Setting values a routine already has changes nothing, so
/// a retry never postpones its schedule. A new prompt is a new prompt
/// revision, which ends the routine's destructive grant. Disabling stops
/// future runs (one already admitted finishes); enabling or a new schedule
/// counts from `now`.
pub async fn update(
    pool: &SqlitePool,
    by: By,
    id: Uuid,
    patch: Patch,
    now: DateTime<Utc>,
) -> Result<Routine, RoutineError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_by(&mut tx, by).await?;
    let current: Option<(String, String, String, String, bool, Option<String>)> = sqlx::query_as(
        "SELECT name, prompt, cron, timezone, enabled, next_due_at FROM routines
         WHERE id = ?1 AND bot_id = ?2 AND deleted_at IS NULL",
    )
    .bind(id)
    .bind(by.bot_id())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((name, prompt, cron, timezone, enabled, next_due)) = current else {
        return Err(RoutineError::NotFound);
    };
    let schedule = Schedule::parse_due(
        patch.cron.as_deref().unwrap_or(&cron),
        Some(patch.timezone.as_deref().unwrap_or(&timezone)),
        now,
    )?;
    let new_name = patch.name.map_or(name.clone(), |n| n.as_str().to_owned());
    let new_prompt = patch
        .prompt
        .map_or(prompt.clone(), |p| p.as_str().to_owned());
    let new_enabled = patch.enabled.unwrap_or(enabled);
    let rescheduled = schedule.cron() != cron || schedule.timezone() != timezone;
    let next_due = if !new_enabled {
        None
    } else if rescheduled || !enabled {
        schedule.next_after(now).map(ts)
    } else {
        next_due
    };
    if new_name != name {
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM routines
                            WHERE bot_id = ?1 AND name = ?2 AND deleted_at IS NULL AND id <> ?3)",
        )
        .bind(by.bot_id())
        .bind(&new_name)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Err(RoutineError::NameTaken);
        }
    }
    let changed = sqlx::query(
        "UPDATE routines SET name = ?2, prompt = ?3, cron = ?4, timezone = ?5, enabled = ?6,
             next_due_at = ?7, prompt_revision = prompt_revision + (prompt IS NOT ?3),
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND (name IS NOT ?2 OR prompt IS NOT ?3 OR cron IS NOT ?4
             OR timezone IS NOT ?5 OR enabled IS NOT ?6 OR next_due_at IS NOT ?7)",
    )
    .bind(id)
    .bind(&new_name)
    .bind(&new_prompt)
    .bind(schedule.cron())
    .bind(schedule.timezone())
    .bind(new_enabled)
    .bind(&next_due)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed > 0 {
        if let By::Turn(caller) = by {
            let (conversation, seq): (Uuid, i64) = sqlx::query_as(
                "UPDATE conversations SET last_item_seq = last_item_seq + 1
                 WHERE bot_id = ?1 AND kind = 'owner' RETURNING id, last_item_seq",
            )
            .bind(caller.bot_id)
            .fetch_one(&mut *tx)
            .await?;
            let item_id = Uuid::new_v4();
            let update = super::turns::RoutineUpdate {
                routine_id: id,
                name: new_name.clone(),
            };
            sqlx::query(
                "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text, routine_update)
                 VALUES (?1, ?2, ?3, ?4, 'status', ?5, ?6)",
            )
            .bind(item_id)
            .bind(conversation)
            .bind(seq)
            .bind(caller.turn_id)
            .bind(format!("Updated routine: {new_name}"))
            .bind(sqlx::types::Json(update))
            .execute(&mut *tx)
            .await?;
            events::item_upsert(&mut tx, caller.bot_id, item_id).await?;
        }
        events::bot_updated(&mut tx, by.bot_id()).await?;
    }
    let routine = load(&mut tx, id).await?;
    tx.commit().await?;
    Ok(routine)
}

/// Delete a routine: it never runs again and leaves the list; a run already
/// admitted finishes. Deleting it again returns `false`.
pub async fn delete(pool: &SqlitePool, by: By, id: Uuid) -> Result<bool, RoutineError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_by(&mut tx, by).await?;
    let deleted: Option<bool> = sqlx::query_scalar(
        "SELECT deleted_at IS NOT NULL FROM routines WHERE id = ?1 AND bot_id = ?2",
    )
    .bind(id)
    .bind(by.bot_id())
    .fetch_optional(&mut *tx)
    .await?;
    match deleted {
        None => Err(RoutineError::NotFound),
        Some(true) => Ok(false),
        Some(false) => {
            sqlx::query(
                "UPDATE routines SET enabled = false, next_due_at = NULL,
                     deleted_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?;
            events::bot_updated(&mut tx, by.bot_id()).await?;
            tx.commit().await?;
            Ok(true)
        }
    }
}

/// The routine run a turn executes, if it is one.
pub async fn run_of(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Option<RoutineRun>, sqlx::Error> {
    let row: Option<(Uuid, String, DateTime<Utc>)> =
        sqlx::query_as("SELECT routine_id, name, due_at FROM routine_runs WHERE turn_id = ?1")
            .bind(turn_id)
            .fetch_optional(conn)
            .await?;
    Ok(row.map(|(routine_id, name, due_at)| RoutineRun {
        routine_id,
        name,
        due_at,
    }))
}

/// Admit up to `limit` routines due at `now`. Returns how many turns were
/// queued. The caller holds a maintenance admission.
pub async fn admit_due(
    pool: &SqlitePool,
    registry: &Registry,
    now: DateTime<Utc>,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    let due: Vec<Uuid> = sqlx::query_scalar(
        "SELECT r.id FROM routines r JOIN bots b ON b.id = r.bot_id
         WHERE r.next_due_at IS NOT NULL AND r.next_due_at <= ?1
           AND b.status = 'active' AND NOT b.cancel_pending
         ORDER BY r.next_due_at, r.id LIMIT ?2",
    )
    .bind(ts(now))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    let mut queued = 0;
    for id in due {
        if admit(pool, registry, id, now).await? {
            queued += 1;
        }
    }
    Ok(queued)
}

#[derive(Debug, Clone, Copy)]
enum Skip {
    Overlap,
    QueueFull,
    ProviderUnavailable,
}

impl Skip {
    fn as_str(self) -> &'static str {
        match self {
            Skip::Overlap => "overlap",
            Skip::QueueFull => "queue_full",
            Skip::ProviderUnavailable => "provider_unavailable",
        }
    }
}

#[derive(sqlx::FromRow)]
struct Due {
    bot_id: Uuid,
    conversation_id: Uuid,
    name: String,
    prompt: String,
    prompt_revision: i64,
    cron: String,
    timezone: String,
    due_at: String,
    last_turn_no: i64,
    last_item_seq: i64,
}

async fn admit(
    pool: &SqlitePool,
    registry: &Registry,
    id: Uuid,
    now: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let row: Option<Due> = sqlx::query_as(
        "SELECT r.bot_id, r.conversation_id, r.name, r.prompt, r.prompt_revision, r.cron, r.timezone,
                r.next_due_at AS due_at, c.last_turn_no, c.last_item_seq
         FROM routines r JOIN bots b ON b.id = r.bot_id
         JOIN conversations c ON c.id = r.conversation_id
         WHERE r.id = ?1 AND r.next_due_at IS NOT NULL AND r.next_due_at <= ?2
           AND b.status = 'active' AND NOT b.cancel_pending",
    )
    .bind(id)
    .bind(ts(now))
    .fetch_optional(&mut *tx)
    .await?;
    let Some(Due {
        bot_id,
        conversation_id,
        name,
        prompt,
        prompt_revision,
        cron,
        timezone,
        due_at,
        last_turn_no,
        last_item_seq,
    }) = row
    else {
        return Ok(false);
    };
    let next = Schedule::parse(&cron, Some(&timezone))
        .ok()
        .and_then(|schedule| schedule.next_after(now));
    sqlx::query("UPDATE routines SET next_due_at = ?2 WHERE id = ?1")
        .bind(id)
        .bind(next.map(ts))
        .execute(&mut *tx)
        .await?;
    let recorded: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM routine_runs WHERE routine_id = ?1 AND due_at = ?2)",
    )
    .bind(id)
    .bind(&due_at)
    .fetch_one(&mut *tx)
    .await?;
    if recorded {
        tx.commit().await?;
        return Ok(false);
    }
    let unfinished: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM routine_runs rr JOIN turns t ON t.id = rr.turn_id
                        WHERE rr.routine_id = ?1
                          AND t.status NOT IN ('completed', 'interrupted', 'failed', 'uncertain'))",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turns WHERE bot_id = ?1 AND status = 'queued'")
            .bind(bot_id)
            .fetch_one(&mut *tx)
            .await?;
    let run_id = Uuid::new_v4();
    let chosen = if unfinished {
        Err(Skip::Overlap)
    } else if queued >= MAX_QUEUED_TURNS {
        Err(Skip::QueueFull)
    } else {
        bot_profiles::for_admission(&mut tx, registry, bot_id)
            .await?
            .ok_or(Skip::ProviderUnavailable)
    };
    let bot_profiles::TurnProvider {
        provider_id,
        profile,
    } = match chosen {
        Ok(chosen) => chosen,
        Err(skip) => {
            sqlx::query(
                "INSERT INTO routine_runs (id, routine_id, due_at, name, skip_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .bind(run_id)
            .bind(id)
            .bind(&due_at)
            .bind(&name)
            .bind(skip.as_str())
            .execute(&mut *tx)
            .await?;
            events::bot_updated(&mut tx, bot_id).await?;
            tx.commit().await?;
            return Ok(false);
        }
    };
    let turn_id = Uuid::new_v4();
    let item_id = Uuid::new_v4();
    let turn_no = last_turn_no + 1;
    let seq = last_item_seq + 1;
    let mut hasher = Sha256::new();
    hasher.update(b"routine-run\0");
    hasher.update(run_id.as_bytes());
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, profile_revision, profile)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(turn_id)
    .bind(bot_id)
    .bind(conversation_id)
    .bind(run_id)
    .bind(hasher.finalize().to_vec())
    .bind(turn_no)
    .bind(item_id)
    .bind(provider_id)
    .bind(profile.as_ref().map(|(revision, _)| *revision))
    .bind(profile.as_ref().map(|(_, profile)| profile))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, ?3, ?4, 'routine', ?5)",
    )
    .bind(item_id)
    .bind(conversation_id)
    .bind(seq)
    .bind(turn_id)
    .bind(&prompt)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET last_turn_no = ?2, last_item_seq = ?3 WHERE id = ?1")
        .bind(conversation_id)
        .bind(turn_no)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id, prompt_revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(run_id)
    .bind(id)
    .bind(&due_at)
    .bind(&name)
    .bind(turn_id)
    .bind(prompt_revision)
    .execute(&mut *tx)
    .await?;
    events::turn_accepted(&mut tx, bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(true)
}
