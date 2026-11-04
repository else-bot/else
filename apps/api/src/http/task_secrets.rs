//! Owner routes for a bot's task secrets (POC-026): credentials its guest
//! scripts use, such as SMTP settings. Values are write-only: listing shows
//! names and variable names, never values. Task grants never reach these
//! routes (everything under `/v1` needs the owner token).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use super::turns::bot_deleted;
use crate::db::task_secrets::{self, SecretError};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/task-secrets", get(list))
        .route(
            "/bots/{bot_id}/task-secrets/{name}",
            put(store).delete(remove),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    env: BTreeMap<String, String>,
}

async fn active_bot(state: &AppState, bot_id: Uuid) -> Result<(), ApiError> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_optional(&state.db)
        .await?;
    match status.as_deref() {
        None => Err(ApiError::not_found()),
        Some("active") => Ok(()),
        Some(_) => Err(bot_deleted()),
    }
}

fn error(err: SecretError) -> ApiError {
    match err {
        SecretError::Invalid(message) => ApiError::bad_request("invalid_secret", message),
        SecretError::NoKey => ApiError::unavailable(
            "secrets_unavailable",
            "Task secrets need the installation's SECRET_KEY.",
        ),
        SecretError::Db(err) => err.into(),
        other => {
            tracing::warn!(error = %other, "task secret operation failed");
            ApiError::unavailable(
                "secrets_unavailable",
                "The task secret could not be stored.",
            )
        }
    }
}

async fn list(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    active_bot(&state, bot_id).await?;
    let secrets = task_secrets::list(&state.db, bot_id).await?;
    Ok(Json(json!({ "secrets": secrets })))
}

async fn store(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, name)): ApiPath<(Uuid, String)>,
    ApiJson(body): ApiJson<Body>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    active_bot(&state, bot_id).await?;
    let key = state
        .secret_key
        .as_deref()
        .ok_or_else(|| error(SecretError::NoKey))?;
    let created = task_secrets::put(&state.db, key, bot_id, &name, &body.env)
        .await
        .map_err(error)?;
    let short: Vec<&String> = body
        .env
        .iter()
        .filter(|(_, v)| v.len() < task_secrets::MIN_REDACTED)
        .map(|(k, _)| k)
        .collect();
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(json!({
            "name": name,
            "env": body.env.keys().collect::<Vec<_>>(),
            // Values this short are not redacted from output.
            "not_redacted": short,
        })),
    ))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, name)): ApiPath<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    active_bot(&state, bot_id).await?;
    if task_secrets::delete(&state.db, bot_id, &name).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}
