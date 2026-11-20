//! The owner's watcher route: stop one. Watchers reach the owner in the
//! conversation snapshot and `bot.updated`.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use uuid::Uuid;

use super::error::{ApiError, ApiPath};
use super::{AppState, Routes};
use crate::db::watchers::{self, StopBy, Stopped};
use crate::views::watcher_json;

pub fn routes() -> Routes {
    Routes::gated(Router::new().route(
        "/bots/{bot_id}/watchers/{watcher_id}/stop",
        post(owner_stop),
    ))
}

/// Stop a watcher; its bot is woken once with the stop.
async fn owner_stop(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, watcher_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    match watchers::stop(&state.db, bot_id, watcher_id, StopBy::Owner).await? {
        Stopped::Stopped(watcher) => {
            state.scheduler.wake();
            Ok(Json(json!({ "watcher": watcher_json(&watcher) })))
        }
        Stopped::NotActive(_) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_active",
            "This watcher already ended.",
        )),
        Stopped::NotFound => Err(ApiError::not_found()),
    }
}
