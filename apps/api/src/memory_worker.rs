//! The memory compactor: one loop inside the API that builds tree nodes.
//!
//! It copies an original that fits a node and joins two children that fit
//! together. Anything longer needs a summary; until a no-tools summary
//! provider exists (POC-015) such jobs wait with `summary_unavailable` and
//! the originals stay complete and traversable. No text is ever truncated
//! to make it fit.

use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::app::memory_tree::Build;
use crate::db::memory_tree::{self, Completion};

/// Lease for one deterministic build; generous for a database round trip.
const LEASE: Duration = Duration::from_secs(60);
/// How long a job that needs a summary waits before it is reconsidered.
const SUMMARY_RETRY: Duration = Duration::from_secs(10 * 60);
const IDLE: Duration = Duration::from_millis(500);

/// Outcome of one step, for tests and logs.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Idle,
    Built,
    Deferred,
    Stale,
}

/// Claim and process one job.
pub async fn step(pool: &PgPool) -> Result<Step, sqlx::Error> {
    let Some(job) = memory_tree::claim(pool, LEASE).await? else {
        return Ok(Step::Idle);
    };
    let Some(prepared) = memory_tree::prepare(pool, &job).await? else {
        memory_tree::defer(pool, &job, "missing_input", None, SUMMARY_RETRY).await?;
        return Ok(Step::Deferred);
    };
    match prepared.build {
        Build::Exact { method, body } => {
            let audit = json!({
                "recipe": job.recipe,
                "sources": prepared.sources,
                "attempt": job.attempts,
            });
            match memory_tree::complete(pool, &job, method, &body, &prepared.input_hash, audit)
                .await?
            {
                Completion::Done => Ok(Step::Built),
                Completion::Stale => Ok(Step::Stale),
            }
        }
        Build::NeedsSummary { input } => {
            let reason = format!(
                "{} bytes exceed the {}-byte node limit and no summary provider is available",
                input.len(),
                job.recipe.node_bytes
            );
            memory_tree::defer(
                pool,
                &job,
                "summary_unavailable",
                Some(&reason),
                SUMMARY_RETRY,
            )
            .await?;
            Ok(Step::Deferred)
        }
    }
}

/// Process every job that is available now (tests and catch-up).
pub async fn drain(pool: &PgPool) -> Result<usize, sqlx::Error> {
    let mut done = 0;
    for _ in 0..1_000_000 {
        match step(pool).await? {
            Step::Idle => break,
            _ => done += 1,
        }
    }
    Ok(done)
}

/// Run until cancelled. Errors are logged and retried after a pause.
pub async fn run(pool: PgPool, cancel: CancellationToken) {
    loop {
        let pause = match step(&pool).await {
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
