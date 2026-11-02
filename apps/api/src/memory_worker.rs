//! The memory compactor: one loop inside the API that builds tree nodes.
//!
//! It copies an original that fits a node and joins two children that fit
//! together. Anything longer is summarized by a no-tools provider attempt
//! when one is configured and a provider slot is free (summaries are low
//! priority and share the turn budget); otherwise the job waits with an
//! explicit reason and the originals stay complete and traversable. No text
//! is ever truncated to make it fit.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::app::memory_tree::{self, Build};
use crate::app::providers::Profile;
use crate::db::memory_tree::{self as store, Completion, Job};
use crate::scheduler::Scheduler;

/// Covers a summary with all its attempts; deterministic work needs little.
const LEASE: Duration = Duration::from_secs(15 * 60);
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
    async fn summarize(
        &self,
        bot_id: Uuid,
        profile: &Profile,
        prompt: &str,
    ) -> Result<String, String>;
}

/// Outcome of one step, for tests and logs.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Idle,
    Built,
    Deferred,
    Stale,
}

#[derive(Clone)]
pub struct Compactor {
    pool: PgPool,
    summarizer: Option<Arc<dyn Summarizer>>,
    scheduler: Option<Arc<Scheduler>>,
}

impl Compactor {
    /// Copies and joins only.
    pub fn deterministic(pool: PgPool) -> Self {
        Self {
            pool,
            summarizer: None,
            scheduler: None,
        }
    }

    /// With model summaries that take provider slots from `scheduler`.
    pub fn with_summaries(
        pool: PgPool,
        summarizer: Arc<dyn Summarizer>,
        scheduler: Arc<Scheduler>,
    ) -> Self {
        Self {
            pool,
            summarizer: Some(summarizer),
            scheduler: Some(scheduler),
        }
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
                self.finish(&job, method, &body, &prepared.input_hash, audit)
                    .await
            }
            Build::NeedsSummary { input } => {
                self.summarize(&job, &input, &prepared.input_hash, audit)
                    .await
            }
        }
    }

    async fn finish(
        &self,
        job: &Job,
        method: &str,
        body: &str,
        input_hash: &[u8],
        audit: serde_json::Value,
    ) -> Result<Step, sqlx::Error> {
        match store::complete(&self.pool, job, method, body, input_hash, audit).await? {
            Completion::Done => Ok(Step::Built),
            Completion::Stale => Ok(Step::Stale),
        }
    }

    async fn wait(
        &self,
        job: &Job,
        reason: &str,
        error: Option<&str>,
        retry: Duration,
    ) -> Result<Step, sqlx::Error> {
        store::defer(&self.pool, job, reason, error, retry).await?;
        Ok(Step::Deferred)
    }

    async fn summarize(
        &self,
        job: &Job,
        input: &str,
        input_hash: &[u8],
        mut audit: serde_json::Value,
    ) -> Result<Step, sqlx::Error> {
        let pool = &self.pool;
        let (Some(summarizer), Some(scheduler)) = (&self.summarizer, &self.scheduler) else {
            let reason = format!(
                "{} bytes exceed the {}-byte node limit and no summary provider is configured",
                input.len(),
                job.recipe.node_bytes
            );
            return self
                .wait(
                    job,
                    "summary_unavailable",
                    Some(&reason),
                    NO_SUMMARIZER_RETRY,
                )
                .await;
        };
        if !summarizer.available() {
            let reason = "the summary provider is unavailable";
            return self
                .wait(
                    job,
                    "summary_unavailable",
                    Some(reason),
                    NO_SUMMARIZER_RETRY,
                )
                .await;
        }
        let Some(profile) = crate::db::profiles::get(pool, summarizer.id()).await? else {
            return self
                .wait(
                    job,
                    "summary_unavailable",
                    Some("no saved profile"),
                    NO_SUMMARIZER_RETRY,
                )
                .await;
        };
        // Low priority: never while a turn waits, and only in a free slot.
        let waiting: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM turns WHERE status = 'queued')")
                .fetch_one(pool)
                .await?;
        let permit = if waiting {
            None
        } else {
            scheduler.try_begin_summary(SUMMARY_CONCURRENCY)
        };
        let Some(_permit) = permit else {
            return self
                .wait(job, "waiting_for_capacity", None, CAPACITY_RETRY)
                .await;
        };
        let (revision, context) = store::context_before(
            pool,
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
        let mut sizes = Vec::new();
        let mut previous = None;
        for _ in 0..SUMMARY_ATTEMPTS {
            let prompt = memory_tree::summary_prompt(
                job.node,
                input,
                &context_text,
                job.recipe.node_bytes,
                previous,
            );
            match summarizer
                .summarize(job.bot_id, &profile.profile(), &prompt)
                .await
            {
                Ok(text) if !text.is_empty() && text.len() <= job.recipe.node_bytes => {
                    sizes.push(text.len());
                    audit["model"] = json!({
                        "provider": summarizer.id(),
                        "model": profile.model,
                        "reasoning_effort": profile.reasoning_effort,
                        "service_tier": profile.service_tier,
                        "profile_revision": profile.revision,
                        "prompt_version": crate::providers::codex::SUMMARY_PROMPT_VERSION,
                        "attempt_bytes": sizes,
                        "output_bytes": text.len(),
                        "input_bytes": input.len(),
                    });
                    audit["context"] = json!({
                        "view_revision": revision,
                        "nodes": context.iter().map(|(n, _)| json!([n.start, n.span])).collect::<Vec<_>>(),
                        "bytes": context_text.len(),
                    });
                    return self.finish(job, "model", &text, input_hash, audit).await;
                }
                Ok(text) => {
                    // Oversized or empty: never cut it; ask again with the measure.
                    sizes.push(text.len());
                    previous = Some(text.len());
                }
                Err(error) => {
                    tracing::warn!(bot = %job.bot_id, node = ?job.node, %error, "memory summary failed");
                    return self
                        .wait(job, "summary_failed", Some(&error), backoff(job.attempts))
                        .await;
                }
            }
        }
        let reason = format!(
            "{SUMMARY_ATTEMPTS} attempts over the {}-byte limit: {sizes:?} bytes",
            job.recipe.node_bytes
        );
        self.wait(
            job,
            "summary_oversized",
            Some(&reason),
            backoff(job.attempts),
        )
        .await
    }

    /// Process every job that is available now (tests and catch-up).
    pub async fn drain(&self) -> Result<usize, sqlx::Error> {
        let mut done = 0;
        for _ in 0..1_000_000 {
            match self.step().await? {
                Step::Idle => break,
                _ => done += 1,
            }
        }
        Ok(done)
    }

    /// Run until cancelled. Errors are logged and retried after a pause.
    pub async fn run(self, cancel: CancellationToken) {
        loop {
            let pause = match self.step().await {
                Ok(Step::Idle) => IDLE,
                Ok(_) => Duration::ZERO,
                Err(err) => {
                    tracing::warn!(error = %err, "memory compactor step failed");
                    Duration::from_secs(5)
                }
            };
            if pause.is_zero() {
                if cancel.is_cancelled() {
                    return;
                }
                continue;
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(pause) => {}
            }
        }
    }
}

/// Bounded exponential backoff after failed claims: 1 minute doubling to 1 hour.
pub fn backoff(claims: i32) -> Duration {
    let minutes = 1u64 << (claims.clamp(1, 7) - 1);
    Duration::from_secs(60 * minutes.min(60))
}

/// Copies and joins only, until nothing is available (tests).
pub async fn drain(pool: &PgPool) -> Result<usize, sqlx::Error> {
    Compactor::deterministic(pool.clone()).drain().await
}

pub async fn step(pool: &PgPool) -> Result<Step, sqlx::Error> {
    Compactor::deterministic(pool.clone()).step().await
}
