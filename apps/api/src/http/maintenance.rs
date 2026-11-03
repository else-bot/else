//! Preparing an update (owner only). The installer calls this before it
//! stops the API to switch versions:
//!
//! - `POST /v1/maintenance/update` closes the maintenance gate (no new turn,
//!   mutation, tool call, or memory step is admitted; queued messages stay
//!   queued), then refuses with `409 busy`, reopening the gate, if a turn is
//!   still active or a command still runs in a bot computer. Otherwise the
//!   gate stays closed until the process exits, so nothing can start
//!   between this answer and the shutdown. Background jobs in the bot
//!   computers are reported: they keep running across a restart.
//! - `DELETE /v1/maintenance/update` reopens the gate (an update that was
//!   abandoned after preparing).
//!
//! Both bypass the gate themselves (see `maintenance::allowed`).

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use serde_json::{Value, json};

use super::AppState;
use super::error::ApiError;

/// How long requests and steps already admitted get to finish.
const ADMITTED_GRACE: Duration = Duration::from_secs(10);

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/maintenance/update", post(prepare).delete(reopen))
}

async fn prepare(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let owner = state.clone();
    state
        .update_hold
        .call_async(move |hold| {
            Box::pin(async move {
                let state = owner;
                if hold.is_none() {
                    let closed = match state.scheduler.gate().close(ADMITTED_GRACE).await {
                        Ok(Some(closed)) => closed,
                        Ok(None) => {
                            return Err(busy("A backup is running; update once it has finished."));
                        }
                        Err(admitted) => {
                            return Err(busy(format!(
                                "{admitted} requests or steps were still running after {} s.",
                                ADMITTED_GRACE.as_secs()
                            )));
                        }
                    };
                    // Admission has stopped: what runs now can only finish, not start.
                    let work = work(&state).await?;
                    if work.turns > 0 || work.commands > 0 {
                        drop(closed);
                        return Err(busy(format!(
                            "Bots are working ({} active turns, {} running commands); \
                 nothing was changed. Update once they have finished.",
                            work.turns, work.commands
                        )));
                    }
                    *hold = Some(closed);
                    tracing::info!("prepared for an update; new work is refused until exit");
                }
                let work = work(&state).await?;
                Ok(Json(json!({
                    "ready": true,
                    "version": crate::VERSION,
                    "queued_turns": work.queued,
                    "background_jobs": work.jobs,
                })))
            })
        })
        .await
}

async fn reopen(State(state): State<Arc<AppState>>) -> Json<Value> {
    let reopened = state.update_hold.call(|hold| hold.take().is_some()).await;
    if reopened {
        tracing::info!("update abandoned; accepting work again");
    }
    Json(json!({ "reopened": reopened }))
}

struct Work {
    turns: i64,
    commands: i64,
    queued: i64,
    jobs: i64,
}

async fn work(state: &AppState) -> Result<Work, ApiError> {
    let (turns, queued): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FILTER (WHERE status IN {}), count(*) FILTER (WHERE status = 'queued')
         FROM turns",
        crate::db::turns::ACTIVE_STATUSES
    )))
    .fetch_one(&state.db)
    .await
    .map_err(|_| unavailable())?;
    let commands: i64 =
        sqlx::query_scalar("SELECT count(*) FROM guest_runs WHERE ended_at IS NULL")
            .fetch_one(&state.db)
            .await
            .map_err(|_| unavailable())?;
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM guest_jobs WHERE status IN ('starting', 'running', 'cancelling')",
    )
    .fetch_one(&state.db)
    .await
    .map_err(|_| unavailable())?;
    // A turn this process still runs counts even before its row says so.
    let turns = turns.max(state.scheduler.running_count().await as i64);
    Ok(Work {
        turns,
        commands,
        queued,
        jobs,
    })
}

fn busy(message: impl Into<String>) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "busy", message).retryable()
}

fn unavailable() -> ApiError {
    ApiError::unavailable("database_unavailable", "The database is not reachable.")
}
