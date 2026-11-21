//! The owner's stop of one thing a bot waits on: an MCP job, a computer
//! job, or a delegated task. Stopping one leaves the bot's other waits and
//! any generating attempt alone. Also the result the wait tools return.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use uuid::Uuid;

use super::error::{ApiError, ApiPath};
use super::{AppState, Routes};
use crate::db::holds::Created;
use crate::db::waits::{self, StopError};

/// What a wait tool tells the bot to do next, by outcome and by whether
/// the turn serves the owner's chat or a task.
pub(super) struct Next {
    pub waiting_owner: &'static str,
    pub waiting_task: &'static str,
    pub carried_owner: &'static str,
    pub carried_task: &'static str,
}

/// The tool result for a wait the caller's turn now has. A new wait parks
/// the attempt, so its grant stops serving tools.
pub(super) async fn waited(
    state: &AppState,
    turn_id: Uuid,
    created: Created,
    owner: bool,
    next: &Next,
) -> Value {
    let next = match (created, owner) {
        (Created::Waiting(_), true) => next.waiting_owner,
        (Created::Waiting(_), false) => next.waiting_task,
        (Created::Carried(_), true) => next.carried_owner,
        (Created::Carried(_), false) => next.carried_task,
    };
    if created.parked() {
        state.grants.park(turn_id).await;
    }
    json!({
        "status": created.status(),
        "parked": created.parked(),
        "wait_id": created.id(),
        "next": next,
    })
}

pub fn routes() -> Routes {
    Routes::gated(Router::new().route("/bots/{bot_id}/waits/{wait_id}/cancel", post(cancel)))
}

/// Stop one wait. The bot is woken with the stop as that wait's outcome
/// once nothing else holds its turn. A repeat returns the same stop; a
/// wait that already ended is `409 wait_ended`.
async fn cancel(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, wait_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let stopped = waits::stop(&state.db, bot_id, wait_id)
        .await
        .map_err(|err| match err {
            StopError::NotFound => ApiError::not_found(),
            StopError::Ended => ApiError::new(
                StatusCode::CONFLICT,
                "wait_ended",
                "This wait already ended.",
            ),
            StopError::Db(err) => ApiError::from(err),
        })?;
    if stopped.created {
        state.scheduler.wake();
    }
    Ok(Json(json!({
        "kind": stopped.kind,
        "created": stopped.created,
        "requeued": stopped.requeued,
    })))
}
