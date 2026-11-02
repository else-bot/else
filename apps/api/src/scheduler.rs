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
            tasks: TaskTracker::new(),
        })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
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
        while self.running().len() < self.max_active {
            let Some(turn_id) = turns::claim_next(&self.pool, &[]).await? else {
                break;
            };
            // Start each claim before the next query, so an error in a later
            // claim cannot strand this one.
            let token = CancellationToken::new();
            self.running().insert(turn_id, token.clone());
            let this = self.clone();
            self.tasks.spawn(async move {
                this.execute(turn_id, token).await;
                this.running().remove(&turn_id);
                this.wake();
            });
        }
        Ok(())
    }

    async fn execute(&self, turn_id: Uuid, cancel: CancellationToken) {
        let input = match turns::input(&self.pool, turn_id).await {
            Ok(input) => input,
            Err(err) => {
                tracing::error!(turn = %turn_id, error = %err, "loading turn failed");
                return;
            }
        };
        let provider_id: String =
            match sqlx::query_scalar("SELECT provider_id FROM turns WHERE id = $1")
                .bind(turn_id)
                .fetch_one(&self.pool)
                .await
            {
                Ok(id) => id,
                Err(err) => {
                    tracing::error!(turn = %turn_id, error = %err, "loading turn provider failed");
                    return;
                }
            };
        let sink = PgSink::new(self.pool.clone(), &input, &provider_id);
        let outcome = match self.registry.get(&provider_id) {
            None => TurnOutcome::Failed {
                code: "provider_unavailable".into(),
                message: "The provider for this message is no longer configured.".into(),
            },
            Some(provider) => {
                let provider = provider.clone();
                // The adapter and a periodic flusher run concurrently in this
                // task. The flusher stops only between flushes, so a flush is
                // never abandoned halfway, and neither future can starve the
                // other while one holds the sink's buffer lock.
                let done = CancellationToken::new();
                let run = async {
                    let outcome = provider.run(input, &sink, cancel.clone()).await;
                    done.cancel();
                    outcome
                };
                let flusher = async {
                    loop {
                        tokio::select! {
                            _ = done.cancelled() => break,
                            _ = tokio::time::sleep(FLUSH_INTERVAL) => {}
                        }
                        if sink.flush().await.is_err() {
                            // Fenced: the turn was stopped or the bot deleted.
                            cancel.cancel();
                        }
                    }
                };
                // A provider that ignores interruption is abandoned after
                // the grace period; its effects are then unknown.
                let watchdog = async {
                    cancel.cancelled().await;
                    tokio::time::sleep(self.cancel_grace).await;
                };
                tokio::select! {
                    (outcome, ()) = async { tokio::join!(run, flusher) } => outcome,
                    () = watchdog => TurnOutcome::Uncertain {
                        message: format!(
                            "The provider did not stop within {} seconds after being interrupted; \
                             its effects are unknown.",
                            self.cancel_grace.as_secs()
                        ),
                    },
                }
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
