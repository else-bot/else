//! Owner routes for a bot's task secrets: credentials its guest
//! scripts use, such as SMTP settings. Values are write-only: listing shows
//! names and variable names, never values. Task grants never reach these
//! routes (everything under `/v1` needs the owner token).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiPath, SecretJson};
use super::turns::active_bot;
use crate::db::secret_requests::{self, SettleError, Settled};
use crate::db::task_secrets::{self, SecretError};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/task-secrets", get(list))
        .route(
            "/bots/{bot_id}/task-secrets/{name}",
            put(store).delete(remove),
        )
        .route(
            "/bots/{bot_id}/secret-requests/{request_id}/fulfill",
            post(fulfill),
        )
        .route(
            "/bots/{bot_id}/secret-requests/{request_id}/cancel",
            post(cancel),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    env: BTreeMap<String, String>,
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
    SecretJson(body): SecretJson<Body>,
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

fn settle_error(err: SettleError) -> ApiError {
    match err {
        SettleError::NotFound => ApiError::not_found(),
        SettleError::Settled(status) => ApiError::new(
            StatusCode::CONFLICT,
            "secret_request_settled",
            format!(
                "This request was already {}.",
                serde_json::to_value(status)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or_default()
            ),
        ),
        err @ SettleError::EnvMismatch(_) => {
            ApiError::bad_request("invalid_secret", err.to_string())
        }
        SettleError::Secret(err) => error(err),
    }
}

fn settled(state: &AppState, settled: Settled) -> Json<Value> {
    if settled.requeued {
        state.scheduler.wake();
    }
    Json(json!({ "request": settled.request, "requeued": settled.requeued }))
}

/// Answer a bot's secret request: store exactly the requested variables as
/// its task secret and continue the waiting turn. Values are never returned.
async fn fulfill(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, request_id)): ApiPath<(Uuid, Uuid)>,
    SecretJson(body): SecretJson<Body>,
) -> Result<Json<Value>, ApiError> {
    let done = secret_requests::fulfill(
        &state.db,
        state.secret_key.as_deref(),
        bot_id,
        request_id,
        &body.env,
    )
    .await
    .map_err(settle_error)?;
    Ok(settled(&state, done))
}

async fn cancel(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, request_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let done = secret_requests::cancel(&state.db, bot_id, request_id)
        .await
        .map_err(settle_error)?;
    Ok(settled(&state, done))
}
