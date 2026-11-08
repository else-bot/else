//! Bot computers: one persistent micro-VM per bot, started on demand.
//!
//! SQLite holds each computer's identity, configuration, owner intent
//! (`desired_state`), what the API last established (`observed_state`), a
//! transition generation, and renewable leases. Runtime operations for one
//! bot (create, start, resume, stop, pause, exec) are serialized by an
//! in-process lock (the API is a singleton), so a slow or failed operation
//! can never act on a newer one; each decision is also taken under the
//! immediate write transaction and re-checked when the runtime call returns. A
//! failed transition is followed by inspecting the runtime and recording
//! what it actually reports; when that is unknown the computer is `error`,
//! keeps its reservation, and maintenance inspects it again. Admission
//! counts running, paused, transitioning, and unknown computers against a
//! global CPU and memory budget. Startup only inspects: it never wakes,
//! recreates, replaces, or removes a computer, and no computer is used until
//! that inspection has succeeded. Provider CLIs and credentials stay on the
//! host.

mod desktop;
pub mod jobs;
mod leases;
mod maintenance;
pub mod runtime;
pub mod screens;
mod snapshot;
pub mod upgrade;
pub mod viewer;

pub use desktop::{ViewerLease, ViewerRefusal, ViewerTarget};

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::actor::Actor;

use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

pub use runtime::{ComputerRuntime, ExecOutput, Mount, RuntimeError, RuntimeState, Spec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAction {
    Stop,
    Pause,
}

#[derive(Debug, Clone)]
pub struct ComputerConfig {
    pub image: String,
    /// The size a computer boots with: enough for commands and scripts.
    pub cpus: u32,
    pub memory_mib: u32,
    /// The size a desktop computer grows to, while it runs, once its screen
    /// is first used.
    pub desktop_cpus: u32,
    pub desktop_memory_mib: u32,
    /// Host memory counted per active computer beyond its guest memory
    /// (the measured VM worker overhead, rounded up).
    pub overhead_mib: u32,
    pub max_cpus: u32,
    pub max_memory_mib: u64,
    pub idle_after: Duration,
    pub idle_action: IdleAction,
    pub lease_ttl: Duration,
    /// Microsandbox network profile for new computers.
    pub network: String,
    /// The image runs the else.bot desktop (noVNC published to host
    /// loopback, VNC password, services started after each boot).
    pub desktop: bool,
    /// Host directory for per-computer secrets (VNC passwords) and the
    /// artifact store, private to the API.
    pub state_dir: std::path::PathBuf,
    /// Root of the bots' host project directories; each computer created
    /// here mounts `<projects_dir>/<bot id>` at the guest user's `~/work`.
    pub projects_dir: Option<std::path::PathBuf>,
}

/// Where a bot's host project directory appears in a desktop computer: the
/// `elsebot` user's working directory (see infra/guest). Commands there run
/// as `elsebot` (the image's first user, ID 1000).
pub const GUEST_PROJECT: &str = "/home/elsebot/work";
const GUEST_UID: u32 = 1000;
/// Without the desktop image commands run as root, so the project is root's
/// `~/work`.
pub const ROOT_PROJECT: &str = "/root/work";

/// Guest port of the desktop's noVNC (see infra/guest).
pub const DESKTOP_GUEST_PORT: u16 = 6080;
/// Starts the guest's supervisor after each boot; idempotent. Images before
/// else-desktop also start the whole desktop here.
pub const GUEST_INIT: &str = "/usr/local/bin/else-init";
/// Starts the desktop (screen, VNC, and Chromium) and waits for the browser;
/// idempotent, and a no-op on images whose `else-init` started it.
pub const GUEST_DESKTOP: &str =
    "if [ -x /usr/local/bin/else-desktop ]; then exec /usr/local/bin/else-desktop; fi";
/// Closes the browser cleanly before a stop.
pub const GUEST_CHECKPOINT: &str = "/usr/local/bin/else-checkpoint";

impl Default for ComputerConfig {
    fn default() -> Self {
        Self {
            image: "python:3.12-slim".into(),
            cpus: 1,
            memory_mib: 1024,
            desktop_cpus: 2,
            desktop_memory_mib: 2048,
            overhead_mib: 256,
            max_cpus: 2,
            max_memory_mib: 2 * (2048 + 256),
            idle_after: Duration::from_secs(600),
            idle_action: IdleAction::Stop,
            lease_ttl: Duration::from_secs(300),
            network: "public".into(),
            desktop: false,
            state_dir: std::env::temp_dir().join("else-computers"),
            projects_dir: None,
        }
    }
}

impl ComputerConfig {
    /// The maximum vCPUs and memory a new computer may grow to, when its
    /// desktop is larger than its boot size.
    fn growth(&self) -> Option<(u32, u32)> {
        let cpus = self.desktop_cpus.max(self.cpus);
        let memory = self.desktop_memory_mib.max(self.memory_mib);
        (self.desktop && (cpus, memory) != (self.cpus, self.memory_mib)).then_some((cpus, memory))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquire {
    /// The computer runs and this lease holds it.
    Ready {
        lease_id: Uuid,
        runtime_name: String,
        generation: i64,
    },
    /// Try again later: the budget is full or the computer is being checked.
    Queued { reason: String },
    /// The owner stopped this computer; it does not wake until resumed.
    OwnerStopped,
    /// A recorded computer is missing or broken; nothing was recreated.
    Unavailable { reason: String },
    /// The bot is deleted or being deleted.
    BotInactive,
}

#[derive(Debug, thiserror::Error)]
pub enum ComputerError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
    #[error("the lease is not active")]
    LeaseEnded,
    #[error("the computer is not running")]
    NotRunning,
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    /// A test fault point stopped the operation (a simulated crash).
    #[error("interrupted at a fault point")]
    Interrupted,
    /// The computer holds live work or is mid-transition (a backup defers).
    #[error("the computer is busy: {0}")]
    Busy(String),
    /// An explicit owner operation that does not apply (image upgrades).
    #[error("{0}")]
    Refused(String),
}

/// What a backup found for one bot's computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiskCapture {
    /// Exported, from a stopped computer (stopped first if it was idle).
    Exported {
        runtime_name: String,
        image: String,
        cpus: u32,
        memory_mib: u32,
        desired_state: String,
        desktop_port: Option<u16>,
        /// Whether the computer was running before the backup stopped it.
        was_running: bool,
        /// For a desktop: whether its browser was checkpointed (`ok`) when
        /// it stopped; `None` when unknown.
        browser_checkpoint: Option<String>,
        /// Its mount of the bot's project directory, if it was created with
        /// one: a restore must bind it again.
        project_mount: Option<Mount>,
        /// The archive includes the image (it was not in `carried`).
        carries_image: bool,
    },
    /// The bot never used a computer.
    NoComputer,
    /// Recorded, but gone from the runtime (it is not recreated).
    Missing,
}

const RESERVING: &str = "('creating', 'starting', 'running', 'pausing', 'paused', 'resuming', \
                         'stopping', 'error')";

#[derive(Debug, Clone, sqlx::FromRow)]
struct Row {
    runtime_name: String,
    image: String,
    cpus: i32,
    memory_mib: i32,
    max_cpus: Option<i32>,
    max_memory_mib: Option<i32>,
    /// The runtime holds the computer at its maximum: it grew, and no reset
    /// of its boot size has been confirmed since.
    grown: bool,
    desired_state: String,
    observed_state: String,
    generation: i64,
    network: Option<String>,
}

impl Row {
    /// What the computer may grow to: its boot size when it cannot grow.
    fn max(&self) -> (u32, u32) {
        (
            self.max_cpus.unwrap_or(self.cpus).max(self.cpus) as u32,
            self.max_memory_mib
                .unwrap_or(self.memory_mib)
                .max(self.memory_mib) as u32,
        )
    }

    /// What it needs of the budget to run (see `RESERVED_CPUS`).
    fn reserved(&self) -> (u32, u32) {
        if self.grown {
            self.max()
        } else {
            (self.cpus as u32, self.memory_mib as u32)
        }
    }
}

const ROW: &str = "runtime_name, image, cpus, memory_mib, max_cpus, max_memory_mib, grown, \
                   desired_state, observed_state, generation, network";

/// What an active computer holds of the budget: its maximum once it grew,
/// until a reset of its boot size is confirmed; its boot size otherwise.
const RESERVED_CPUS: &str =
    "CASE WHEN grown THEN max(cpus, coalesce(max_cpus, cpus)) ELSE cpus END";
const RESERVED_MEMORY: &str = "CASE WHEN grown THEN max(memory_mib, coalesce(max_memory_mib, memory_mib)) ELSE memory_mib END";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    Create,
    Start,
    Resume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    MustStop,
    Idle(IdleAction),
    Backup,
    ForJobs,
}

fn bounded(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// How the owner reaches a bot's desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desktop {
    pub lease_id: Uuid,
    pub url: String,
    pub password: String,
    pub lease_seconds: u64,
}

pub struct ComputerManager {
    pool: SqlitePool,
    runtime: Arc<dyn ComputerRuntime>,
    config: ComputerConfig,
    ops: Actor<HashMap<Uuid, std::sync::Weak<Semaphore>>>,
    inspected: AtomicBool,
    foreground: Actor<HashMap<Uuid, jobs::Foreground>>,
    killing: Actor<std::collections::HashSet<Uuid>>,
    secret_key: Option<Arc<crate::credentials::SecretKey>>,
    input_batches: Actor<HashMap<Uuid, u32>>,
    controllers: Actor<HashMap<Uuid, Uuid>>,
}

impl ComputerManager {
    pub fn new(
        pool: SqlitePool,
        runtime: Arc<dyn ComputerRuntime>,
        config: ComputerConfig,
    ) -> Self {
        Self {
            pool,
            runtime,
            config,
            ops: Actor::default(),
            inspected: AtomicBool::new(false),
            foreground: Actor::default(),
            killing: Actor::default(),
            secret_key: None,
            input_batches: Actor::default(),
            controllers: Actor::default(),
        }
    }

    /// Count one input batch for a turn; returns the count including it.
    pub async fn count_input(&self, turn_id: Uuid) -> Result<u32, ComputerError> {
        let pool = self.pool.clone();
        self.input_batches
            .call_async(move |batches| {
                Box::pin(async move {
                    if batches.len() >= 10_000 {
                        let live: Vec<Uuid> = sqlx::query_scalar(
                            "SELECT id FROM turns WHERE status IN
                         ('queued', 'preparing', 'sending', 'running', 'cancelling', 'waiting')",
                        )
                        .fetch_all(&pool)
                        .await?;
                        let live: std::collections::HashSet<_> = live.into_iter().collect();
                        batches.retain(|turn, _| live.contains(turn));
                    }
                    let n = batches.entry(turn_id).or_insert(0);
                    *n = n.saturating_add(1);
                    Ok(*n)
                })
            })
            .await
    }

    pub fn with_secret_key(mut self, key: Arc<crate::credentials::SecretKey>) -> Self {
        self.secret_key = Some(key);
        self
    }

    pub fn config(&self) -> &ComputerConfig {
        &self.config
    }

    /// The runtime's view of a computer (inspection only).
    pub async fn runtime_state(&self, name: &str) -> Result<Option<RuntimeState>, RuntimeError> {
        self.runtime.state(name).await
    }

    /// Every computer the runtime reports (inspection only).
    pub async fn runtime_list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        self.runtime.list().await
    }

    /// The runtime name of a bot's computer.
    pub fn runtime_name(bot_id: Uuid) -> String {
        format!("elsebot-{bot_id}")
    }

    async fn op(&self, bot_id: Uuid) -> OwnedSemaphorePermit {
        let semaphore = self
            .ops
            .call(move |ops| {
                ops.retain(|_, weak| weak.strong_count() > 0);
                if let Some(semaphore) = ops.get(&bot_id).and_then(std::sync::Weak::upgrade) {
                    return semaphore;
                }
                let semaphore = Arc::new(Semaphore::new(1));
                ops.insert(bot_id, Arc::downgrade(&semaphore));
                semaphore
            })
            .await;
        semaphore
            .acquire_owned()
            .await
            .expect("computer operation semaphore stays open")
    }

    async fn row(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<Option<Row>, sqlx::Error> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {ROW} FROM bot_computers WHERE bot_id = ?1"
        )))
        .bind(bot_id)
        .fetch_optional(&mut *conn)
        .await
    }

    /// Hold the bot's computer for `holder`, creating it on first use and
    /// starting or resuming it when stopped or paused. Retrying with the
    /// same holder returns its existing lease.
    pub async fn acquire(&self, bot_id: Uuid, holder: &str) -> Result<Acquire, ComputerError> {
        if !self.inspected.load(Ordering::SeqCst) {
            return Ok(Acquire::Queued {
                reason: "the computer runtime has not been inspected yet".into(),
            });
        }
        let _op = self.op(bot_id).await;
        let stopped = {
            let mut conn = self.pool.acquire().await?;
            Self::row(&mut conn, bot_id)
                .await?
                .filter(|row| row.observed_state == "stopped")
        };
        if let Some(row) = stopped {
            self.configure_boot(bot_id, &row).await?;
        }
        for _ in 0..2 {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            let active: Option<bool> =
                sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = ?1")
                    .bind(bot_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if active != Some(true) {
                return Ok(Acquire::BotInactive);
            }
            let growth = self.config.growth();
            sqlx::query(
                "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib, network,
                                            max_cpus, max_memory_mib)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT DO NOTHING",
            )
            .bind(bot_id)
            .bind(Self::runtime_name(bot_id))
            .bind(&self.config.image)
            .bind(self.config.cpus as i32)
            .bind(self.config.memory_mib as i32)
            .bind(&self.config.network)
            .bind(growth.map(|(cpus, _)| cpus as i32))
            .bind(growth.map(|(_, memory)| memory as i32))
            .execute(&mut *tx)
            .await?;
            let row = Self::row(&mut tx, bot_id).await?.expect("inserted");
            if row.desired_state == "stopped" {
                return Ok(Acquire::OwnerStopped);
            }
            let wake = match row.observed_state.as_str() {
                "running" => {
                    let lease = self.lease(&mut tx, bot_id, holder, row.generation).await?;
                    tx.commit().await?;
                    self.ensure_services(bot_id, &row.runtime_name).await?;
                    return Ok(Acquire::Ready {
                        lease_id: lease,
                        runtime_name: row.runtime_name,
                        generation: row.generation,
                    });
                }
                "missing" => {
                    return Ok(Acquire::Unavailable {
                        reason: "this bot's computer is missing from the runtime; it was not \
                                 recreated"
                            .into(),
                    });
                }
                "absent" => Wake::Create,
                "stopped" => Wake::Start,
                "paused" => Wake::Resume,
                _ => {
                    drop(tx);
                    self.observe(bot_id).await?;
                    continue;
                }
            };
            return self.wake(tx, bot_id, holder, row, wake).await;
        }
        Ok(Acquire::Queued {
            reason: "the computer's state is uncertain; it is being checked".into(),
        })
    }

    async fn wake(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Sqlite>,
        bot_id: Uuid,
        holder: &str,
        row: Row,
        wake: Wake,
    ) -> Result<Acquire, ComputerError> {
        let (cpus, memory) = row.reserved();
        if wake != Wake::Resume && !self.admits(&mut tx, bot_id, cpus, memory).await? {
            return Ok(Acquire::Queued {
                reason: "the computer budget is in use; waiting for capacity".into(),
            });
        }
        let next = row.generation + 1;
        let transient = match wake {
            Wake::Create => "creating",
            Wake::Start => "starting",
            Wake::Resume => "resuming",
        };
        self.set_state(&mut tx, bot_id, transient, Some(next), None)
            .await?;
        tx.commit().await?;
        if crate::faults::hit("computer.after_start_decision") {
            return Err(ComputerError::Interrupted);
        }

        let result = match wake {
            Wake::Create => {
                let mut outcome = Err(RuntimeError::Unavailable("not attempted".into()));
                for _ in 0..3 {
                    let resources = match self.desktop_resources(bot_id).await {
                        Ok((ports, files)) => self
                            .project_mounts(bot_id)
                            .map(|mounts| (ports, files, mounts)),
                        Err(err) => Err(err),
                    };
                    outcome = match resources {
                        Ok((ports, files, mounts)) => {
                            self.runtime
                                .create(&Spec {
                                    name: row.runtime_name.clone(),
                                    image: row.image.clone(),
                                    cpus: row.cpus as u32,
                                    memory_mib: row.memory_mib as u32,
                                    max_cpus: row.max().0,
                                    max_memory_mib: row.max().1,
                                    network: row
                                        .network
                                        .clone()
                                        .unwrap_or_else(|| self.config.network.clone()),
                                    ports,
                                    files,
                                    mounts,
                                })
                                .await
                        }
                        Err(err) => Err(err),
                    };
                    let collided = matches!(&outcome, Err(err)
                        if { let e = err.to_string().to_lowercase(); e.contains("address") || e.contains("in use") || e.contains("desktop_port") });
                    let exists = matches!(self.runtime.state(&row.runtime_name).await, Ok(Some(_)));
                    if !(self.config.desktop && collided && !exists) {
                        break;
                    }
                }
                outcome
            }
            Wake::Start => self.runtime.start(&row.runtime_name).await,
            Wake::Resume => self.runtime.resume(&row.runtime_name).await,
        };
        if let Err(err) = result {
            self.observe_after_failure(bot_id, &err.to_string()).await?;
            return Ok(Acquire::Unavailable {
                reason: err.to_string(),
            });
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active: Option<bool> =
            sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = ?1")
                .bind(bot_id)
                .fetch_optional(&mut *tx)
                .await?;
        let row = Self::row(&mut tx, bot_id).await?.expect("exists");
        self.set_state(&mut tx, bot_id, "running", None, None)
            .await?;
        sqlx::query("UPDATE bot_computers SET browser_checkpoint = NULL WHERE bot_id = ?1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        if active != Some(true) || row.desired_state == "stopped" {
            tx.commit().await?;
            self.stop_held(bot_id, Reason::MustStop).await?;
            return Ok(if active != Some(true) {
                Acquire::BotInactive
            } else {
                Acquire::OwnerStopped
            });
        }
        sqlx::query("UPDATE bot_computers SET last_error = NULL WHERE bot_id = ?1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        let lease = self.lease(&mut tx, bot_id, holder, next).await?;
        tx.commit().await?;
        self.ensure_services(bot_id, &row.runtime_name).await?;
        Ok(Acquire::Ready {
            lease_id: lease,
            runtime_name: row.runtime_name,
            generation: next,
        })
    }

    pub(crate) fn project_mounts(&self, bot_id: Uuid) -> Result<Vec<Mount>, RuntimeError> {
        let Some(root) = &self.config.projects_dir else {
            return Ok(Vec::new());
        };
        // Microsandbox refuses a host path through a symlink (macOS's /tmp,
        // or an owner's linked directory), so the resolved path is mounted.
        let dir = crate::providers::host::project_dir(root, bot_id)
            .and_then(std::fs::canonicalize)
            .map_err(|e| RuntimeError::Unavailable(format!("the bot's project directory: {e}")))?;
        let (guest, uid) = if self.config.desktop {
            (GUEST_PROJECT, GUEST_UID)
        } else {
            (ROOT_PROJECT, 0)
        };
        Ok(vec![Mount {
            host: dir,
            guest: guest.into(),
            uid,
        }])
    }

    async fn set_state(
        &self,
        conn: &mut SqliteConnection,
        bot_id: Uuid,
        state: &str,
        generation: Option<i64>,
        error: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE bot_computers SET observed_state = ?2, generation = coalesce(?3, generation),
                 last_error = coalesce(?4, last_error), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .bind(state)
        .bind(generation)
        .bind(error.map(|e| bounded(e, 2000)))
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    async fn observe(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT runtime_name, observed_state FROM bot_computers WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((name, was)) = row else {
            return Ok(());
        };
        let state = match self.runtime.state(&name).await {
            Ok(Some(RuntimeState::Running)) => "running",
            Ok(Some(RuntimeState::Paused)) => "paused",
            Ok(Some(RuntimeState::Stopped)) => "stopped",
            Ok(Some(RuntimeState::Failed)) | Err(_) => "error",
            Ok(None) if matches!(was.as_str(), "absent" | "creating") => "absent",
            Ok(None) => "missing",
        };
        let mut conn = self.pool.acquire().await?;
        self.set_state(&mut conn, bot_id, state, None, None).await?;
        Ok(())
    }

    async fn observe_after_failure(&self, bot_id: Uuid, error: &str) -> Result<(), ComputerError> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query(
            "UPDATE bot_computers SET last_error = ?2,
                 observed_state = CASE WHEN observed_state = 'creating' THEN 'creating' ELSE 'error' END,
                 updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .bind(bounded(error, 2000))
        .execute(&mut *conn)
        .await?;
        drop(conn);
        self.observe(bot_id).await
    }

    /// Set a stopped computer to boot at the configured size, able to grow
    /// to the desktop size, and return its row as it now is. Growth persists
    /// across a restart, so a computer that grew stays counted at its
    /// maximum until this reset succeeds. The caller holds the bot's
    /// operation lock, and admits the start after this, against the result.
    async fn configure_boot(&self, bot_id: Uuid, row: &Row) -> Result<Row, ComputerError> {
        let Some((max_cpus, max_memory)) = self.config.growth() else {
            return Ok(row.clone());
        };
        let (cpus, memory) = (self.config.cpus, self.config.memory_mib);
        match self
            .runtime
            .configure(&row.runtime_name, cpus, memory, max_cpus, max_memory)
            .await
        {
            Ok(()) => {
                sqlx::query(
                    "UPDATE bot_computers SET cpus = ?2, memory_mib = ?3, max_cpus = ?4,
                         max_memory_mib = ?5, grown = 0
                     WHERE bot_id = ?1",
                )
                .bind(bot_id)
                .bind(cpus as i32)
                .bind(memory as i32)
                .bind(max_cpus as i32)
                .bind(max_memory as i32)
                .execute(&self.pool)
                .await?;
            }
            Err(err) => {
                tracing::warn!(bot = %bot_id, error = %err, "computer boot size unchanged");
            }
        }
        let mut conn = self.pool.acquire().await?;
        Ok(Self::row(&mut conn, bot_id).await?.expect("exists"))
    }

    /// Whether the budget, besides every other active computer, has room for
    /// this one at `cpus` and `memory_mib`.
    async fn admits(
        &self,
        tx: &mut SqliteConnection,
        bot_id: Uuid,
        cpus_wanted: u32,
        memory_wanted: u32,
    ) -> Result<bool, sqlx::Error> {
        let (cpus, memory): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(sum({RESERVED_CPUS}), 0), coalesce(sum({RESERVED_MEMORY} + ?2), 0)
             FROM bot_computers WHERE bot_id <> ?1 AND observed_state IN {RESERVING}"
        )))
        .bind(bot_id)
        .bind(self.config.overhead_mib as i64)
        .fetch_one(&mut *tx)
        .await?;
        let needs_cpus = cpus + cpus_wanted as i64;
        let needs_memory = memory + memory_wanted as i64 + self.config.overhead_mib as i64;
        Ok(needs_cpus <= self.config.max_cpus as i64
            && needs_memory <= self.config.max_memory_mib as i64)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn errors_are_cut_by_bytes_on_character_boundaries() {
        let text = "é".repeat(1500);
        let cut = super::bounded(&text, 2000);
        assert!(cut.len() <= 2000 && cut.chars().all(|c| c == 'é'));
        assert_eq!(super::bounded("short", 2000), "short");
    }
}
