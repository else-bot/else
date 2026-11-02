//! Owner bot management routes.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::get};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use crate::app::bots::{BotName, BotRole};
use crate::db::bots::{self, BotError, BotRecord, BotStatus, Created};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots", get(list).post(create))
        .route("/bots/{bot_id}", get(show).delete(delete))
}

impl From<BotError> for ApiError {
    fn from(err: BotError) -> Self {
        match err {
            BotError::NotFound => ApiError::not_found(),
            BotError::RequestConflict => ApiError::new(
                StatusCode::CONFLICT,
                "request_conflict",
                "This request ID was already used with different input.",
            ),
            BotError::Db(err) => err.into(),
        }
    }
}

/// Public projection of an active bot or a deletion receipt.
pub fn bot_json(bot: &BotRecord) -> Value {
    if bot.status != BotStatus::Active {
        // Deletion receipts expose no role or conversation reference.
        return json!({
            "id": bot.id,
            "name": bot.name,
            "deleted_at": bot.deleted_at,
            "cleanup": {
                "state": if bot.status == BotStatus::Deleted { "done" } else { "pending" },
                "error": null,
            },
        });
    }
    json!({
        "id": bot.id,
        "name": bot.name,
        "role": bot.role,
        "conversation_id": bot.conversation_id,
        "created_at": bot.created_at,
        "deleted_at": null,
        "selected_provider_id": bot.selected_provider_id,
        "activity": activity_json(bot),
        "cleanup": null,
    })
}

/// Project turn records into the sidebar activity. No separate state.
pub fn activity_json(bot: &BotRecord) -> Value {
    let phase = match (bot.cancel_pending, bot.active_turn_status.as_deref()) {
        (true, _) | (_, Some("cancelling")) => "cancelling",
        (_, Some(_)) => "working",
        (_, None) if bot.queued_count > 0 => "queued",
        _ => "idle",
    };
    json!({
        "phase": phase,
        "active_turn_id": bot.active_turn_id,
        "queued_count": bot.queued_count,
        "running_job_count": 0,
        "error": null,
    })
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    include_deleting: bool,
}

async fn list(
    State(state): State<Arc<AppState>>,
    query: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request("invalid_query", "include_deleting must be true or false.")
    })?;
    let listing = bots::list(&state.db, query.include_deleting).await?;
    Ok(Json(json!({
        "default_bot_id": listing.default_bot_id,
        "bots": listing.bots.iter().map(bot_json).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBot {
    request_id: Uuid,
    name: String,
    role: String,
}

async fn create(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<CreateBot>,
) -> Result<Response, ApiError> {
    let invalid = |err: crate::app::bots::InvalidBot| {
        let error = if err.is_size_limit() {
            ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", err.to_string())
        } else {
            ApiError::bad_request("invalid_input", err.to_string())
        };
        error.with_request_id(body.request_id)
    };
    let name = BotName::parse(&body.name).map_err(invalid)?;
    let role = BotRole::parse(&body.role).map_err(invalid)?;
    let created = bots::create(&state.db, body.request_id, &name, &role)
        .await
        .map_err(|err| ApiError::from(err).with_request_id(body.request_id))?;
    Ok(match created {
        Created::New(bot) => {
            let location = HeaderValue::from_str(&format!("/v1/bots/{}", bot.id))
                .expect("UUID path is a valid header");
            (
                StatusCode::CREATED,
                [(header::LOCATION, location)],
                Json(json!({ "bot": bot_json(&bot) })),
            )
                .into_response()
        }
        Created::Existing(bot) => {
            (StatusCode::OK, Json(json!({ "bot": bot_json(&bot) }))).into_response()
        }
    })
}

async fn show(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let bot = bots::find(&state.db, bot_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({ "bot": bot_json(&bot) })))
}

async fn delete(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    let bot = bots::delete(&state.db, bot_id).await?;
    let status = match bot.status {
        BotStatus::Deleted => StatusCode::OK,
        _ => StatusCode::ACCEPTED,
    };
    Ok((status, Json(json!({ "bot": bot_json(&bot) }))).into_response())
}
