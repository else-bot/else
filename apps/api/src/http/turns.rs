//! Sending owner messages and resolving lost send responses.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use crate::app::messages::{InvalidMessage, MAX_QUEUED_TURNS, validate_text};
use crate::app::providers::Availability;
use crate::db::turns::{self, AcceptError, SendRequest};
use crate::views::*;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/turns", post(send))
        .route(
            "/bots/{bot_id}/turns/by-request/{request_id}",
            get(by_request),
        )
}

pub fn bot_deleted() -> ApiError {
    ApiError::new(
        StatusCode::GONE,
        "bot_deleted",
        "This bot has been deleted.",
    )
}

impl From<AcceptError> for ApiError {
    fn from(err: AcceptError) -> Self {
        match err {
            AcceptError::NotFound => ApiError::not_found(),
            AcceptError::Deleted => bot_deleted(),
            AcceptError::RequestConflict => ApiError::new(
                StatusCode::CONFLICT,
                "request_conflict",
                "This request ID was already used with different input.",
            ),
            AcceptError::QueueFull => ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "queue_full",
                "Too many messages are waiting for this bot. Try again shortly.",
            )
            .retry_after(5),
            AcceptError::CancellationPending => ApiError::new(
                StatusCode::CONFLICT,
                "cancellation_pending",
                "Work is still stopping. Try again when it has stopped.",
            )
            .retryable(),
            AcceptError::ProfileMissing(provider) => ApiError::unavailable(
                "provider_unavailable",
                format!("Provider {provider} has no saved execution profile; run else-api setup."),
            ),
            AcceptError::Db(err) => err.into(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SendBody {
    request_id: Uuid,
    text: String,
    #[serde(default)]
    provider_id: Option<String>,
}

async fn send(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<SendBody>,
) -> Result<Response, ApiError> {
    let request_id = body.request_id;
    let with_id = |err: ApiError| err.with_request_id(request_id);
    validate_text(&body.text).map_err(|err| {
        with_id(match err {
            InvalidMessage::TooLarge => {
                ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", err.to_string())
            }
            _ => ApiError::bad_request("invalid_input", err.to_string()),
        })
    })?;
    let registry = state.scheduler.registry();
    let provider_id = match body.provider_id.as_deref() {
        Some(id) => id,
        None => registry.default_id().ok_or_else(|| {
            with_id(ApiError::unavailable(
                "provider_unavailable",
                "No provider is available.",
            ))
        })?,
    };
    let provider = registry.get(provider_id).ok_or_else(|| {
        with_id(ApiError::bad_request(
            "unknown_provider",
            "Unknown provider.",
        ))
    })?;
    if let Availability::Unavailable(reason) = provider.availability() {
        return Err(with_id(ApiError::unavailable(
            "provider_unavailable",
            reason,
        )));
    }
    let accepted = turns::accept(
        &state.db,
        SendRequest {
            bot_id,
            request_id,
            text: &body.text,
            requested_provider: body.provider_id.as_deref(),
            provider_id: provider.id(),
            uses_profile: provider.uses_profile(),
            max_queued: MAX_QUEUED_TURNS,
        },
    )
    .await
    .map_err(|err| with_id(err.into()))?;
    if accepted.created {
        state.scheduler.wake();
    }
    let status = if accepted.created {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((
        status,
        Json(json!({ "item": item_json(&accepted.item), "turn": turn_json(&accepted.turn) })),
    )
        .into_response())
}

async fn by_request(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, request_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let bot = crate::db::bots::find(&state.db, bot_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if bot.status != crate::db::bots::BotStatus::Active {
        return Err(bot_deleted());
    }
    let (turn, item) = turns::find_by_request(&state.db, bot_id, request_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(
        json!({ "item": item_json(&item), "turn": turn_json(&turn) }),
    ))
}
