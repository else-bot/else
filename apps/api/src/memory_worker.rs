//! The memory compactor: one loop inside the API that builds tree nodes.
//!
//! It copies an original that fits a node and joins two children that fit
//! together. Anything longer is summarized by a no-tools provider attempt
//! when one is configured. A summary runs as its own task, so copies and
//! joins for every bot keep progressing meanwhile; it takes a provider slot
//! per attempt, only while no turn is queued, and releases it between
//! attempts. Without a provider the job waits with an explicit reason and
//! the originals stay complete and traversable. No text is ever truncated
//! to make it fit.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::actor::Actor;

use async_trait::async_trait;
use serde_json::json;
use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::app::memory_tree::{self, Build};
use crate::app::providers::Profile;
use crate::db::memory_tree::{self as store, Completion, Job};
use crate::scheduler::Scheduler;

/// Longer than a whole summary batch (`BATCH_LIMIT` plus one attempt).
const LEASE: Duration = Duration::from_secs(20 * 60);
/// A summary batch starts no attempt after this long, so with an attempt's
/// own deadline it ends well inside the lease.
const BATCH_LIMIT: Duration = Duration::from_secs(12 * 60);
/// Output attempts per claim, each told the measured size of the last.
const SUMMARY_ATTEMPTS: usize = 5;
/// Summaries in flight at once (the reference allows up to eight).
const SUMMARY_CONCURRENCY: usize = 1;
const NO_SUMMARIZER_RETRY: Duration = Duration::from_secs(10 * 60);
const CAPACITY_RETRY: Duration = Duration::from_secs(20);
const IDLE: Duration = Duration::from_millis(500);

/// A provider that can summarize with no tools at all.
#[async_trait]
pub trait Summarizer: Send + Sync {
    fn id(&self) -> &'static str;
    fn available(&self) -> bool;
    /// One attempt. Cancellation stops it and reaps its process.
    async fn summarize(
        &self,
        bot_id: Uuid,
        profile: &Profile,
        prompt: &str,
        cancel: &CancellationToken,
    ) -> Result<String, String>;
}

/// Outcome of one step, for tests and logs.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Idle,
    Built,
    Deferred,
    Stale,
    /// A summary task started for this job.
    Summarizing,
}

#[derive(Clone)]
pub struct Compactor {
    pool: SqlitePool,
    summarizer: Option<Arc<dyn Summarizer>>,
    scheduler: Option<Arc<Scheduler>>,
    cancel: CancellationToken,
    summary: Actor<Option<JoinHandle<()>>>,
    /// Steps are admitted through the backup gate when one is set.
    gate: Option<Arc<crate::maintenance::Gate>>,
}

impl Compactor {
    /// Admit each step through a backup's maintenance gate.
    pub fn with_gate(mut self, gate: Arc<crate::maintenance::Gate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// Copies and joins only.
    pub fn deterministic(pool: SqlitePool) -> Self {
        Self {
            pool,
            summarizer: None,
            scheduler: None,
            cancel: CancellationToken::new(),
            summary: Actor::default(),
            gate: None,
        }
    }

    /// With model summaries that take provider slots from `scheduler`.
    pub fn with_summaries(
        pool: SqlitePool,
        summarizer: Arc<dyn Summarizer>,
        scheduler: Arc<Scheduler>,
    ) -> Self {
        Self {
            summarizer: Some(summarizer),
            scheduler: Some(scheduler),
            ..Self::deterministic(pool)
        }
    }

    async fn summary_pending(&self) -> bool {
        self.summary.call(|summary| summary.is_some()).await
    }

    /// Wait for the summary task, if one runs.
    pub async fn settle(&self) {
        self.summary
            .call_async(|summary| {
                Box::pin(async move {
                    if let Some(handle) = summary.take() {
                        let _ = handle.await;
                    }
                })
            })
            .await;
    }

    /// Claim and process one job.
    pub async fn step(&self) -> Result<Step, sqlx::Error> {
        let pool = &self.pool;
        let Some(job) = store::claim(pool, LEASE).await? else {
            // No job: publish built leaves still waiting (a long backlog
            // continues in bounded steps).
            if store::advance(pool).await? {
                return Ok(Step::Built);
            }
            return Ok(Step::Idle);
        };
        let Some(prepared) = store::prepare(pool, &job).await? else {
            store::defer(pool, &job, "missing_input", None, NO_SUMMARIZER_RETRY).await?;
            return Ok(Step::Deferred);
        };
        let audit = json!({
            "recipe": job.recipe,
            "sources": prepared.sources,
            "claim": job.attempts,
        });
        match prepared.build {
            Build::Exact { method, body } => {
                match store::complete(pool, &job, method, &body, &prepared.input_hash, audit)
                    .await?
                {
                    Completion::Done => Ok(Step::Built),
                    Completion::Stale => Ok(Step::Stale),
                }
            }
            Build::NeedsSummary { input } => {
                let Some(summarizer) = self.summarizer.as_ref().filter(|s| s.available()) else {
                    let reason = format!(
                        "{} bytes exceed the {}-byte node limit and no summary provider is available",
                        input.len(),
                        job.recipe.node_bytes
                    );
                    store::defer(
                        pool,
                        &job,
                        "summary_unavailable",
                        Some(&reason),
                        NO_SUMMARIZER_RETRY,
                    )
                    .await?;
                    return Ok(Step::Deferred);
                };
                if self.cancel.is_cancelled() {
                    // Woken as soon as the running summary ends.
                    store::defer(pool, &job, "waiting_for_summary", None, NO_SUMMARIZER_RETRY)
                        .await?;
                    return Ok(Step::Deferred);
                }
                let task = SummaryTask {
                    pool: pool.clone(),
                    summarizer: summarizer.clone(),
                    scheduler: self.scheduler.clone().expect("set with a summarizer"),
                    cancel: self.cancel.clone(),
                    job: job.clone(),
                    input,
                    input_hash: prepared.input_hash,
                    audit,
                };
                let wake_pool = pool.clone();
                let started = self
                    .summary
                    .call(move |summary| {
                        if summary.as_ref().is_some_and(|handle| !handle.is_finished()) {
                            return false;
                        }
                        let handle = tokio::spawn(async move {
                            if let Err(err) = task.run().await {
                                tracing::warn!(error = %err, "memory summary task failed");
                            }
                            // Jobs that waited for this summary may go next.
                            let woken = sqlx::query(
                                "UPDATE memory_job SET available_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                         WHERE blocked_reason = 'waiting_for_summary' AND lease_token IS NULL",
                            )
                            .execute(&wake_pool)
                            .await;
                            if let Err(err) = woken {
                                tracing::warn!(error = %err, "waking memory jobs failed");
                            }
                        });
                        *summary = Some(handle);
                        true
                    })
                    .await;
                if started {
                    Ok(Step::Summarizing)
                } else {
                    store::defer(pool, &job, "waiting_for_summary", None, NO_SUMMARIZER_RETRY)
                        .await?;
                    Ok(Step::Deferred)
                }
            }
        }
    }

    /// Process every job that is available now, waiting for summaries
    /// (tests and catch-up).
    pub async fn drain(&self) -> Result<usize, sqlx::Error> {
        let mut done = 0;
        for _ in 0..1_000_000 {
            match self.step().await? {
                // A finished summary wakes the jobs that waited for it.
                Step::Idle if self.summary_pending().await => self.settle().await,
                Step::Idle => break,
                _ => done += 1,
            }
        }
        Ok(done)
    }

    /// Run until cancelled; a running summary is stopped and awaited.
    /// Errors are logged and retried after a pause.
    pub async fn run(mut self, cancel: CancellationToken) {
        self.cancel = cancel.clone();
        loop {
            // While a backup holds the gate, compaction waits.
            let admission = match &self.gate {
                Some(gate) => match gate.enter() {
                    Some(admission) => Some(admission),
                    None => {
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                        }
                        continue;
                    }
                },
                None => None,
            };
            let step = self.step().await;
            drop(admission);
            let pause = match step {
                Ok(Step::Idle) => IDLE,
                Ok(_) => Duration::ZERO,
                Err(err) => {
                    tracing::warn!(error = %err, "memory compactor step failed");
                    Duration::from_secs(5)
                }
            };
            if cancel.is_cancelled() {
                break;
            }
            if pause.is_zero() {
                continue;
            }
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(pause) => {}
            }
        }
        self.settle().await;
    }
}

/// One job's summary attempts, run beside the compactor loop.
struct SummaryTask {
    pool: SqlitePool,
    summarizer: Arc<dyn Summarizer>,
    scheduler: Arc<Scheduler>,
    cancel: CancellationToken,
    job: Job,
    input: String,
    input_hash: Vec<u8>,
    audit: serde_json::Value,
}

impl SummaryTask {
    async fn wait(
        &self,
        reason: &str,
        error: Option<&str>,
        retry: Duration,
        failure: bool,
    ) -> Result<(), sqlx::Error> {
        store::defer_counting(&self.pool, &self.job, reason, error, retry, failure).await
    }

    /// Back off by provider failures only (capacity waits do not count).
    async fn fail(&self, reason: &str, error: &str) -> Result<(), sqlx::Error> {
        let failures = store::failures(&self.pool, &self.job).await? + 1;
        self.wait(reason, Some(error), backoff(failures), true)
            .await
    }

    async fn run(mut self) -> Result<(), sqlx::Error> {
        let pool = self.pool.clone();
        let job = self.job.clone();
        let Some(profile) = crate::db::profiles::get(&pool, self.summarizer.id()).await? else {
            return self
                .wait(
                    "summary_unavailable",
                    Some("no saved profile"),
                    NO_SUMMARIZER_RETRY,
                    false,
                )
                .await;
        };
        let (revision, context) = store::context_before(
            &pool,
            job.bot_id,
            job.generation,
            job.node.start,
            job.recipe.context_low,
        )
        .await?;
        let context_text = memory_tree::render(
            &context
                .iter()
                .map(|(n, b)| (*n, b.as_str()))
                .collect::<Vec<_>>(),
        );
        let started = Instant::now();
        let mut sizes = Vec::new();
        let mut previous = None;
        for _ in 0..SUMMARY_ATTEMPTS {
            if self.cancel.is_cancelled() {
                return self
                    .wait(
                        "waiting_for_capacity",
                        Some("the API is stopping"),
                        CAPACITY_RETRY,
                        false,
                    )
                    .await;
            }
            if started.elapsed() > BATCH_LIMIT {
                break;
            }
            // Each attempt takes a slot only while no turn waits, and gives
            // it back afterwards, so an arriving message is not held up by
            // the remaining attempts.
            let waiting: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM turns WHERE status = 'queued')")
                    .fetch_one(&pool)
                    .await?;
            let permit = if waiting {
                None
            } else {
                self.scheduler.try_begin_summary(SUMMARY_CONCURRENCY)
            };
            let Some(permit) = permit else {
                return self
                    .wait("waiting_for_capacity", None, CAPACITY_RETRY, false)
                    .await;
            };
            let prompt = memory_tree::summary_prompt(
                job.node,
                &self.input,
                &context_text,
                job.recipe.node_bytes,
                previous,
            );
            let result = self
                .summarizer
                .summarize(job.bot_id, &profile.profile(), &prompt, &self.cancel)
                .await;
            drop(permit);
            match result {
                Ok(text) if !text.is_empty() && text.len() <= job.recipe.node_bytes => {
                    sizes.push(text.len());
                    self.audit["model"] = json!({
                        "provider": self.summarizer.id(),
                        "model": profile.model,
                        "reasoning_effort": profile.reasoning_effort,
                        "service_tier": profile.service_tier,
                        "profile_revision": profile.revision,
                        "prompt_version": crate::providers::codex::SUMMARY_PROMPT_VERSION,
                        "attempt_bytes": sizes,
                        "output_bytes": text.len(),
                        "input_bytes": self.input.len(),
                    });
                    self.audit["context"] = json!({
                        "view_revision": revision,
                        "nodes": context.iter().map(|(n, _)| json!([n.start, n.span])).collect::<Vec<_>>(),
                        "bytes": context_text.len(),
                    });
                    store::complete(
                        &pool,
                        &job,
                        "model",
                        &text,
                        &self.input_hash,
                        self.audit.clone(),
                    )
                    .await?;
                    return Ok(());
                }
                Ok(text) => {
                    // Oversized or empty: never cut it; ask again with the measure.
                    sizes.push(text.len());
                    previous = Some(text.len());
                }
                Err(_) if self.cancel.is_cancelled() => {
                    return self
                        .wait(
                            "waiting_for_capacity",
                            Some("the API is stopping"),
                            CAPACITY_RETRY,
                            false,
                        )
                        .await;
                }
                Err(error) => {
                    tracing::warn!(bot = %job.bot_id, node = ?job.node, %error, "memory summary failed");
                    return self.fail("summary_failed", &error).await;
                }
            }
        }
        let reason = format!(
            "{} attempts over the {}-byte limit: {sizes:?} bytes",
            sizes.len(),
            job.recipe.node_bytes
        );
        self.fail("summary_oversized", &reason).await
    }
}

/// Bounded exponential backoff after provider failures: 1 minute doubling
/// to 1 hour.
pub fn backoff(failures: i32) -> Duration {
    let minutes = 1u64 << (failures.clamp(1, 7) - 1);
    Duration::from_secs(60 * minutes.min(60))
}

/// Copies and joins only, until nothing is available (tests).
pub async fn drain(pool: &SqlitePool) -> Result<usize, sqlx::Error> {
    Compactor::deterministic(pool.clone()).drain().await
}

pub async fn step(pool: &SqlitePool) -> Result<Step, sqlx::Error> {
    Compactor::deterministic(pool.clone()).step().await
}
