//! Owner routes for the Vault, the installation's task secrets: credentials
//! any bot uses, such as SMTP settings or a website login. Listing shows
//! names, descriptions, websites, variable names, and the last use, never
//! values; only `reveal` returns values, to the owner. Task grants never
//! reach these routes (everything under `/v1` needs the owner token).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiPath, SecretJson};
use crate::db::secret_requests::{self, SettleError, Settled};
use crate::db::task_secrets::{self, Meta, Mode, Patch, SecretError, Website};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/task-secrets", get(list))
        .route(
            "/task-secrets/{name}",
            axum::routing::put(replace)
                .post(create)
                .patch(edit)
                .delete(remove),
        )
        .route("/task-secrets/{name}/reveal", post(reveal))
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
    description: Option<String>,
    #[serde(default, deserialize_with = "present")]
    website: Option<Option<String>>,
}

/// Tells an explicit `null` (clear it) from an absent field (keep it).
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

fn meta(description: Option<String>, website: Option<Option<String>>) -> Result<Meta, ApiError> {
    let description = description
        .map(|d| task_secrets::description(&d))
        .transpose()
        .map_err(error)?;
    let website = website
        .map(|w| w.as_deref().map(Website::parse).transpose())
        .transpose()
        .map_err(error)?;
    Ok(Meta {
        description,
        website,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    description: Option<String>,
    #[serde(default, deserialize_with = "present")]
    website: Option<Option<String>>,
    #[serde(default)]
    set: BTreeMap<String, String>,
    #[serde(default)]
    remove: BTreeSet<String>,
}

pub(super) fn error(err: SecretError) -> ApiError {
    match err {
        SecretError::Invalid(message) => ApiError::bad_request("invalid_secret", message),
        SecretError::NoKey => ApiError::unavailable(
            "secrets_unavailable",
            "Task secrets need the installation's SECRET_KEY.",
        ),
        SecretError::Db(err) => err.into(),
        SecretError::Unknown(_) => ApiError::not_found(),
        err @ SecretError::Exists(_) => {
            ApiError::new(StatusCode::CONFLICT, "secret_exists", err.to_string())
        }
        other => {
            tracing::warn!(error = %other, "task secret operation failed");
            ApiError::unavailable(
                "secrets_unavailable",
                "The task secret could not be stored.",
            )
        }
    }
}

async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let secrets = task_secrets::list(&state.db).await?;
    Ok(Json(json!({ "secrets": secrets })))
}

/// Create or replace an entry.
async fn replace(
    State(state): State<Arc<AppState>>,
    ApiPath(name): ApiPath<String>,
    SecretJson(body): SecretJson<Body>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    store(&state, &name, body, Mode::CreateOrReplace).await
}

/// Create an entry; a name already taken is a conflict, never overwritten.
async fn create(
    State(state): State<Arc<AppState>>,
    ApiPath(name): ApiPath<String>,
    SecretJson(body): SecretJson<Body>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    store(&state, &name, body, Mode::CreateOnly).await
}

async fn store(
    state: &AppState,
    name: &str,
    body: Body,
    mode: Mode,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let key = state
        .secret_key
        .as_deref()
        .ok_or_else(|| error(SecretError::NoKey))?;
    let meta = meta(body.description, body.website)?;
    let created = task_secrets::put(&state.db, key, name, &body.env, &meta, mode)
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

/// Edit an entry: its description and website, values to set, and
/// variables to remove. Values are never returned.
async fn edit(
    State(state): State<Arc<AppState>>,
    ApiPath(name): ApiPath<String>,
    SecretJson(body): SecretJson<Edit>,
) -> Result<Json<Value>, ApiError> {
    let patch = Patch {
        meta: meta(body.description, body.website)?,
        set: body.set,
        remove: body.remove,
    };
    let entry = task_secrets::update(&state.db, state.secret_key.as_deref(), &name, &patch)
        .await
        .map_err(error)?;
    Ok(Json(json!(entry)))
}

/// Show an entry's values to the owner, who confirmed it in the desktop.
async fn reveal(
    State(state): State<Arc<AppState>>,
    ApiPath(name): ApiPath<String>,
) -> Result<Json<Value>, ApiError> {
    let env = task_secrets::reveal(&state.db, state.secret_key.as_deref(), &name)
        .await
        .map_err(error)?;
    tracing::info!(secret = %name, "the owner revealed a vault entry");
    let env: BTreeMap<&str, &str> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    Ok(Json(json!({ "env": env })))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    ApiPath(name): ApiPath<String>,
) -> Result<StatusCode, ApiError> {
    if task_secrets::delete(&state.db, &name).await? {
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
/// a task secret every bot can use and continue the waiting turn. Values are never returned.
async fn fulfill(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, request_id)): ApiPath<(Uuid, Uuid)>,
    SecretJson(body): SecretJson<Body>,
) -> Result<Json<Value>, ApiError> {
    let meta = meta(body.description, body.website)?;
    let done = secret_requests::fulfill(
        &state.db,
        state.secret_key.as_deref(),
        bot_id,
        request_id,
        &body.env,
        meta,
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
