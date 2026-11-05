use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::error::{ApiError, ApiJson};
use crate::backup::service::{Backups, Snapshot};
use crate::backup::settings::{DestinationInput, Error, Update};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/backups", get(show).put(save))
        .route("/settings/backups/test", post(test))
        .route("/settings/backups/run", post(run))
}

fn service(state: &AppState) -> Result<&Backups, ApiError> {
    state.backups.as_ref().ok_or_else(|| {
        ApiError::unavailable(
            "backups_unavailable",
            "Backups are unavailable on this server.",
        )
    })
}

fn error(error: Error) -> ApiError {
    match error {
        Error::Invalid(message) => ApiError::bad_request("invalid_backup_settings", message),
        Error::Conflict => {
            ApiError::new(StatusCode::CONFLICT, "revision_conflict", error.to_string())
        }
        Error::Busy => ApiError::new(StatusCode::CONFLICT, "backup_running", error.to_string()),
        Error::Unavailable => ApiError::unavailable("backups_unavailable", error.to_string()),
        Error::Crypto | Error::Db(_) => ApiError::internal(),
    }
}

async fn show(State(state): State<Arc<AppState>>) -> Result<Json<Snapshot>, ApiError> {
    Ok(Json(service(&state)?.snapshot().await.map_err(error)?))
}

async fn save(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<Update>,
) -> Result<Json<Snapshot>, ApiError> {
    Ok(Json(service(&state)?.save(body).await.map_err(error)?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Test {
    expected_revision: i64,
    destination: DestinationInput,
}

async fn test(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<Test>,
) -> Result<Json<Value>, ApiError> {
    service(&state)?
        .test(body.expected_revision, body.destination)
        .await
        .map_err(error)?;
    Ok(Json(
        json!({ "message": "Connection verified. Bucket list, read, write, and delete permissions are available." }),
    ))
}

async fn run(State(state): State<Arc<AppState>>) -> Result<(StatusCode, Json<Value>), ApiError> {
    service(&state)?.run_now().await.map_err(error)?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "accepted": true }))))
}
