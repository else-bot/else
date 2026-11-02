//! The single in-process scheduler. PostgreSQL rows are the queue; this
//! loop claims eligible turns, runs them through their adapters, and records
//! outcomes. Handlers only write requests and call [`Scheduler::wake`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::app::providers::TurnOutcome;
use crate::db::turns::{self, FLUSH_INTERVAL, PgSink};
use crate::grants::{Grants, Tool};

/// Longer than a provider turn's own deadline.
const TOOL_GRANT_TTL: Duration = Duration::from_secs(40 * 60);
use crate::providers::Registry;

const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const FINISH_ATTEMPTS: u32 = 20;

pub struct Scheduler {
    pool: PgPool,
    registry: Registry,
    wake: Notify,
    /// Cancellation handles of turns running in this process, by turn ID.
    running: Mutex<HashMap<Uuid, CancellationToken>>,
    max_active: usize,
    cancel_grace: Duration,
    tasks: TaskTracker,
    grants: Arc<Grants>,
    /// Provider slots in use: turns (reserved before their claim) and
    /// memory summaries share one count, so together they never exceed
    /// `max_active`.
    slots: Mutex<Slots>,
    /// Closed while a coordinated backup runs: no turn is claimed.
    gate: Arc<crate::maintenance::Gate>,
}

#[derive(Default)]
struct Slots {
    used: usize,
    summaries: usize,
}

/// A provider slot held by one memory summary. Released on drop.
pub struct SummaryPermit {
    scheduler: Arc<Scheduler>,
    /// A summary is admitted work: a backup waits for it.
    _admission: crate::maintenance::Admission,
}

impl Drop for SummaryPermit {
    fn drop(&mut self) {
        {
            let mut slots = self.scheduler.slots();
            slots.used -= 1;
            slots.summaries -= 1;
        }
        self.scheduler.wake();
    }
}

/// How long an interrupted provider may take to stop before it is abandoned.
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(10);

impl Scheduler {
    pub fn new(pool: PgPool, registry: Registry, max_active: usize) -> Arc<Self> {
        Self::with_cancel_grace(pool, registry, max_active, DEFAULT_CANCEL_GRACE)
    }

    pub fn with_cancel_grace(
        pool: PgPool,
        registry: Registry,
        max_active: usize,
        cancel_grace: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            registry,
            wake: Notify::new(),
            running: Mutex::new(HashMap::new()),
            max_active,
            cancel_grace,
            grants: Arc::new(Grants::default()),
            tasks: TaskTracker::new(),
            slots: Mutex::new(Slots::default()),
            gate: Arc::default(),
        })
    }

    /// The maintenance gate shared with the HTTP layer.
    pub fn gate(&self) -> &Arc<crate::maintenance::Gate> {
        &self.gate
    }

    /// Wait until no turn runs in this process or remains active in the
    /// database. False on timeout (the caller defers its work).
    pub async fn drain(&self, timeout: Duration) -> Result<bool, sqlx::Error> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let local = self.running().is_empty();
            let active: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT EXISTS (SELECT 1 FROM turns WHERE status IN {})",
                crate::db::turns::ACTIVE_STATUSES
            )))
            .fetch_one(&self.pool)
            .await?;
            if local && !active {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Task grants issued to running turns. Revoked when a turn ends.
    pub fn grants(&self) -> &Arc<Grants> {
        &self.grants
    }

    /// Take a provider slot for one memory summary if one is free. Summaries
    /// are low priority: the caller checks first that no turn is waiting,
    /// and at most `limit` run at once.
    pub fn try_begin_summary(self: &Arc<Self>, limit: usize) -> Option<SummaryPermit> {
        let admission = self.gate.enter()?;
        let mut slots = self.slots();
        if slots.summaries >= limit || slots.used >= self.max_active {
            return None;
        }
        slots.used += 1;
        slots.summaries += 1;
        Some(SummaryPermit {
            scheduler: self.clone(),
            _admission: admission,
        })
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Reserve a slot for a turn before claiming it from the database.
    fn reserve_turn(&self) -> bool {
        let mut slots = self.slots();
        if slots.used >= self.max_active {
            return false;
        }
        slots.used += 1;
        true
    }

    fn release_turn(&self) {
        self.slots().used -= 1;
    }

    /// Hint that new work may be eligible. Rows remain authoritative.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    fn running(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, CancellationToken>> {
        self.running.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Request interruption of a turn running in this process.
    pub fn interrupt(&self, turn_id: Uuid) -> bool {
        match self.running().get(&turn_id) {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    pub fn is_running(&self, turn_id: Uuid) -> bool {
        self.running().contains_key(&turn_id)
    }

    /// Run until `shutdown` fires, then interrupt running turns and wait up
    /// to `grace` for them to record outcomes.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken, grace: Duration) {
        loop {
            if let Err(err) = self.dispatch().await {
                tracing::warn!(error = %err, "scheduler scan failed");
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(SCAN_INTERVAL) => {}
            }
        }
        for token in self.running().values() {
            token.cancel();
        }
        self.tasks.close();
        if tokio::time::timeout(grace, self.tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!("turns still running at shutdown; restart will reconcile them");
        }
    }

    async fn dispatch(self: &Arc<Self>) -> Result<(), sqlx::Error> {
        // Redeliver interruption: a stop or deletion committed even if its
        // request never reached `interrupt` (for example, a lost response).
        for turn_id in turns::cancelling(&self.pool).await? {
            self.interrupt(turn_id);
        }
        // Claims below insert into `running` before the next scan, so every
        // turn missing from it here has no provider task.
        let live: Vec<Uuid> = self.running().keys().copied().collect();
        let orphaned = turns::settle_orphans(&self.pool, &live).await?;
        if orphaned > 0 {
            tracing::warn!(orphaned, "settled turns without a running provider task");
        }
        // The slot is reserved before the claim, so a summary cannot take
        // it while the claim is in flight. Each claim is admitted through
        // the maintenance gate until its turn is registered as running, so
        // a backup either sees it in the drain or it is never claimed.
        loop {
            let Some(_admission) = self.gate.enter() else {
                break;
            };
            if !self.reserve_turn() {
                break;
            }
            let claimed = turns::claim_next(&self.pool, &[]).await;
            let turn_id = match claimed {
                Ok(Some(turn_id)) => turn_id,
                Ok(None) => {
                    self.release_turn();
                    break;
                }
                Err(err) => {
                    self.release_turn();
                    return Err(err);
                }
            };
            // Start each claim before the next query, so an error in a later
            // claim cannot strand this one.
            let token = CancellationToken::new();
            self.running().insert(turn_id, token.clone());
            let this = self.clone();
            self.tasks.spawn(async move {
                this.execute(turn_id, token).await;
                this.grants.revoke_turn(turn_id);
                this.running().remove(&turn_id);
                this.release_turn();
                this.wake();
            });
        }
        Ok(())
    }

    async fn execute(&self, turn_id: Uuid, cancel: CancellationToken) {
        let loaded = async {
            let input = turns::input(&self.pool, turn_id).await?;
            let provider_id: String =
                sqlx::query_scalar("SELECT provider_id FROM turns WHERE id = $1")
                    .bind(turn_id)
                    .fetch_one(&self.pool)
                    .await?;
            Ok::<_, sqlx::Error>((input, provider_id))
        }
        .await;
        let (mut input, provider_id) = match loaded {
            Ok(loaded) => loaded,
            Err(err) => {
                tracing::error!(turn = %turn_id, error = %err, "loading turn failed");
                // Nothing was sent, so ending the turn is safe. Retry so a
                // transient error cannot leave the bot's slot occupied.
                let mut delay = Duration::from_millis(100);
                for _ in 0..FINISH_ATTEMPTS {
                    let message = "The message could not be prepared. Send it again.";
                    if turns::fail_unstarted(&self.pool, turn_id, message)
                        .await
                        .is_ok()
                    {
                        return;
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
                return;
            }
        };
        // A delegated task reads its own bot's memory, and other bots only
        // where both the delegating bot and its own bot may: delegation
        // never widens what the original request could reach.
        let memory_scope = match input.delegation.parent_bot_id {
            Some(parent) if input.delegation.delegated_task => {
                match delegated_scope(&self.pool, parent, input.bot_id).await {
                    Ok(scope) => Some(scope),
                    Err(err) => {
                        tracing::warn!(error = %err, "loading a delegated memory scope failed");
                        Some(vec![input.bot_id])
                    }
                }
            }
            _ if input.delegation.delegated_task => Some(vec![input.bot_id]),
            _ => None,
        };
        // Tools for this turn only; revoked when it ends (see dispatch).
        input.tool_grant = Some(self.grants.issue_scoped(
            input.bot_id,
            input.conversation_id,
            turn_id,
            if input.delegation.can_delegate {
                vec![
                    Tool::ReadHistory,
                    Tool::ReadMemory,
                    Tool::Computer,
                    Tool::Delegate,
                ]
            } else {
                vec![Tool::ReadHistory, Tool::ReadMemory, Tool::Computer]
            },
            memory_scope,
            TOOL_GRANT_TTL,
        ));
        // A provider switch: freeze settled history and prepare the
        // checkpoint before anything is sent.
        if self.registry.get(&provider_id).is_some() {
            match crate::handoff::prepare(&self.pool, &self.registry, &input, &provider_id, &cancel)
                .await
            {
                Ok(handoff) => input.handoff = handoff,
                Err(err) => {
                    // Without a durable record nothing is handed off; the
                    // turn still gets the recorded history it missed.
                    tracing::warn!(turn = %turn_id, error = %err, "preparing a handoff failed");
                }
            }
        }
        let sink = PgSink::new(self.pool.clone(), &input, &provider_id);
        let outcome = match self.registry.get(&provider_id) {
            None => TurnOutcome::Failed {
                code: "provider_unavailable".into(),
                message: "The provider for this message is no longer configured.".into(),
            },
            Some(provider) => {
                let provider = provider.clone();
                let done = CancellationToken::new();
                // The adapter only buffers text; it never writes it. That
                // lets the watchdog abandon an adapter that ignores
                // interruption without cutting a database commit short.
                let run = async {
                    let watchdog = async {
                        cancel.cancelled().await;
                        tokio::time::sleep(self.cancel_grace).await;
                    };
                    let outcome = tokio::select! {
                        outcome = provider.run(input, &sink, cancel.clone()) => outcome,
                        () = watchdog => TurnOutcome::Uncertain {
                            message: format!(
                                "The provider did not stop within {} seconds after being \
                                 interrupted; its effects are unknown.",
                                self.cancel_grace.as_secs()
                            ),
                        },
                    };
                    done.cancel();
                    outcome
                };
                // The flusher owns every text write and always finishes the
                // flush it started; it stops only between flushes.
                let flusher = async {
                    loop {
                        tokio::select! {
                            _ = done.cancelled() => break,
                            _ = tokio::time::sleep(FLUSH_INTERVAL) => {}
                            _ = sink.flush_wanted() => {}
                        }
                        if sink.flush().await.is_err() {
                            // Fenced: the turn was stopped or the bot deleted.
                            cancel.cancel();
                        }
                    }
                };
                tokio::join!(run, flusher).0
            }
        };
        tracing::info!(turn = %turn_id, provider = %provider_id, ?outcome, "turn finished");
        // The terminal outcome must not be lost to a transient database
        // error; otherwise the bot's slot would stay occupied until restart.
        let mut delay = Duration::from_millis(100);
        for attempt in 1..=FINISH_ATTEMPTS {
            match sink.finish(&outcome).await {
                Ok(()) => break,
                Err(err) if attempt < FINISH_ATTEMPTS => {
                    tracing::warn!(turn = %turn_id, attempt, error = %err, "recording turn outcome failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(5));
                }
                Err(err) => tracing::error!(
                    turn = %turn_id, error = %err,
                    "recording turn outcome failed; startup reconciliation will settle it"
                ),
            }
        }
    }
}

/// The bots a delegated task may read: its own, plus those both the
/// delegating bot and its own bot may read.
pub async fn delegated_scope(
    pool: &sqlx::PgPool,
    parent: Uuid,
    target: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let parent_reads: Vec<Uuid> = crate::db::memory::readable_bots(pool, parent)
        .await?
        .into_iter()
        .map(|(id, _, _)| id)
        .collect();
    let mut scope = vec![target];
    for (id, _, _) in crate::db::memory::readable_bots(pool, target).await? {
        if id != target && parent_reads.contains(&id) {
            scope.push(id);
        }
    }
    Ok(scope)
}
