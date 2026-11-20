//! A computer's lifecycle as stored in `bot_computers` and
//! `computer_upgrades`, and every write that moves it. Each transition
//! names the states it may start from; the `UPDATE` is guarded by them, so
//! a write that lost a race changes nothing and the caller sees it.

use sqlx::SqliteConnection;
use uuid::Uuid;

use super::{IdleAction, RuntimeState, bounded};
use crate::db::in_ids;

/// What the API last established about a computer (`observed_state`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    sqlx::Type,
    strum::IntoStaticStr,
    strum::Display,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Observed {
    /// Never created.
    Absent,
    Creating,
    Starting,
    Running,
    Pausing,
    Paused,
    Resuming,
    Stopping,
    Stopped,
    /// Recorded, but gone from the runtime; it is not recreated.
    Missing,
    /// A transition failed and the runtime's state is unknown.
    Error,
}

impl Observed {
    /// A runtime call is (or was, before a restart) moving the computer.
    pub fn is_transient(self) -> bool {
        matches!(
            self,
            Self::Creating | Self::Starting | Self::Pausing | Self::Resuming | Self::Stopping
        )
    }

    /// What the runtime reports for a computer recorded as `self`. One that
    /// never finished being created is `absent`, not `missing`.
    pub fn from_runtime(self, actual: Option<RuntimeState>) -> Self {
        match actual {
            Some(RuntimeState::Running) => Self::Running,
            Some(RuntimeState::Paused) => Self::Paused,
            Some(RuntimeState::Stopped) => Self::Stopped,
            Some(RuntimeState::Failed) => Self::Error,
            None if matches!(self, Self::Absent | Self::Creating) => Self::Absent,
            None => Self::Missing,
        }
    }
}

/// The owner's intent for a computer (`desired_state`).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    sqlx::Type,
    strum::Display,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Desired {
    /// Wakes on use and stops or pauses when idle.
    Auto,
    /// The owner stopped it; nothing wakes it until they resume it.
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Wake {
    Create,
    Start,
    Resume,
}

impl Wake {
    fn from(self) -> Observed {
        match self {
            Self::Create => Observed::Absent,
            Self::Start => Observed::Stopped,
            Self::Resume => Observed::Paused,
        }
    }

    pub(super) fn transient(self) -> Observed {
        match self {
            Self::Create => Observed::Creating,
            Self::Start => Observed::Starting,
            Self::Resume => Observed::Resuming,
        }
    }
}

/// A write of `observed_state` for one computer.
#[derive(Debug, Clone, Copy)]
pub(super) enum Transition<'a> {
    /// A wake was decided; a new generation begins.
    Wake(Wake),
    /// The wake's runtime call succeeded.
    Woke(Wake),
    /// A stop or pause was decided; a new generation begins.
    Halt(IdleAction),
    /// The stop or pause's runtime call succeeded.
    Halted(IdleAction),
    /// A paused computer was resumed in place; its leases carry over.
    Resumed,
    /// A runtime call from `from` failed. A failed create stays `creating`,
    /// so inspecting it resolves to `absent` rather than `missing`.
    Failed { from: Observed, error: &'a str },
    /// The runtime reported `now` for a computer recorded as `was`.
    Observed { was: Observed, now: Observed },
    /// Startup recorded what the runtime reports for a computer recorded as
    /// `was` at `generation` before the runtime was listed; a new generation
    /// begins, so no lease from before the restart matches by accident.
    Reconciled {
        was: Observed,
        generation: i64,
        now: Observed,
    },
}

impl Transition<'_> {
    fn sources(self) -> Vec<Observed> {
        use Observed::*;
        match self {
            Self::Wake(wake) => vec![wake.from()],
            Self::Woke(wake) => vec![wake.transient()],
            Self::Halt(IdleAction::Stop) => vec![Running, Paused, Error],
            Self::Halt(IdleAction::Pause) => vec![Running],
            Self::Halted(IdleAction::Stop) => vec![Stopping],
            Self::Halted(IdleAction::Pause) => vec![Pausing],
            Self::Resumed => vec![Paused],
            Self::Failed { from, .. } => vec![from],
            Self::Observed { was, .. } | Self::Reconciled { was, .. } => vec![was],
        }
    }

    fn target(self) -> Observed {
        match self {
            Self::Wake(wake) => wake.transient(),
            Self::Woke(_) | Self::Resumed => Observed::Running,
            Self::Halt(IdleAction::Stop) => Observed::Stopping,
            Self::Halt(IdleAction::Pause) => Observed::Pausing,
            Self::Halted(IdleAction::Stop) => Observed::Stopped,
            Self::Halted(IdleAction::Pause) => Observed::Paused,
            Self::Failed {
                from: Observed::Creating,
                ..
            } => Observed::Creating,
            Self::Failed { .. } => Observed::Error,
            Self::Observed { now, .. } | Self::Reconciled { now, .. } => now,
        }
    }

    /// The generation the computer must still be at: a reconciliation acts
    /// on a listing taken after it read the row, so any transition since
    /// (which may have finished in the same state) makes the listing stale.
    fn expected_generation(self) -> Option<i64> {
        match self {
            Self::Reconciled { generation, .. } => Some(generation),
            _ => None,
        }
    }

    fn new_generation(self) -> bool {
        matches!(
            self,
            Self::Wake(_) | Self::Halt(_) | Self::Reconciled { .. }
        )
    }

    /// Apply it to `bot_id`'s computer. Returns the computer's generation
    /// afterwards, or `None` when it was not in a source state and nothing
    /// changed.
    pub(super) async fn apply(
        self,
        conn: &mut SqliteConnection,
        bot_id: Uuid,
    ) -> Result<Option<i64>, sqlx::Error> {
        let error = match self {
            Self::Failed { error, .. } => Some(bounded(error, 2000)),
            _ => None,
        };
        sqlx::query_scalar(
            "UPDATE bot_computers SET observed_state = ?2, generation = generation + ?3,
                 last_error = coalesce(?4, last_error),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE bot_id = ?1 AND observed_state IN (SELECT value FROM json_each(?5))
               AND (?6 IS NULL OR generation = ?6)
             RETURNING generation",
        )
        .bind(bot_id)
        .bind(self.target())
        .bind(i64::from(self.new_generation()))
        .bind(error)
        .bind(sqlx::types::Json(self.sources()))
        .bind(self.expected_generation())
        .fetch_optional(&mut *conn)
        .await
    }
}

/// Switch a stopped computer to another runtime computer (an upgrade's
/// replacement, or the one a rollback returns to). Returns whether it was
/// still stopped on `from_runtime_name`; otherwise nothing changed.
pub(super) async fn switch(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    from_runtime_name: &str,
    runtime_name: &str,
    image: &str,
    network: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let switched = sqlx::query(
        "UPDATE bot_computers SET runtime_name = ?3, image = ?4, network = ?5,
             generation = generation + 1, services_generation = NULL,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND runtime_name = ?2 AND observed_state = ?6",
    )
    .bind(bot_id)
    .bind(from_runtime_name)
    .bind(runtime_name)
    .bind(image)
    .bind(network)
    .bind(Observed::Stopped)
    .execute(&mut *conn)
    .await?;
    Ok(switched.rows_affected() == 1)
}

/// After a restore: the computers whose disks it brought back (`disks`) are
/// stopped, whatever the restored database recorded, and every other
/// created computer is missing. Restore runs offline, before any manager
/// uses the database. Returns how many of `disks` had a computer row; the
/// caller refuses a restore where one did not.
pub async fn restored(conn: &mut SqliteConnection, disks: &[Uuid]) -> Result<u64, sqlx::Error> {
    let restored = in_ids("bot_id", "?1");
    let stopped = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE bot_computers SET observed_state = ?2,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE {restored}"
    )))
    .bind(sqlx::types::Json(disks))
    .bind(Observed::Stopped)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE bot_computers SET observed_state = ?2,
             last_error = 'not included in the restored backup',
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE observed_state <> ?3 AND NOT ({restored})"
    )))
    .bind(sqlx::types::Json(disks))
    .bind(Observed::Missing)
    .bind(Observed::Absent)
    .execute(&mut *conn)
    .await?;
    Ok(stopped)
}

/// Where an image upgrade is (`computer_upgrades.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, strum::Display)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum UpgradeState {
    /// Copying, or failed with its replacement not yet removed; either way
    /// its computers still reserve budget.
    Started,
    Completed,
    Failed,
    RolledBack,
}

/// A write that ends or records an upgrade.
#[derive(Debug, Clone, Copy)]
pub(super) enum UpgradeEnd<'a> {
    /// It failed and its replacement is gone.
    Fail(&'a str),
    /// It failed, but removing its replacement failed too: it stays
    /// `started` until maintenance removes it.
    Strand(&'a str),
    /// The bot switched to the replacement.
    Complete {
        bytes: u64,
        sha256: &'a str,
        unrestored: &'a [String],
    },
    /// The bot switched back to the computer it replaced.
    RollBack,
}

impl UpgradeEnd<'_> {
    fn source(self) -> UpgradeState {
        match self {
            Self::Fail(_) | Self::Strand(_) | Self::Complete { .. } => UpgradeState::Started,
            Self::RollBack => UpgradeState::Completed,
        }
    }

    fn target(self) -> UpgradeState {
        match self {
            Self::Fail(_) => UpgradeState::Failed,
            Self::Strand(_) => UpgradeState::Started,
            Self::Complete { .. } => UpgradeState::Completed,
            Self::RollBack => UpgradeState::RolledBack,
        }
    }

    /// Apply it to upgrade `id`. Returns whether it was in the source state;
    /// otherwise nothing changed.
    pub(super) async fn apply(
        self,
        conn: &mut SqliteConnection,
        id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let (error, bytes, sha256, unrestored) = match self {
            Self::Fail(error) | Self::Strand(error) => {
                (Some(bounded(error, 2000)), None, None, None)
            }
            Self::Complete {
                bytes,
                sha256,
                unrestored,
            } => (
                None,
                Some(bytes as i64),
                Some(sha256),
                (!unrestored.is_empty())
                    .then(|| serde_json::to_string(unrestored).expect("strings serialize")),
            ),
            Self::RollBack => (None, None, None, None),
        };
        let applied = sqlx::query(
            "UPDATE computer_upgrades SET state = ?3,
                 finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 error = coalesce(error, ?4), data_bytes = coalesce(?5, data_bytes),
                 data_sha256 = coalesce(?6, data_sha256),
                 unrestored_packages = coalesce(?7, unrestored_packages)
             WHERE id = ?1 AND state = ?2",
        )
        .bind(id)
        .bind(self.source())
        .bind(self.target())
        .bind(error)
        .bind(bytes)
        .bind(sha256)
        .bind(unrestored)
        .execute(&mut *conn)
        .await?;
        Ok(applied.rows_affected() == 1)
    }
}
