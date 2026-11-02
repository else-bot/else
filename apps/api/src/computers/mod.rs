//! Bot computers: one persistent micro-VM per bot, started on demand
//! (POC-017).
//!
//! PostgreSQL holds each computer's identity, configuration, owner intent
//! (`desired_state`), what the API last established (`observed_state`), a
//! transition generation, and renewable leases. Every decision is taken
//! under the computer's row lock; runtime calls happen outside it, and their
//! result is recorded only if the generation is unchanged. Admission counts
//! running, paused, and starting computers against a global CPU and memory
//! budget, so a paused computer keeps its reservation until it stops.
//! Startup only inspects: it never wakes, recreates, replaces, or removes a
//! computer. Provider CLIs and credentials stay on the host.

pub mod runtime;

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
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
}

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
    /// Try again later: the budget is full or a transition is under way.
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
}

pub struct ComputerManager {
    pool: PgPool,
    runtime: Arc<dyn ComputerRuntime>,
    config: ComputerConfig,
}

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

/// The runtime operation a start decision needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    Create,
    Start,
    Resume,
}

impl ComputerManager {
    pub fn new(pool: PgPool, runtime: Arc<dyn ComputerRuntime>, config: ComputerConfig) -> Self {
        Self {
            pool,
            runtime,
            config,
        }
    }

    pub fn runtime(&self) -> &Arc<dyn ComputerRuntime> {
        &self.runtime
    }

    pub fn config(&self) -> &ComputerConfig {
        &self.config
    }

    /// The runtime name of a bot's computer.
    pub fn runtime_name(bot_id: Uuid) -> String {
        format!("elsebot-{bot_id}")
    }

    /// Hold the bot's computer for `holder`, creating it on first use and
    /// starting or resuming it when stopped or paused. Retrying with the
    /// same holder returns its existing lease.
    pub async fn acquire(&self, bot_id: Uuid, holder: &str) -> Result<Acquire, ComputerError> {
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
        let row: Row = sqlx::query_as(
            "SELECT runtime_name, image, cpus, memory_mib, desired_state, observed_state, generation
             FROM bot_computers WHERE bot_id = $1 FOR UPDATE",
        )
        .bind(bot_id)
        .fetch_one(&mut *tx)
        .await?;
        if row.desired_state == "stopped" {
            return Ok(Acquire::OwnerStopped);
        }
        let wake = match row.observed_state.as_str() {
            "running" => {
                let lease = self.lease(&mut tx, bot_id, holder, row.generation).await?;
                tx.commit().await?;
                return Ok(Acquire::Ready {
                    lease_id: lease,
                    runtime_name: row.runtime_name,
                    generation: row.generation,
                });
            }
            "missing" => {
                return Ok(Acquire::Unavailable {
                    reason: "this bot's computer is missing from the runtime; it was not recreated"
                        .into(),
                });
            }
            "absent" => Wake::Create,
            "stopped" | "error" => Wake::Start,
            "paused" => Wake::Resume,
            transition => {
                return Ok(Acquire::Queued {
                    reason: format!("the computer is {transition}"),
                });
            }
        };
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
        sqlx::query(
            "UPDATE bot_computers SET observed_state = $2, generation = $3, updated_at = now()
             WHERE bot_id = $1",
        )
        .bind(bot_id)
        .bind(transient)
        .bind(next)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        // The runtime call happens outside the lock.
        let result = match wake {
            Wake::Create => {
                self.runtime
                    .create(&Spec {
                        name: row.runtime_name.clone(),
                        image: row.image.clone(),
                        cpus: row.cpus as u32,
                        memory_mib: row.memory_mib as u32,
                    })
                    .await
            }
            Wake::Start => self.runtime.start(&row.runtime_name).await,
            Wake::Resume => self.runtime.resume(&row.runtime_name).await,
        };
        let mut tx = self.pool.begin().await?;
        let current: (i64, String) = sqlx::query_as(
            "SELECT generation, desired_state FROM bot_computers WHERE bot_id = $1 FOR UPDATE",
        )
        .bind(bot_id)
        .fetch_one(&mut *tx)
        .await?;
        if current.0 != next {
            // Someone (an owner stop) decided otherwise meanwhile; make sure
            // the computer this call may have started is stopped again.
            tx.commit().await?;
            if result.is_ok() {
                let _ = self.runtime.stop(&row.runtime_name).await;
            }
            return Ok(if current.1 == "stopped" {
                Acquire::OwnerStopped
            } else {
                Acquire::Queued {
                    reason: "the computer changed state; try again".into(),
                }
            });
        }
        match result {
            Ok(()) => {
                sqlx::query(
                    "UPDATE bot_computers SET observed_state = 'running', last_error = NULL,
                         last_used_at = now(), updated_at = now()
                     WHERE bot_id = $1",
                )
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
                let lease = self.lease(&mut tx, bot_id, holder, next).await?;
                tx.commit().await?;
                Ok(Acquire::Ready {
                    lease_id: lease,
                    runtime_name: row.runtime_name,
                    generation: next,
                })
            }
            Err(err) => {
                // A create that left nothing behind is retried as a create;
                // anything that exists is an error a later acquire retries
                // with a start.
                let exists = matches!(self.runtime.state(&row.runtime_name).await, Ok(Some(_)));
                let state = if wake == Wake::Create && !exists {
                    "absent"
                } else {
                    "error"
                };
                sqlx::query(
                    "UPDATE bot_computers SET observed_state = $2, last_error = left($3, 2000),
                         updated_at = now()
                     WHERE bot_id = $1",
                )
                .bind(bot_id)
                .bind(state)
                .bind(err.to_string())
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(Acquire::Unavailable {
                    reason: err.to_string(),
                })
            }
        }
    }

    /// Whether starting this computer stays within the global budget.
    /// Running, paused, and transitioning computers hold their reservation.
    async fn admits(
        &self,
        tx: &mut sqlx::PgConnection,
        bot_id: Uuid,
        row: &Row,
    ) -> Result<bool, sqlx::Error> {
        // Serialize admission across computers.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext('else_computer_admission'))")
            .execute(&mut *tx)
            .await?;
        let (cpus, memory): (i64, i64) = sqlx::query_as(
            "SELECT coalesce(sum(cpus), 0)::bigint, coalesce(sum(memory_mib + $2), 0)::bigint
             FROM bot_computers
             WHERE bot_id <> $1 AND observed_state IN
                   ('creating', 'starting', 'running', 'pausing', 'paused', 'resuming', 'stopping')",
        )
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
        tx: &mut sqlx::PgConnection,
        bot_id: Uuid,
        holder: &str,
        generation: i64,
    ) -> Result<Uuid, sqlx::Error> {
        let existing: Option<Uuid> = sqlx::query_scalar(
            "UPDATE computer_leases SET expires_at = now() + make_interval(secs => $3)
             WHERE bot_id = $1 AND holder = $2 AND released_at IS NULL RETURNING id",
        )
        .bind(bot_id)
        .bind(holder)
        .bind(self.config.lease_ttl.as_secs_f64())
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(id) = existing {
            return Ok(id);
        }
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
        Ok(renewed == 1)
    }

    /// End a lease. Repeating it is harmless.
    pub async fn release(&self, lease_id: Uuid) -> Result<(), ComputerError> {
        sqlx::query(
            "WITH ended AS (
                 UPDATE computer_leases SET released_at = now(), end_reason = 'released'
                 WHERE id = $1 AND released_at IS NULL RETURNING bot_id)
             UPDATE bot_computers SET last_used_at = now()
             WHERE bot_id IN (SELECT bot_id FROM ended)",
        )
        .bind(lease_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Owner stop: no implicit wake until resumed. Active leases are
    /// revoked and the computer stops; its disk is kept.
    pub async fn owner_stop(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<Row> = sqlx::query_as(
            "SELECT runtime_name, image, cpus, memory_mib, desired_state, observed_state, generation
             FROM bot_computers WHERE bot_id = $1 FOR UPDATE",
        )
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            // No computer yet: record the intent so first use cannot wake it.
            sqlx::query(
                "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib, desired_state)
                 VALUES ($1, $2, $3, $4, $5, 'stopped')",
            )
            .bind(bot_id)
            .bind(Self::runtime_name(bot_id))
            .bind(&self.config.image)
            .bind(self.config.cpus as i32)
            .bind(self.config.memory_mib as i32)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(());
        };
        sqlx::query("UPDATE bot_computers SET desired_state = 'stopped', updated_at = now() WHERE bot_id = $1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE computer_leases SET released_at = now(), end_reason = 'owner_stop'
             WHERE bot_id = $1 AND released_at IS NULL",
        )
        .bind(bot_id)
        .execute(&mut *tx)
        .await?;
        let live = matches!(
            row.observed_state.as_str(),
            "running" | "paused" | "creating" | "starting" | "resuming" | "pausing"
        );
        // The intent and revoked leases commit before the runtime is asked
        // to stop, so nothing can acquire the computer in between.
        tx.commit().await?;
        if !live {
            return Ok(());
        }
        self.stop_now(bot_id, "owner stop").await
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

    /// Stop a computer (owner stop, idle, deletion): decide under the lock,
    /// call the runtime outside it, and record the result if no other
    /// transition happened meanwhile.
    async fn stop_now(&self, bot_id: Uuid, why: &str) -> Result<(), ComputerError> {
        self.transition(bot_id, IdleAction::Stop, why, false)
            .await
            .map(|_| ())
    }

    /// Stop or pause an idle (or must-stop) computer. With `only_if_idle`,
    /// nothing happens while any lease is active. Returns whether it acted.
    async fn transition(
        &self,
        bot_id: Uuid,
        action: IdleAction,
        why: &str,
        only_if_idle: bool,
    ) -> Result<bool, ComputerError> {
        let mut tx = self.pool.begin().await?;
        let row: Option<Row> = sqlx::query_as(
            "SELECT runtime_name, image, cpus, memory_mib, desired_state, observed_state, generation
             FROM bot_computers WHERE bot_id = $1 FOR UPDATE",
        )
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else { return Ok(false) };
        if only_if_idle {
            let leases: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM computer_leases WHERE bot_id = $1 AND released_at IS NULL",
            )
            .bind(bot_id)
            .fetch_one(&mut *tx)
            .await?;
            if leases > 0 {
                return Ok(false);
            }
        }
        let applies = match action {
            IdleAction::Stop => matches!(
                row.observed_state.as_str(),
                "running" | "paused" | "creating" | "starting" | "resuming" | "pausing"
            ),
            IdleAction::Pause => row.observed_state == "running",
        };
        if !applies {
            return Ok(false);
        }
        let next = row.generation + 1;
        let (transient, done) = match action {
            IdleAction::Stop => ("stopping", "stopped"),
            IdleAction::Pause => ("pausing", "paused"),
        };
        sqlx::query(
            "UPDATE bot_computers SET observed_state = $2, generation = $3, updated_at = now()
             WHERE bot_id = $1",
        )
        .bind(bot_id)
        .bind(transient)
        .bind(next)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!(bot = %bot_id, action = ?action, why, "computer transition");
        let result = match action {
            IdleAction::Stop => self.runtime.stop(&row.runtime_name).await,
            IdleAction::Pause => self.runtime.pause(&row.runtime_name).await,
        };
        let (state, error) = match &result {
            Ok(()) => (done, None),
            Err(err) => ("error", Some(err.to_string())),
        };
        sqlx::query(
            "UPDATE bot_computers SET observed_state = $3, last_error = coalesce(left($4, 2000), last_error),
                 updated_at = now()
             WHERE bot_id = $1 AND generation = $2",
        )
        .bind(bot_id)
        .bind(next)
        .bind(state)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(result.is_ok())
    }

    /// One maintenance pass: reconcile expired leases against live work,
    /// stop computers of deleted or owner-stopped bots, and stop or pause
    /// computers idle past the timeout.
    pub async fn tick(&self) -> Result<(), ComputerError> {
        // Expired leases end only if their holder's work is no longer live.
        let expired: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT id, holder FROM computer_leases WHERE released_at IS NULL AND expires_at < now()",
        )
        .fetch_all(&self.pool)
        .await?;
        for (lease, holder) in expired {
            if self.holder_is_live(&holder).await? {
                self.renew(lease).await?;
            } else {
                sqlx::query(
                    "UPDATE computer_leases SET released_at = now(), end_reason = 'expired'
                     WHERE id = $1 AND released_at IS NULL",
                )
                .bind(lease)
                .execute(&self.pool)
                .await?;
            }
        }
        // Deleted bots and owner stops: stop now (disks are kept).
        let must_stop: Vec<Uuid> = sqlx::query_scalar(
            "SELECT c.bot_id FROM bot_computers c JOIN bots b ON b.id = c.bot_id
             WHERE c.observed_state IN ('running', 'paused')
               AND (b.status <> 'active' OR c.desired_state = 'stopped')",
        )
        .fetch_all(&self.pool)
        .await?;
        for bot in must_stop {
            sqlx::query(
                "UPDATE computer_leases SET released_at = now(), end_reason = 'revoked'
                 WHERE bot_id = $1 AND released_at IS NULL",
            )
            .bind(bot)
            .execute(&self.pool)
            .await?;
            self.stop_now(bot, "bot deleted or stopped by the owner")
                .await?;
        }
        // Idle: no active lease since the timeout.
        let idle_states = match self.config.idle_action {
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
            self.transition(bot, self.config.idle_action, "idle", true)
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
            _ => Ok(false),
        }
    }

    /// Startup: record what the runtime reports without changing it. A
    /// recorded computer that is gone becomes `missing`, never recreated;
    /// an interrupted transition takes the runtime's actual state.
    pub async fn reconcile_startup(&self) -> Result<(), ComputerError> {
        let listed = match self.runtime.list().await {
            Ok(listed) => listed,
            Err(err) => {
                tracing::warn!(error = %err, "computer runtime unavailable at startup; states unchanged");
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
        Ok(())
    }

    /// Maintenance loop until cancelled.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            if let Err(err) = self.tick().await {
                tracing::warn!(error = %err, "computer maintenance failed");
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }
}
