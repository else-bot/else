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

pub struct Scheduler {
    pool: PgPool,
    registry: Registry,
    wake: Notify,
    /// Cancellation handles of turns running in this process, by turn ID.
    running: Mutex<HashMap<Uuid, CancellationToken>>,
    max_active: usize,
    tasks: TaskTracker,
}

impl Scheduler {
    pub fn new(pool: PgPool, registry: Registry, max_active: usize) -> Arc<Self> {
        Arc::new(Self {
            pool,
            registry,
            wake: Notify::new(),
            running: Mutex::new(HashMap::new()),
            max_active,
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
        let free = self.max_active.saturating_sub(self.running().len());
        for turn_id in turns::claim(&self.pool, free as i64).await? {
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
                // Flush buffered text periodically while the adapter runs.
                let run = provider.run(input, &sink, cancel.clone());
                tokio::pin!(run);
                loop {
                    tokio::select! {
                        outcome = &mut run => break outcome,
                        _ = tokio::time::sleep(FLUSH_INTERVAL) => {
                            if sink.flush().await.is_err() {
                                // Fenced: the turn was stopped or the bot deleted.
                                cancel.cancel();
                            }
                        }
                    }
                }
            }
        };
        tracing::info!(turn = %turn_id, provider = %provider_id, ?outcome, "turn finished");
        if let Err(err) = sink.finish(&outcome).await {
            tracing::error!(turn = %turn_id, error = %err, "recording turn outcome failed");
        }
    }
}
