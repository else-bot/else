use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::actor::Actor;

use sqlx::SqlitePool;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use crate::app::providers::{FailureCode, Provider, Stop, TurnInput, TurnOutcome};
use crate::db::turn_sink::{FLUSH_INTERVAL, SqliteSink};
use crate::db::turns;
use crate::grants::{Grants, Tool, policy};
use crate::providers::Registry;

pub(crate) mod pump;

const TOOL_GRANT_TTL: Duration = Duration::from_secs(40 * 60);

const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const FINISH_ATTEMPTS: u32 = 20;

pub struct Scheduler {
    pool: SqlitePool,
    registry: Registry,
    claim: Notify,
    pump: Notify,
    running: Actor<HashMap<Uuid, Stop>>,
    cancel_grace: Duration,
    tasks: TaskTracker,
    grants: Arc<Grants>,
    slots: Arc<Semaphore>,
    /// Background work (routines, connector events, summaries) also takes
    /// one of these, so a slot stays free for the owner when the cap is
    /// above 1.
    background: Arc<Semaphore>,
    summaries: AtomicUsize,
    gate: Arc<crate::maintenance::Gate>,
    mcp_root: std::path::PathBuf,
    computers: Option<Arc<crate::computers::ComputerManager>>,
}

/// What the scheduler runs turns with, built before it.
pub struct Services {
    pub pool: SqlitePool,
    pub registry: Registry,
    pub gate: Arc<crate::maintenance::Gate>,
    pub grants: Arc<Grants>,
    /// The shared MCP library's directory, so turns can list its packages.
    pub mcp_root: std::path::PathBuf,
    /// Where routine and watcher checks run, when bot computers started.
    pub computers: Option<Arc<crate::computers::ComputerManager>>,
}

/// A provider slot held by one memory summary. Released on drop.
pub struct SummaryPermit {
    scheduler: Arc<Scheduler>,
    slot: Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)>,
    _admission: crate::maintenance::Admission,
}

impl Drop for SummaryPermit {
    fn drop(&mut self) {
        self.scheduler.summaries.fetch_sub(1, Ordering::AcqRel);
        drop(self.slot.take());
        self.scheduler.wake();
    }
}

/// How long an interrupted provider may take to stop before it must stop at
/// once.
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(10);

impl Scheduler {
    pub fn new(services: Services, max_active: usize, cancel_grace: Duration) -> Arc<Self> {
        let Services {
            pool,
            registry,
            gate,
            grants,
            mcp_root,
            computers,
        } = services;
        Arc::new(Self {
            pool,
            registry,
            claim: Notify::new(),
            pump: Notify::new(),
            running: Actor::default(),
            cancel_grace,
            grants,
            tasks: TaskTracker::new(),
            slots: Arc::new(Semaphore::new(max_active)),
            background: Arc::new(Semaphore::new(max_active.saturating_sub(1).max(1))),
            summaries: AtomicUsize::new(0),
            gate,
            mcp_root,
            computers,
        })
    }

    /// Turns this process is running now.
    pub async fn running_count(&self) -> usize {
        self.running.call(|running| running.len()).await
    }

    /// The gate turns are admitted through, which a backup closes before
    /// it drains them.
    pub(crate) fn gate(&self) -> &Arc<crate::maintenance::Gate> {
        &self.gate
    }

    /// Wait until no turn runs in this process or remains active in the
    /// database. False on timeout (the caller defers its work).
    pub async fn drain(&self, timeout: Duration) -> Result<bool, sqlx::Error> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let local = self.running.call(|running| running.is_empty()).await;
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

    /// Take a provider slot for one memory summary if one is free. Summaries
    /// are low priority: the caller checks first that no turn is waiting,
    /// and at most `limit` run at once.
    pub fn try_begin_summary(self: &Arc<Self>, limit: usize) -> Option<SummaryPermit> {
        let admission = self.gate.enter()?;
        let background = self.background.clone().try_acquire_owned().ok()?;
        let slot = self.slots.clone().try_acquire_owned().ok()?;
        self.summaries
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < limit).then_some(n + 1)
            })
            .ok()?;
        Some(SummaryPermit {
            scheduler: self.clone(),
            slot: Some((slot, background)),
            _admission: admission,
        })
    }

    /// Hint that new work may be eligible. Rows remain authoritative.
    pub fn wake(&self) {
        self.claim.notify_one();
        self.pump.notify_one();
    }

    /// Request interruption of a turn running in this process.
    pub async fn interrupt(&self, turn_id: Uuid) -> bool {
        self.running
            .call(move |running| match running.get(&turn_id) {
                Some(stop) => {
                    stop.request();
                    true
                }
                None => false,
            })
            .await
    }

    /// Wake a turn running in this process to take the owner messages
    /// steered into it. False when it does not run here; its end then
    /// releases them.
    pub async fn steer(&self, turn_id: Uuid) -> bool {
        self.running
            .call(move |running| match running.get(&turn_id) {
                Some(stop) => {
                    stop.steer();
                    true
                }
                None => false,
            })
            .await
    }

    pub async fn cancellation(&self, turn_id: Uuid) -> Option<CancellationToken> {
        self.running
            .call(move |running| running.get(&turn_id).map(|stop| stop.token().clone()))
            .await
    }

    pub async fn is_running(&self, turn_id: Uuid) -> bool {
        self.running
            .call(move |running| running.contains_key(&turn_id))
            .await
    }

    /// Run until `shutdown` fires, then interrupt running turns and wait up
    /// to `grace` for them to record outcomes. Claiming and the feature
    /// pumps run in separate loops, so a slow pump never delays a claim.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken, grace: Duration) {
        tokio::join!(self.claim_loop(&shutdown), self.pump_loop(&shutdown));
        self.running
            .call(|running| {
                for stop in running.values() {
                    stop.request();
                }
            })
            .await;
        self.tasks.close();
        if tokio::time::timeout(grace, self.tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!(
                "turns or check launches still running at shutdown; restart will reconcile them"
            );
        }
    }

    async fn claim_loop(self: &Arc<Self>, shutdown: &CancellationToken) {
        loop {
            if let Err(err) = self.dispatch().await {
                tracing::warn!(error = %err, "scheduler scan failed");
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = self.claim.notified() => {}
                _ = tokio::time::sleep(SCAN_INTERVAL) => {}
            }
        }
    }

    /// Advance the features' state machines once per tick, then wake the
    /// claim loop for the turns they queued.
    async fn pump_loop(&self, shutdown: &CancellationToken) {
        loop {
            if let Some(admission) = self.gate.enter() {
                let launch = |check| self.launch_check(&admission, check);
                pump::all(&self.tick(&launch)).await;
                self.claim.notify_one();
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = self.pump.notified() => {}
                _ = tokio::time::sleep(SCAN_INTERVAL) => {}
            }
        }
    }

    async fn dispatch(self: &Arc<Self>) -> Result<(), sqlx::Error> {
        for turn_id in turns::cancelling(&self.pool).await? {
            self.interrupt(turn_id).await;
        }
        let mut live: Vec<Uuid> = self
            .running
            .call(|running| running.keys().copied().collect())
            .await;
        let orphaned = turns::lifecycle::settle_orphans(&self.pool, &live).await?;
        if orphaned > 0 {
            tracing::warn!(orphaned, "settled turns without a running provider task");
        }
        if let Some(admission) = self.gate.enter() {
            let launch = |check| self.launch_check(&admission, check);
            match turns::settle_deferred(&self.pool).await {
                Ok((0, _)) => {}
                Ok((_, woken)) => {
                    for into in woken {
                        self.steer(into).await;
                    }
                    self.wake();
                }
                Err(err) => tracing::warn!(error = %err, "settling deferred messages failed"),
            }
            pump::before_claim(&self.tick(&launch)).await;
        }
        loop {
            let Some(_admission) = self.gate.enter() else {
                break;
            };
            let Ok(slot) = self.slots.clone().try_acquire_owned() else {
                break;
            };
            let background = self.background.clone().try_acquire_owned().ok();
            let Some(claimed) = turns::claim_next(&self.pool, &live, background.is_some()).await?
            else {
                break;
            };
            live.push(claimed.id);
            let (turn_id, background) = (claimed.id, background.filter(|_| claimed.background));
            let stop = Stop::new(CancellationToken::new());
            let this = self.clone();
            self.running
                .call(move |running| {
                    running.insert(turn_id, stop.clone());
                    let tasks = this.tasks.clone();
                    tasks.spawn(async move {
                        this.execute(turn_id, stop).await;
                        this.grants.revoke_turn(turn_id).await;
                        this.running
                            .call(move |running| running.remove(&turn_id))
                            .await;
                        drop((slot, background));
                        this.wake();
                    });
                })
                .await;
        }
        Ok(())
    }

    fn tick<'a>(&'a self, launch: &'a (dyn Fn(pump::Check) + Send + Sync)) -> pump::Tick<'a> {
        pump::Tick {
            pool: &self.pool,
            registry: &self.registry,
            computers: self.computers.as_deref(),
            now: chrono::Utc::now(),
            launch,
        }
    }

    /// Launch a reserved check under a share of the pass's admission, so a
    /// backup waits until the check holds its computer.
    fn launch_check(&self, admission: &crate::maintenance::Admission, check: pump::Check) {
        // Checks are reserved only when computers exist.
        let Some(computers) = self.computers.clone() else {
            return;
        };
        let admission = admission.share();
        self.tasks.spawn(async move {
            let _admission = admission;
            if let Err(err) = computers
                .launch_check(check.bot_id, check.job_id, &check.command, &check.secrets)
                .await
            {
                tracing::warn!(job = %check.job_id, error = %err, "launching a routine or watcher check failed");
            }
        });
    }

    async fn execute(&self, turn_id: Uuid, stop: Stop) {
        let (sink, provider_id, outcome) = match self.prepare(turn_id, stop.token()).await {
            Ok(Prepared {
                input,
                provider_id,
                provider,
            }) => {
                let project = input
                    .delegation
                    .host_tools()
                    .then(|| provider.project_dir(input.bot_id))
                    .flatten();
                let sink = SqliteSink::new(self.pool.clone(), &input, &provider_id, project);
                let outcome = self.run_provider(provider, input, &sink, stop).await;
                (sink, provider_id, outcome)
            }
            Err(PrepareError::ProviderMissing { input, provider_id }) => {
                let sink = SqliteSink::new(self.pool.clone(), &input, &provider_id, None);
                let outcome = TurnOutcome::Failed {
                    code: FailureCode::ProviderUnavailable,
                    message: "The provider for this message is no longer configured.".into(),
                };
                (sink, provider_id, outcome)
            }
            Err(PrepareError::Load(err)) => {
                tracing::error!(turn = %turn_id, error = %err, "loading turn failed");
                let message = "The message could not be prepared. Send it again.";
                if let Err(err) = with_backoff(turn_id, "failing an unprepared turn", || {
                    turns::fail_unstarted(&self.pool, turn_id, message)
                })
                .await
                {
                    tracing::error!(
                        turn = %turn_id, error = %err,
                        "failing an unprepared turn failed; startup reconciliation will settle it"
                    );
                }
                return;
            }
        };
        tracing::info!(turn = %turn_id, provider = %provider_id, ?outcome, "turn finished");
        if let Err(err) =
            with_backoff(turn_id, "recording turn outcome", || sink.finish(&outcome)).await
        {
            tracing::error!(
                turn = %turn_id, error = %err,
                "recording turn outcome failed; startup reconciliation will settle it"
            );
        }
    }

    /// Load the turn, then issue its task grant and fill in what its grant
    /// lets it see. A missing provider fails before any grant is issued.
    async fn prepare(
        &self,
        turn_id: Uuid,
        cancel: &CancellationToken,
    ) -> Result<Prepared, PrepareError> {
        let mut input = turns::input(&self.pool, turn_id).await?;
        let provider_id: String = sqlx::query_scalar("SELECT provider_id FROM turns WHERE id = ?1")
            .bind(turn_id)
            .fetch_one(&self.pool)
            .await?;
        let Some(provider) = self.registry.get(&provider_id).cloned() else {
            return Err(PrepareError::ProviderMissing {
                input: Box::new(input),
                provider_id,
            });
        };
        let access = policy::for_turn(&input.delegation);
        let memory_scope =
            policy::memory_scope(&self.pool, turn_id, input.bot_id, access.memory).await;
        let lists_mcp = access.tools.contains(&Tool::Mcp);
        input.tool_grant = Some(
            self.grants
                .issue_scoped(
                    input.bot_id,
                    input.conversation_id,
                    turn_id,
                    access.tools,
                    memory_scope,
                    TOOL_GRANT_TTL,
                )
                .await,
        );
        if lists_mcp {
            let (mcps, skills) = tokio::join!(
                crate::mcp::installed(self.mcp_root.clone()),
                crate::db::skills::visible(&self.pool, input.bot_id),
            );
            match mcps {
                Ok(summaries) => {
                    input.mcps = Some(
                        summaries
                            .into_iter()
                            .map(|s| crate::app::providers::InstalledMcp {
                                id: s.id.into(),
                                name: s.name,
                                description: s.description,
                            })
                            .collect(),
                    );
                }
                Err(err) => {
                    tracing::warn!(turn = %turn_id, error = %err, "listing shared MCPs failed");
                }
            }
            match skills {
                Ok(skills) => input.skills = Some(skills),
                Err(err) => {
                    tracing::warn!(turn = %turn_id, error = %err, "listing skills failed");
                }
            }
        }
        match crate::handoff::prepare(&self.pool, &self.registry, &input, &provider_id, cancel)
            .await
        {
            Ok(handoff) => input.handoff = handoff,
            Err(err) => {
                tracing::warn!(turn = %turn_id, error = %err, "preparing a handoff failed");
            }
        }
        Ok(Prepared {
            input,
            provider_id,
            provider,
        })
    }

    /// Run the provider, flushing its output as it streams, and escalate a
    /// stop it does not honor within the cancel grace.
    async fn run_provider(
        &self,
        provider: Arc<dyn Provider>,
        input: TurnInput,
        sink: &SqliteSink,
        stop: Stop,
    ) -> TurnOutcome {
        let turn_id = input.turn_id;
        let cancel = stop.token().clone();
        let done = CancellationToken::new();
        let run = async {
            let escalation = async {
                cancel.cancelled().await;
                tokio::time::sleep(self.cancel_grace).await;
            };
            let attempt = provider.run(input, sink, stop.clone());
            tokio::pin!(attempt);
            let outcome = tokio::select! {
                outcome = &mut attempt => outcome,
                () = escalation => {
                    tracing::warn!(turn = %turn_id, "provider did not stop in time; escalating");
                    stop.escalate();
                    attempt.await
                }
            };
            done.cancel();
            outcome
        };
        let flusher = async {
            loop {
                tokio::select! {
                    _ = done.cancelled() => break,
                    _ = tokio::time::sleep(FLUSH_INTERVAL) => {}
                    _ = sink.flush_wanted() => {}
                }
                if sink.flush().await.is_err() {
                    cancel.cancel();
                }
            }
        };
        tokio::join!(run, flusher).0
    }
}

/// A turn ready to run: loaded, granted, and its provider found.
struct Prepared {
    input: TurnInput,
    provider_id: String,
    provider: Arc<dyn Provider>,
}

enum PrepareError {
    /// The turn could not be loaded; it fails unstarted.
    Load(sqlx::Error),
    /// Its provider is no longer configured; it fails as unavailable.
    ProviderMissing {
        input: Box<TurnInput>,
        provider_id: String,
    },
}

impl From<sqlx::Error> for PrepareError {
    fn from(err: sqlx::Error) -> Self {
        Self::Load(err)
    }
}

/// Run `op` up to [`FINISH_ATTEMPTS`] times, backing off from 100 ms to 5 s
/// between tries, and return its last error.
async fn with_backoff<E: std::fmt::Display, F: Future<Output = Result<(), E>>>(
    turn_id: Uuid,
    what: &str,
    mut op: impl FnMut() -> F,
) -> Result<(), E> {
    let mut delay = Duration::from_millis(100);
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(()) => return Ok(()),
            Err(err) if attempt < FINISH_ATTEMPTS => {
                tracing::warn!(turn = %turn_id, attempt, error = %err, "{what} failed; retrying");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(5));
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}
