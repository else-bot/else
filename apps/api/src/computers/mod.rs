//! Bot computers: one persistent micro-VM per bot, started on demand
//! (POC-017).
//!
//! PostgreSQL holds each computer's identity, configuration, owner intent
//! (`desired_state`), what the API last established (`observed_state`), a
//! transition generation, and renewable leases. Runtime operations for one
//! bot (create, start, resume, stop, pause, exec) are serialized by an
//! in-process lock (the API is a singleton), so a slow or failed operation
//! can never act on a newer one; each decision is also taken under the
//! computer's row lock and re-checked when the runtime call returns. A
//! failed transition is followed by inspecting the runtime and recording
//! what it actually reports; when that is unknown the computer is `error`,
//! keeps its reservation, and maintenance inspects it again. Admission
//! counts running, paused, transitioning, and unknown computers against a
//! global CPU and memory budget. Startup only inspects: it never wakes,
//! recreates, replaces, or removes a computer, and no computer is used until
//! that inspection has succeeded. Provider CLIs and credentials stay on the
//! host.

pub mod jobs;
pub mod runtime;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use sqlx::{PgConnection, PgPool};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub use runtime::{ComputerRuntime, ExecOutput, RuntimeError, RuntimeState, Spec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAction {
    Stop,
    Pause,
}

#[derive(Debug, Clone)]
pub struct ComputerConfig {
    pub image: String,
    pub cpus: u32,
    pub memory_mib: u32,
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
}

/// Guest port of the desktop's noVNC (see infra/guest).
pub const DESKTOP_GUEST_PORT: u16 = 6080;
/// Starts the supervised desktop services; idempotent.
pub const GUEST_INIT: &str = "/usr/local/bin/else-init";
/// Closes the browser cleanly before a stop.
pub const GUEST_CHECKPOINT: &str = "/usr/local/bin/else-checkpoint";

impl Default for ComputerConfig {
    fn default() -> Self {
        Self {
            image: "python:3.12-slim".into(),
            cpus: 1,
            memory_mib: 2048,
            overhead_mib: 256,
            max_cpus: 2,
            max_memory_mib: 2 * (2048 + 256),
            idle_after: Duration::from_secs(600),
            idle_action: IdleAction::Stop,
            lease_ttl: Duration::from_secs(300),
            network: "public".into(),
            desktop: false,
            state_dir: std::env::temp_dir().join("else-computers"),
        }
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
}

/// States that hold a budget reservation: anything that is or may be live.
const RESERVING: &str = "('creating', 'starting', 'running', 'pausing', 'paused', 'resuming', \
                         'stopping', 'error')";

/// Columns of a computer row used for decisions.
#[derive(Debug, sqlx::FromRow)]
struct Row {
    runtime_name: String,
    image: String,
    cpus: i32,
    memory_mib: i32,
    desired_state: String,
    observed_state: String,
    generation: i64,
}

const ROW: &str =
    "runtime_name, image, cpus, memory_mib, desired_state, observed_state, generation";

/// The runtime operation a start decision needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    Create,
    Start,
    Resume,
}

/// Why a computer should stop or pause; re-checked under the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    /// The owner stopped it or the bot is no longer active.
    MustStop,
    /// No lease and unused since the idle timeout.
    Idle(IdleAction),
}

/// Text cut to at most `max` bytes on a character boundary.
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
    pool: PgPool,
    runtime: Arc<dyn ComputerRuntime>,
    config: ComputerConfig,
    /// One runtime operation at a time per bot.
    ops: std::sync::Mutex<HashMap<Uuid, Arc<tokio::sync::Mutex<()>>>>,
    /// The startup inspection succeeded; until then nothing is used.
    inspected: AtomicBool,
}

impl ComputerManager {
    pub fn new(pool: PgPool, runtime: Arc<dyn ComputerRuntime>, config: ComputerConfig) -> Self {
        Self {
            pool,
            runtime,
            config,
            ops: std::sync::Mutex::new(HashMap::new()),
            inspected: AtomicBool::new(false),
        }
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

    async fn op(&self, bot_id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self
            .ops
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(bot_id)
            .or_default()
            .clone();
        lock.lock_owned().await
    }

    async fn row(conn: &mut PgConnection, bot_id: Uuid) -> Result<Option<Row>, sqlx::Error> {
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {ROW} FROM bot_computers WHERE bot_id = $1 FOR UPDATE"
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
        // At most one inspection of an uncertain state, then decide again.
        for _ in 0..2 {
            let mut tx = self.pool.begin().await?;
            let active: Option<bool> =
                sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = $1 FOR SHARE")
                    .bind(bot_id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if active != Some(true) {
                return Ok(Acquire::BotInactive);
            }
            sqlx::query(
                "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib)
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
            )
            .bind(bot_id)
            .bind(Self::runtime_name(bot_id))
            .bind(&self.config.image)
            .bind(self.config.cpus as i32)
            .bind(self.config.memory_mib as i32)
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
                    // A boot whose services never started (a failed or
                    // interrupted init) is retried on use.
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
                // Unknown or interrupted: look at the runtime, then decide.
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

    /// Start, create, or resume under the bot's operation lock.
    async fn wake(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        bot_id: Uuid,
        holder: &str,
        row: Row,
        wake: Wake,
    ) -> Result<Acquire, ComputerError> {
        if wake != Wake::Resume && !self.admits(&mut tx, bot_id, &row).await? {
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

        // The runtime call happens outside the database locks.
        let result = match wake {
            Wake::Create => {
                // A port taken between allocation and the runtime's bind
                // is retried with another one.
                let mut outcome = Err(RuntimeError::Unavailable("not attempted".into()));
                for _ in 0..3 {
                    outcome = match self.desktop_resources(bot_id).await {
                        Ok((ports, files)) => {
                            self.runtime
                                .create(&Spec {
                                    name: row.runtime_name.clone(),
                                    image: row.image.clone(),
                                    cpus: row.cpus as u32,
                                    memory_mib: row.memory_mib as u32,
                                    network: self.config.network.clone(),
                                    ports,
                                    files,
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
            // What is actually there now? Record it; unknown keeps the
            // reservation until maintenance can tell.
            self.observe_after_failure(bot_id, &err.to_string()).await?;
            return Ok(Acquire::Unavailable {
                reason: err.to_string(),
            });
        }
        let mut tx = self.pool.begin().await?;
        let active: Option<bool> =
            sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = $1 FOR SHARE")
                .bind(bot_id)
                .fetch_optional(&mut *tx)
                .await?;
        let row = Self::row(&mut tx, bot_id).await?.expect("exists");
        self.set_state(&mut tx, bot_id, "running", None, None)
            .await?;
        if active != Some(true) || row.desired_state == "stopped" {
            // Deleted or stopped by the owner while starting: it does not
            // stay up, and no lease is granted.
            tx.commit().await?;
            self.stop_held(bot_id, Reason::MustStop).await?;
            return Ok(if active != Some(true) {
                Acquire::BotInactive
            } else {
                Acquire::OwnerStopped
            });
        }
        sqlx::query("UPDATE bot_computers SET last_error = NULL WHERE bot_id = $1")
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

    /// For a desktop computer: a free host loopback port for noVNC (saved
    /// with the computer) and a fresh VNC password in a private host file
    /// that is copied into the guest.
    async fn desktop_resources(
        &self,
        bot_id: Uuid,
    ) -> Result<(Vec<(u16, u16)>, Vec<(std::path::PathBuf, String)>), RuntimeError> {
        if !self.config.desktop {
            return Ok((Vec::new(), Vec::new()));
        }
        let unavailable = |e: String| RuntimeError::Unavailable(e);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map_err(|e| unavailable(e.to_string()))?
            .port();
        sqlx::query("UPDATE bot_computers SET desktop_port = $2 WHERE bot_id = $1")
            .bind(bot_id)
            .bind(port as i32)
            .execute(&self.pool)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let password = self
            .write_vnc_password(bot_id)
            .map_err(|e| unavailable(e.to_string()))?;
        Ok((
            vec![(port, DESKTOP_GUEST_PORT)],
            vec![(password, "/etc/elsebot/vnc.pass".into())],
        ))
    }

    fn secret_dir(&self, bot_id: Uuid) -> std::path::PathBuf {
        self.config
            .state_dir
            .join("computers")
            .join(bot_id.to_string())
    }

    fn write_vnc_password(&self, bot_id: Uuid) -> std::io::Result<std::path::PathBuf> {
        use base64::Engine;
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let dir = self.secret_dir(bot_id);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut bytes = [0u8; 12];
        getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
        // VNC uses at most 8 characters of a password.
        let password: String = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(bytes)
            .chars()
            .take(8)
            .collect();
        let path = dir.join("vnc.pass");
        let temp = dir.join(format!(".vnc.pass.{}", Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        writeln!(file, "{password}")?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        Ok(path)
    }

    /// Owner access to a desktop: hold the computer with a renewable
    /// `desktop:` lease (an open VNC session counts as use) and return its
    /// loopback address and VNC password. Calling again renews it.
    pub async fn desktop(&self, bot_id: Uuid) -> Result<Result<Desktop, Acquire>, ComputerError> {
        if !self.config.desktop {
            return Ok(Err(Acquire::Unavailable {
                reason: "this installation's computers have no desktop".into(),
            }));
        }
        let acquired = self.acquire(bot_id, &format!("desktop:{bot_id}")).await?;
        let Acquire::Ready { lease_id, .. } = acquired else {
            return Ok(Err(acquired));
        };
        let (port, ready): (Option<i32>, bool) = sqlx::query_as(
            "SELECT desktop_port, services_generation IS NOT DISTINCT FROM generation
             FROM bot_computers WHERE bot_id = $1",
        )
        .bind(bot_id)
        .fetch_one(&self.pool)
        .await?;
        if !ready {
            self.release(lease_id).await?;
            return Ok(Err(Acquire::Queued {
                reason: "the desktop services are not running yet; try again shortly".into(),
            }));
        }
        let password = std::fs::read_to_string(self.secret_dir(bot_id).join("vnc.pass"))
            .map(|p| p.trim().to_owned())
            .ok();
        match (port, password) {
            (Some(port), Some(password)) => Ok(Ok(Desktop {
                lease_id,
                url: format!("http://127.0.0.1:{port}/vnc.html?autoconnect=1&resize=scale"),
                password,
                lease_seconds: self.config.lease_ttl.as_secs(),
            })),
            _ => {
                self.release(lease_id).await?;
                Ok(Err(Acquire::Unavailable {
                    reason: "this computer was created without a desktop".into(),
                }))
            }
        }
    }

    /// End the owner's desktop session.
    pub async fn close_desktop(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let lease: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM computer_leases WHERE bot_id = $1 AND holder = $2 AND released_at IS NULL",
        )
        .bind(bot_id)
        .bind(format!("desktop:{bot_id}"))
        .fetch_optional(&self.pool)
        .await?;
        if let Some(lease) = lease {
            self.release(lease).await?;
        }
        Ok(())
    }

    /// Start the desktop services for the current boot unless already done
    /// for this generation. The caller holds the bot's operation lock. A
    /// failure is recorded and retried on the next use.
    async fn ensure_services(&self, bot_id: Uuid, name: &str) -> Result<(), ComputerError> {
        if !self.config.desktop {
            return Ok(());
        }
        let (generation, done): (i64, Option<i64>) = sqlx::query_as(
            "SELECT generation, services_generation FROM bot_computers WHERE bot_id = $1",
        )
        .bind(bot_id)
        .fetch_one(&self.pool)
        .await?;
        if done == Some(generation) {
            return Ok(());
        }
        let result = self
            .runtime
            .exec(name, GUEST_INIT, &[], Duration::from_secs(60))
            .await;
        match result {
            Ok(out) if out.status == Some(0) => {
                sqlx::query(
                    "UPDATE bot_computers SET services_generation = $2 WHERE bot_id = $1 AND generation = $2",
                )
                .bind(bot_id)
                .bind(generation)
                .execute(&self.pool)
                .await?;
            }
            other => {
                let error = match other {
                    Ok(out) => format!("guest services did not start: {}", out.stderr.trim()),
                    Err(err) => format!("guest services did not start: {err}"),
                };
                tracing::warn!(bot = %bot_id, %error, "desktop services");
                sqlx::query("UPDATE bot_computers SET last_error = $2 WHERE bot_id = $1")
                    .bind(bot_id)
                    .bind(bounded(&error, 2000))
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    async fn set_state(
        &self,
        conn: &mut PgConnection,
        bot_id: Uuid,
        state: &str,
        generation: Option<i64>,
        error: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE bot_computers SET observed_state = $2, generation = coalesce($3, generation),
                 last_error = coalesce($4, last_error), updated_at = now()
             WHERE bot_id = $1",
        )
        .bind(bot_id)
        .bind(state)
        .bind(generation)
        .bind(error.map(|e| bounded(e, 2000)))
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    /// Record the runtime's actual state for a bot's computer. The caller
    /// holds the bot's operation lock.
    async fn observe(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT runtime_name, observed_state FROM bot_computers WHERE bot_id = $1",
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
            // Gone: never created (absent) or lost (missing).
            Ok(None) if matches!(was.as_str(), "absent" | "creating") => "absent",
            Ok(None) => "missing",
        };
        let mut conn = self.pool.acquire().await?;
        self.set_state(&mut conn, bot_id, state, None, None).await?;
        Ok(())
    }

    async fn observe_after_failure(&self, bot_id: Uuid, error: &str) -> Result<(), ComputerError> {
        let mut conn = self.pool.acquire().await?;
        // Keep a create that may not have happened distinguishable from a
        // lost computer: `creating` stays until observed.
        sqlx::query(
            "UPDATE bot_computers SET last_error = $2,
                 observed_state = CASE WHEN observed_state = 'creating' THEN 'creating' ELSE 'error' END,
                 updated_at = now()
             WHERE bot_id = $1",
        )
        .bind(bot_id)
        .bind(bounded(error, 2000))
        .execute(&mut *conn)
        .await?;
        drop(conn);
        self.observe(bot_id).await
    }

    /// Whether starting this computer stays within the global budget.
    async fn admits(
        &self,
        tx: &mut PgConnection,
        bot_id: Uuid,
        row: &Row,
    ) -> Result<bool, sqlx::Error> {
        // Serialize admission across computers.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('else_computer_admission'))")
            .execute(&mut *tx)
            .await?;
        let (cpus, memory): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT coalesce(sum(cpus), 0)::bigint, coalesce(sum(memory_mib + $2), 0)::bigint
             FROM bot_computers WHERE bot_id <> $1 AND observed_state IN {RESERVING}"
        )))
        .bind(bot_id)
        .bind(self.config.overhead_mib as i64)
        .fetch_one(&mut *tx)
        .await?;
        let needs_cpus = cpus + row.cpus as i64;
        let needs_memory = memory + row.memory_mib as i64 + self.config.overhead_mib as i64;
        Ok(needs_cpus <= self.config.max_cpus as i64
            && needs_memory <= self.config.max_memory_mib as i64)
    }

    async fn lease(
        &self,
        tx: &mut PgConnection,
        bot_id: Uuid,
        holder: &str,
        generation: i64,
    ) -> Result<Uuid, sqlx::Error> {
        let existing: Option<Uuid> = sqlx::query_scalar(
            "UPDATE computer_leases SET expires_at = now() + make_interval(secs => $3),
                 generation = $4
             WHERE bot_id = $1 AND holder = $2 AND released_at IS NULL RETURNING id",
        )
        .bind(bot_id)
        .bind(holder)
        .bind(self.config.lease_ttl.as_secs_f64())
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
        let id = match existing {
            Some(id) => id,
            None => {
                let id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO computer_leases (id, bot_id, holder, generation, expires_at)
                     VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))",
                )
                .bind(id)
                .bind(bot_id)
                .bind(holder)
                .bind(generation)
                .bind(self.config.lease_ttl.as_secs_f64())
                .execute(&mut *tx)
                .await?;
                id
            }
        };
        sqlx::query("UPDATE bot_computers SET last_used_at = now() WHERE bot_id = $1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        Ok(id)
    }

    /// Extend a live lease. False if it already ended.
    pub async fn renew(&self, lease_id: Uuid) -> Result<bool, ComputerError> {
        let renewed = sqlx::query(
            "UPDATE computer_leases SET expires_at = now() + make_interval(secs => $2)
             WHERE id = $1 AND released_at IS NULL",
        )
        .bind(lease_id)
        .bind(self.config.lease_ttl.as_secs_f64())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if crate::faults::hit("lease.after_renew") {
            return Err(ComputerError::Interrupted);
        }
        Ok(renewed == 1)
    }

    /// End a lease. Repeating it is harmless. Locks the computer, then the
    /// lease, like every other path.
    pub async fn release(&self, lease_id: Uuid) -> Result<(), ComputerError> {
        let mut tx = self.pool.begin().await?;
        let bot: Option<Uuid> =
            sqlx::query_scalar("SELECT bot_id FROM computer_leases WHERE id = $1")
                .bind(lease_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(bot) = bot else { return Ok(()) };
        sqlx::query("SELECT 1 FROM bot_computers WHERE bot_id = $1 FOR UPDATE")
            .bind(bot)
            .execute(&mut *tx)
            .await?;
        let ended = sqlx::query(
            "UPDATE computer_leases SET released_at = now(), end_reason = 'released'
             WHERE id = $1 AND released_at IS NULL",
        )
        .bind(lease_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if ended == 1 {
            sqlx::query("UPDATE bot_computers SET last_used_at = now() WHERE bot_id = $1")
                .bind(bot)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Run a command in the computer a live lease holds. Checked and run
    /// under the bot's operation lock, so a stop cannot slip in between the
    /// check and the command (which would otherwise wake a stopped VM).
    pub async fn exec(
        &self,
        lease_id: Uuid,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, ComputerError> {
        let (bot, name, _op) = self.checked(lease_id).await?;
        let out = self.runtime.exec(&name, program, args, timeout).await?;
        sqlx::query("UPDATE bot_computers SET last_used_at = now() WHERE bot_id = $1")
            .bind(bot)
            .execute(&self.pool)
            .await?;
        Ok(out)
    }

    /// Validate a lease under its bot's operation lock: live, its bot
    /// active, and its computer running at the lease's generation with no
    /// owner stop. The returned guard keeps a stop from slipping in before
    /// the caller's runtime call (which could otherwise wake a stopped VM).
    pub(crate) async fn checked(
        &self,
        lease_id: Uuid,
    ) -> Result<(Uuid, String, tokio::sync::OwnedMutexGuard<()>), ComputerError> {
        let bot: Option<Uuid> =
            sqlx::query_scalar("SELECT bot_id FROM computer_leases WHERE id = $1")
                .bind(lease_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(bot) = bot else {
            return Err(ComputerError::LeaseEnded);
        };
        let op = self.op(bot).await;
        let ok: Option<(String, bool, bool)> = sqlx::query_as(
            "SELECT c.runtime_name,
                    c.observed_state = 'running' AND c.desired_state = 'auto'
                        AND l.generation = c.generation,
                    l.released_at IS NULL AND b.status = 'active'
             FROM computer_leases l JOIN bot_computers c ON c.bot_id = l.bot_id
             JOIN bots b ON b.id = l.bot_id WHERE l.id = $1",
        )
        .bind(lease_id)
        .fetch_optional(&self.pool)
        .await?;
        match ok {
            Some((name, true, true)) => Ok((bot, name, op)),
            Some((_, false, true)) => Err(ComputerError::NotRunning),
            _ => Err(ComputerError::LeaseEnded),
        }
    }

    /// Owner stop: no implicit wake until resumed. Active leases are
    /// revoked and the computer stops; its disk is kept.
    pub async fn owner_stop(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let mut tx = self.pool.begin().await?;
        // The bot row serializes this with a first acquisition creating
        // the computer row; the upsert records the intent either way.
        sqlx::query("SELECT 1 FROM bots WHERE id = $1 FOR SHARE")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib, desired_state)
             VALUES ($1, $2, $3, $4, $5, 'stopped')
             ON CONFLICT (bot_id) DO UPDATE SET desired_state = 'stopped', updated_at = now()",
        )
        .bind(bot_id)
        .bind(Self::runtime_name(bot_id))
        .bind(&self.config.image)
        .bind(self.config.cpus as i32)
        .bind(self.config.memory_mib as i32)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE computer_leases SET released_at = now(), end_reason = 'owner_stop'
             WHERE bot_id = $1 AND released_at IS NULL",
        )
        .bind(bot_id)
        .execute(&mut *tx)
        .await?;
        // The intent and revoked leases commit before the runtime is asked
        // to stop, so nothing can acquire the computer in between.
        tx.commit().await?;
        let _op = self.op(bot_id).await;
        self.stop_held(bot_id, Reason::MustStop).await.map(|_| ())
    }

    /// Clear an owner stop. The computer starts again only on next use.
    pub async fn owner_resume(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        sqlx::query(
            "UPDATE bot_computers SET desired_state = 'auto', updated_at = now() WHERE bot_id = $1",
        )
        .bind(bot_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stop or pause if `reason` still holds, re-checked under the row lock
    /// together with any lease revocation. The caller holds the bot's
    /// operation lock. Returns whether it acted.
    async fn stop_held(&self, bot_id: Uuid, reason: Reason) -> Result<bool, ComputerError> {
        let mut tx = self.pool.begin().await?;
        let active: Option<bool> =
            sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = $1 FOR SHARE")
                .bind(bot_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(row) = Self::row(&mut tx, bot_id).await? else {
            return Ok(false);
        };
        let action = match reason {
            Reason::MustStop => {
                if active == Some(true) && row.desired_state != "stopped" {
                    return Ok(false);
                }
                sqlx::query(
                    "UPDATE computer_leases SET released_at = now(), end_reason = 'revoked'
                     WHERE bot_id = $1 AND released_at IS NULL",
                )
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
                IdleAction::Stop
            }
            Reason::Idle(action) => {
                let idle: bool = sqlx::query_scalar(
                    "SELECT NOT EXISTS (SELECT 1 FROM computer_leases
                                        WHERE bot_id = $1 AND released_at IS NULL)
                        AND coalesce(last_used_at, updated_at) < now() - make_interval(secs => $2)
                     FROM bot_computers WHERE bot_id = $1",
                )
                .bind(bot_id)
                .bind(self.config.idle_after.as_secs_f64())
                .fetch_one(&mut *tx)
                .await?;
                if !idle || active != Some(true) || row.desired_state != "auto" {
                    return Ok(false);
                }
                action
            }
        };
        let applies = match action {
            IdleAction::Stop => {
                matches!(row.observed_state.as_str(), "running" | "paused" | "error")
            }
            IdleAction::Pause => row.observed_state == "running",
        };
        if !applies {
            tx.commit().await?;
            return Ok(false);
        }
        let (transient, done) = match action {
            IdleAction::Stop => ("stopping", "stopped"),
            IdleAction::Pause => ("pausing", "paused"),
        };
        self.set_state(&mut tx, bot_id, transient, Some(row.generation + 1), None)
            .await?;
        tx.commit().await?;
        if crate::faults::hit("computer.after_stop_decision") {
            return Err(ComputerError::Interrupted);
        }
        tracing::info!(bot = %bot_id, ?reason, "computer transition");
        // A desktop closes its browser cleanly first, so the profile is
        // flushed and keeps no lock (pausing keeps processes as they are).
        if self.config.desktop && action == IdleAction::Stop && row.observed_state == "running" {
            let _ = self
                .runtime
                .exec(
                    &row.runtime_name,
                    GUEST_CHECKPOINT,
                    &[],
                    Duration::from_secs(30),
                )
                .await;
        }
        let result = match action {
            IdleAction::Stop => self.runtime.stop(&row.runtime_name).await,
            IdleAction::Pause => self.runtime.pause(&row.runtime_name).await,
        };
        match result {
            Ok(()) => {
                let mut conn = self.pool.acquire().await?;
                self.set_state(&mut conn, bot_id, done, None, None).await?;
                Ok(true)
            }
            Err(err) => {
                // Not proven stopped: record what the runtime says; an
                // unknown state keeps the reservation and is retried.
                self.observe_after_failure(bot_id, &err.to_string()).await?;
                Ok(false)
            }
        }
    }

    /// One maintenance pass: finish the startup inspection if needed,
    /// reconcile expired leases against live work, re-inspect uncertain
    /// computers, stop computers of deleted or owner-stopped bots (retried
    /// until confirmed), and stop or pause idle ones. Stops early when
    /// cancelled.
    pub async fn tick(&self, cancel: &CancellationToken) -> Result<(), ComputerError> {
        if !self.inspected.load(Ordering::SeqCst) {
            self.reconcile_startup().await?;
            if !self.inspected.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
        let expired: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
            "SELECT id, bot_id, holder FROM computer_leases
             WHERE released_at IS NULL AND expires_at < now()",
        )
        .fetch_all(&self.pool)
        .await?;
        for (lease, bot, holder) in expired {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if self.holder_is_live(&holder).await? {
                self.renew(lease).await?;
                continue;
            }
            let mut tx = self.pool.begin().await?;
            sqlx::query("SELECT 1 FROM bot_computers WHERE bot_id = $1 FOR UPDATE")
                .bind(bot)
                .execute(&mut *tx)
                .await?;
            // Only if it is still expired: a renewal meanwhile wins.
            sqlx::query(
                "UPDATE computer_leases SET released_at = now(), end_reason = 'expired'
                 WHERE id = $1 AND released_at IS NULL AND expires_at < now()",
            )
            .bind(lease)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        // Uncertain or interrupted states: look again.
        let uncertain: Vec<Uuid> = sqlx::query_scalar(
            "SELECT bot_id FROM bot_computers WHERE observed_state IN
                 ('error', 'creating', 'starting', 'resuming', 'stopping', 'pausing')",
        )
        .fetch_all(&self.pool)
        .await?;
        for bot in uncertain {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let _op = self.op(bot).await;
            self.observe(bot).await?;
        }
        if let Err(err) = self.poll_jobs(cancel).await {
            tracing::warn!(error = %err, "polling guest jobs failed");
        }
        let must_stop: Vec<Uuid> = sqlx::query_scalar(
            "SELECT c.bot_id FROM bot_computers c JOIN bots b ON b.id = c.bot_id
             WHERE c.observed_state IN ('running', 'paused', 'error')
               AND (b.status <> 'active' OR c.desired_state = 'stopped')",
        )
        .fetch_all(&self.pool)
        .await?;
        for bot in must_stop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let _op = self.op(bot).await;
            self.stop_held(bot, Reason::MustStop).await?;
        }
        let idle_states: Vec<&str> = match self.config.idle_action {
            IdleAction::Stop => vec!["running", "paused"],
            IdleAction::Pause => vec!["running"],
        };
        let idle: Vec<Uuid> = sqlx::query_scalar(
            "SELECT c.bot_id FROM bot_computers c
             WHERE c.observed_state = ANY($1)
               AND coalesce(c.last_used_at, c.updated_at) < now() - make_interval(secs => $2)
               AND NOT EXISTS (SELECT 1 FROM computer_leases l
                               WHERE l.bot_id = c.bot_id AND l.released_at IS NULL)",
        )
        .bind(&idle_states)
        .bind(self.config.idle_after.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        for bot in idle {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let _op = self.op(bot).await;
            self.stop_held(bot, Reason::Idle(self.config.idle_action))
                .await?;
        }
        Ok(())
    }

    /// Whether a lease holder's work is still live. Turns are live while
    /// they run; holders this version does not know end with their lease.
    async fn holder_is_live(&self, holder: &str) -> Result<bool, sqlx::Error> {
        let Some((kind, id)) = holder.split_once(':') else {
            return Ok(false);
        };
        let Ok(id) = id.parse::<Uuid>() else {
            return Ok(false);
        };
        match kind {
            "turn" => {
                sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM turns WHERE id = $1
                                AND status IN ('preparing', 'sending', 'running', 'cancelling'))",
                )
                .bind(id)
                .fetch_one(&self.pool)
                .await
            }
            "job" => {
                sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM guest_jobs WHERE id = $1
                                    AND status IN ('starting', 'running'))",
                )
                .bind(id)
                .fetch_one(&self.pool)
                .await
            }
            _ => Ok(false),
        }
    }

    /// Startup: record what the runtime reports without changing it. A
    /// recorded computer that is gone becomes `missing`, never recreated;
    /// an interrupted transition takes the runtime's actual state. Until
    /// this succeeds no computer is used, and maintenance retries it.
    pub async fn reconcile_startup(&self) -> Result<(), ComputerError> {
        let listed = match self.runtime.list().await {
            Ok(listed) => listed,
            Err(err) => {
                tracing::warn!(error = %err, "computer runtime inspection failed; retrying");
                return Ok(());
            }
        };
        let rows: Vec<(Uuid, String, String)> =
            sqlx::query_as("SELECT bot_id, runtime_name, observed_state FROM bot_computers")
                .fetch_all(&self.pool)
                .await?;
        for (bot, name, observed) in rows {
            let actual = listed.iter().find(|(n, _)| *n == name).map(|(_, s)| *s);
            let state = match actual {
                Some(RuntimeState::Running) => "running",
                Some(RuntimeState::Paused) => "paused",
                Some(RuntimeState::Stopped) => "stopped",
                Some(RuntimeState::Failed) => "error",
                None if observed == "absent" || observed == "creating" => "absent",
                None => "missing",
            };
            if state != observed {
                tracing::info!(bot = %bot, from = %observed, to = state, "computer state reconciled");
            }
            sqlx::query(
                "UPDATE bot_computers SET observed_state = $2, generation = generation + 1,
                     updated_at = now()
                 WHERE bot_id = $1",
            )
            .bind(bot)
            .bind(state)
            .execute(&self.pool)
            .await?;
        }
        // Work that survived (jobs in a still-running computer) keeps its
        // leases on the new generation; maintenance probes it from there.
        sqlx::query(
            "UPDATE computer_leases l SET generation = c.generation
             FROM bot_computers c
             WHERE c.bot_id = l.bot_id AND l.released_at IS NULL
               AND c.observed_state IN ('running', 'paused')",
        )
        .execute(&self.pool)
        .await?;
        self.inspected.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Maintenance loop until cancelled.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            if let Err(err) = self.tick(&cancel).await {
                tracing::warn!(error = %err, "computer maintenance failed");
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn errors_are_cut_by_bytes_on_character_boundaries() {
        let text = "é".repeat(1500); // 3,000 bytes
        let cut = super::bounded(&text, 2000);
        assert!(cut.len() <= 2000 && cut.chars().all(|c| c == 'é'));
        assert_eq!(super::bounded("short", 2000), "short");
    }
}
