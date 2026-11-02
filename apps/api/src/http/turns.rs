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
        .route("/bots/{bot_id}/cancel", post(cancel))
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
    let default_id = registry.default_id();
    let requested = body.provider_id.as_deref();
    let mut request = SendRequest {
        bot_id,
        request_id,
        text: &body.text,
        requested_provider: requested,
        provider_id: requested.or(default_id).unwrap_or(""),
        uses_profile: false,
        max_queued: MAX_QUEUED_TURNS,
    };
    // A matching retry returns its saved result even if the provider has
    // since become unavailable; provider checks apply to new messages only.
    let accepted = match turns::existing_send(&state.db, &request)
        .await
        .map_err(|err| with_id(err.into()))?
    {
        Some(existing) => existing,
        None => {
            if request.provider_id.is_empty() {
                return Err(with_id(ApiError::unavailable(
                    "provider_unavailable",
                    "No provider is available.",
                )));
            }
            let provider = registry.get(request.provider_id).ok_or_else(|| {
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
            request.provider_id = provider.id();
            request.uses_profile = provider.uses_profile();
            turns::accept(&state.db, request)
                .await
                .map_err(|err| with_id(err.into()))?
        }
    };
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelBody {
    request_id: Uuid,
}

/// Stop work: cancel everything accepted for this bot so far. `202` while
/// the provider is still stopping, `200` once confirmed. New messages are
/// rejected with `409 cancellation_pending` until then.
async fn cancel(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<CancelBody>,
) -> Result<Response, ApiError> {
    use crate::db::stop::{self, StopError};
    let receipt = stop::stop_work(&state.db, bot_id, body.request_id)
        .await
        .map_err(|err| {
            match err {
                StopError::Bot(err) => ApiError::from(err),
                StopError::Deleted => bot_deleted(),
            }
            .with_request_id(body.request_id)
        })?;
    // A retried stop returns its old receipt and must not touch newer work.
    if receipt.created {
        state.scheduler.grants().revoke_bot(bot_id);
    }
    if let Some(turn_id) = receipt.interrupt {
        state.scheduler.interrupt(turn_id);
    }
    let status = if receipt.state == "done" {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    Ok((
        status,
        Json(json!({
            "request_id": receipt.request_id,
            "bot_id": receipt.bot_id,
            "state": receipt.state,
        })),
    )
        .into_response())
}
