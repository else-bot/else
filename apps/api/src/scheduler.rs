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

use crate::app::providers::{Stop, TurnOutcome};
use crate::db::turn_sink::{FLUSH_INTERVAL, SqliteSink};
use crate::db::turns;
use crate::grants::{Grants, Tool};

const TOOL_GRANT_TTL: Duration = Duration::from_secs(40 * 60);
use crate::providers::Registry;

const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const FINISH_ATTEMPTS: u32 = 20;

pub struct Scheduler {
    pool: SqlitePool,
    registry: Registry,
    wake: Notify,
    running: Actor<HashMap<Uuid, CancellationToken>>,
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
    /// The shared MCP library's directory, set by the router.
    mcp_root: std::sync::OnceLock<std::path::PathBuf>,
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
    pub fn new(pool: SqlitePool, registry: Registry, max_active: usize) -> Arc<Self> {
        Self::with_cancel_grace(pool, registry, max_active, DEFAULT_CANCEL_GRACE)
    }

    pub fn with_cancel_grace(
        pool: SqlitePool,
        registry: Registry,
        max_active: usize,
        cancel_grace: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            pool,
            registry,
            wake: Notify::new(),
            running: Actor::default(),
            cancel_grace,
            grants: Arc::new(Grants::default()),
            tasks: TaskTracker::new(),
            slots: Arc::new(Semaphore::new(max_active)),
            background: Arc::new(Semaphore::new(max_active.saturating_sub(1).max(1))),
            summaries: AtomicUsize::new(0),
            gate: Arc::default(),
            mcp_root: std::sync::OnceLock::new(),
        })
    }

    /// Where the shared MCP library lives, so turns can list its packages.
    pub fn use_mcp_library(&self, root: std::path::PathBuf) {
        let _ = self.mcp_root.set(root);
    }

    /// Turns this process is running now.
    pub async fn running_count(&self) -> usize {
        self.running.call(|running| running.len()).await
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
        self.wake.notify_one();
    }

    /// Request interruption of a turn running in this process.
    pub async fn interrupt(&self, turn_id: Uuid) -> bool {
        self.running
            .call(move |running| match running.get(&turn_id) {
                Some(token) => {
                    token.cancel();
                    true
                }
                None => false,
            })
            .await
    }

    pub async fn cancellation(&self, turn_id: Uuid) -> Option<CancellationToken> {
        self.running
            .call(move |running| running.get(&turn_id).cloned())
            .await
    }

    pub async fn is_running(&self, turn_id: Uuid) -> bool {
        self.running
            .call(move |running| running.contains_key(&turn_id))
            .await
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
        self.running
            .call(|running| {
                for token in running.values() {
                    token.cancel();
                }
            })
            .await;
        self.tasks.close();
        if tokio::time::timeout(grace, self.tasks.wait())
            .await
            .is_err()
        {
            tracing::warn!("turns still running at shutdown; restart will reconcile them");
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
        let orphaned = turns::settle_orphans(&self.pool, &live).await?;
        if orphaned > 0 {
            tracing::warn!(orphaned, "settled turns without a running provider task");
        }
        if let Some(_admission) = self.gate.enter() {
            let now = chrono::Utc::now();
            if let Err(err) = crate::db::routines::review_ended(
                &self.pool,
                &self.registry,
                now,
                crate::db::routines::REVIEW_BATCH,
            )
            .await
            {
                tracing::warn!(error = %err, "reviewing ended routine runs failed");
            }
            let (admitted, woken, mail, social) = tokio::join!(
                crate::db::routines::admit_due(
                    &self.pool,
                    &self.registry,
                    now,
                    crate::db::routines::ADMIT_BATCH,
                ),
                crate::db::webhooks::admit_requested(
                    &self.pool,
                    &self.registry,
                    now,
                    crate::db::webhooks::ADMIT_BATCH,
                ),
                crate::connectors::delivery::admit_pending(&self.pool, &self.registry, 32),
                crate::socials::delivery::admit_pending(&self.pool, &self.registry, 32)
            );
            if let Err(err) = admitted {
                tracing::warn!(error = %err, "admitting due routines failed");
            }
            if let Err(err) = woken {
                tracing::warn!(error = %err, "admitting webhook wakes failed");
            }
            if let Err(err) = social {
                tracing::warn!(error=%err,"admitting social deliveries failed");
            }
            if let Err(err) = mail {
                tracing::warn!(error = %err, "admitting email deliveries failed");
            }
        }
        loop {
            let Some(_admission) = self.gate.enter() else {
                break;
            };
            let Ok(slot) = self.slots.clone().try_acquire_owned() else {
                break;
            };
            let background = self.background.clone().try_acquire_owned().ok();
            let claimed = turns::claim_next(&self.pool, &live, background.is_some()).await;
            let (turn_id, background) = match claimed {
                Ok(Some(claimed)) => {
                    live.push(claimed.id);
                    (claimed.id, background.filter(|_| claimed.background))
                }
                Ok(None) => {
                    break;
                }
                Err(err) => {
                    return Err(err);
                }
            };
            let token = CancellationToken::new();
            let this = self.clone();
            self.running
                .call(move |running| {
                    running.insert(turn_id, token.clone());
                    let tasks = this.tasks.clone();
                    tasks.spawn(async move {
                        this.execute(turn_id, token).await;
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

    async fn execute(&self, turn_id: Uuid, cancel: CancellationToken) {
        let loaded = async {
            let input = turns::input(&self.pool, turn_id).await?;
            let provider_id: String =
                sqlx::query_scalar("SELECT provider_id FROM turns WHERE id = ?1")
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
        let memory_scope = match input.delegation.delegated_by {
            Some((parent, _)) if input.delegation.delegated_task => {
                match delegated_scope(&self.pool, parent, input.bot_id).await {
                    Ok(scope) => Some(scope),
                    Err(err) => {
                        tracing::warn!(error = %err, "loading a delegated memory scope failed");
                        Some(vec![input.bot_id])
                    }
                }
            }
            _ if input.delegation.delegated_task
                || matches!(
                    input.delegation.origin,
                    crate::app::providers::Origin::ConnectorEvent
                        | crate::app::providers::Origin::SocialEvent
                ) =>
            {
                Some(vec![input.bot_id])
            }
            _ => None,
        };
        input.tool_grant = Some(
            self.grants
                .issue_scoped(
                    input.bot_id,
                    input.conversation_id,
                    turn_id,
                    turn_tools(&input.delegation),
                    memory_scope,
                    TOOL_GRANT_TTL,
                )
                .await,
        );
        if let Some(root) = self.mcp_root.get()
            && turn_tools(&input.delegation).contains(&Tool::Mcp)
        {
            match crate::mcp::installed(root.clone()).await {
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
        }
        if turn_tools(&input.delegation).contains(&Tool::Mcp) {
            match crate::db::skills::visible(&self.pool, input.bot_id).await {
                Ok(skills) => input.skills = Some(skills),
                Err(err) => {
                    tracing::warn!(turn = %turn_id, error = %err, "listing skills failed");
                }
            }
        }
        if self.registry.get(&provider_id).is_some() {
            match crate::handoff::prepare(&self.pool, &self.registry, &input, &provider_id, &cancel)
                .await
            {
                Ok(handoff) => input.handoff = handoff,
                Err(err) => {
                    tracing::warn!(turn = %turn_id, error = %err, "preparing a handoff failed");
                }
            }
        }
        let project = input
            .delegation
            .host_tools()
            .then(|| self.registry.get(&provider_id)?.project_dir(input.bot_id))
            .flatten();
        let sink = SqliteSink::new(self.pool.clone(), &input, &provider_id, project);
        let outcome = match self.registry.get(&provider_id) {
            None => TurnOutcome::Failed {
                code: "provider_unavailable".into(),
                message: "The provider for this message is no longer configured.".into(),
            },
            Some(provider) => {
                let provider = provider.clone();
                let stop = Stop::new(cancel.clone());
                let done = CancellationToken::new();
                let run = async {
                    let escalation = async {
                        cancel.cancelled().await;
                        tokio::time::sleep(self.cancel_grace).await;
                    };
                    let attempt = provider.run(input, &sink, stop.clone());
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
        };
        tracing::info!(turn = %turn_id, provider = %provider_id, ?outcome, "turn finished");
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

fn turn_tools(delegation: &crate::app::providers::DelegationContext) -> Vec<Tool> {
    if delegation.origin == crate::app::providers::Origin::SocialEvent {
        return vec![Tool::ReadHistory, Tool::ReadMemory, Tool::Social];
    }
    if delegation.origin == crate::app::providers::Origin::ConnectorEvent {
        return vec![
            Tool::ReadHistory,
            Tool::ReadMemory,
            Tool::Mail,
            Tool::DraftEmail,
        ];
    }
    let mut tools = vec![
        Tool::ReadHistory,
        Tool::ReadMemory,
        Tool::Computer,
        Tool::Mcp,
        Tool::AskPermission,
        Tool::Diagnose,
    ];
    if delegation.acts_for_owner() {
        tools.extend([
            Tool::Mail,
            Tool::Social,
            Tool::RequestSecret,
            Tool::AskOwner,
            Tool::DraftEmail,
        ]);
    }
    if delegation.can_delegate() {
        tools.push(Tool::Delegate);
    }
    if delegation.can_create_bot {
        tools.push(Tool::CreateBot);
    }
    if delegation.origin == crate::app::providers::Origin::OwnerRequest {
        tools.push(Tool::EditBot);
    }
    if delegation.can_manage_routines() {
        tools.extend([Tool::Routines, Tool::ManageMcp]);
    }
    if delegation.can_edit_rules() {
        tools.push(Tool::Rules);
    }
    tools
}

/// The bots a delegated task may read: its own, plus those both the
/// delegating bot and its own bot may read.
pub async fn delegated_scope(
    pool: &sqlx::SqlitePool,
    parent: Uuid,
    target: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let parent_reads: Vec<Uuid> = crate::db::memory::readable_bots(pool, parent)
        .await?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut scope = vec![target];
    for (id, _) in crate::db::memory::readable_bots(pool, target).await? {
        if id != target && parent_reads.contains(&id) {
            scope.push(id);
        }
    }
    Ok(scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::providers::{DelegationContext, Origin};

    fn task(origin: Origin) -> Vec<Tool> {
        turn_tools(&DelegationContext {
            origin,
            delegated_task: true,
            ..DelegationContext::default()
        })
    }

    #[test]
    fn a_task_delegated_for_an_owner_request_can_ask_the_owner() {
        let tools = task(Origin::OwnerRequest);
        for tool in [
            Tool::Social,
            Tool::Mail,
            Tool::RequestSecret,
            Tool::AskOwner,
        ] {
            assert!(tools.contains(&tool), "{tool:?} missing from {tools:?}");
        }
        assert!(!tools.contains(&Tool::Delegate), "a task stays one hop");
        assert!(!tools.contains(&Tool::CreateBot));
    }

    #[test]
    fn only_a_turn_answering_the_owner_directly_can_edit_rules() {
        let direct = turn_tools(&DelegationContext {
            origin: Origin::OwnerRequest,
            ..DelegationContext::default()
        });
        assert!(direct.contains(&Tool::Rules));
        for origin in [
            Origin::OwnerRequest,
            Origin::RoutineRun,
            Origin::SocialEvent,
            Origin::ConnectorEvent,
            Origin::Other,
        ] {
            assert!(!task(origin).contains(&Tool::Rules), "delegated {origin:?}");
            let own = turn_tools(&DelegationContext {
                origin,
                ..DelegationContext::default()
            });
            assert_eq!(
                own.contains(&Tool::Rules),
                origin == Origin::OwnerRequest,
                "{origin:?}"
            );
        }
    }

    #[test]
    fn inbound_social_and_mail_events_cannot_read_installation_status() {
        assert!(task(Origin::OwnerRequest).contains(&Tool::Diagnose));
        assert!(task(Origin::RoutineRun).contains(&Tool::Diagnose));
        for origin in [Origin::SocialEvent, Origin::ConnectorEvent] {
            assert!(!task(origin).contains(&Tool::Diagnose), "{origin:?}");
        }
    }

    #[test]
    fn a_mail_event_drafts_email_for_the_owner_but_cannot_ask_them() {
        let event = turn_tools(&DelegationContext {
            origin: Origin::ConnectorEvent,
            ..DelegationContext::default()
        });
        assert!(event.contains(&Tool::DraftEmail));
        assert!(!event.contains(&Tool::AskOwner));
        assert!(
            !turn_tools(&DelegationContext {
                origin: Origin::SocialEvent,
                ..DelegationContext::default()
            })
            .contains(&Tool::DraftEmail)
        );
        assert!(task(Origin::OwnerRequest).contains(&Tool::DraftEmail));
        assert!(!task(Origin::RoutineRun).contains(&Tool::DraftEmail));
    }

    #[test]
    fn a_task_delegated_by_a_routine_run_cannot_ask_the_owner() {
        let tools = task(Origin::RoutineRun);
        for tool in [
            Tool::Social,
            Tool::Mail,
            Tool::RequestSecret,
            Tool::AskOwner,
        ] {
            assert!(!tools.contains(&tool), "{tool:?} granted in {tools:?}");
        }
    }
}
