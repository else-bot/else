//! Owner-only provider profile settings. Task grants never reach these
//! routes; prompt text and model output cannot change settings.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use crate::db::profiles::{self, ProfileError, ProfilePatch, ProfileRecord};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/settings/providers/{provider_id}", get(show).patch(update))
}

fn profile_json(record: &ProfileRecord) -> Value {
    json!({
        "provider_id": record.provider_id,
        "model": record.model,
        "reasoning_effort": record.reasoning_effort,
        "service_tier": record.service_tier,
        "revision": record.revision,
        "updated_at": record.updated_at,
    })
}

async fn show(
    State(state): State<Arc<AppState>>,
    ApiPath(provider_id): ApiPath<String>,
) -> Result<Json<Value>, ApiError> {
    let record = profiles::get(&state.db, &provider_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({ "profile": profile_json(&record) })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateBody {
    model: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
    expected_revision: Option<i64>,
}

async fn update(
    State(state): State<Arc<AppState>>,
    ApiPath(provider_id): ApiPath<String>,
    ApiJson(body): ApiJson<UpdateBody>,
) -> Result<Json<Value>, ApiError> {
    let saved = profiles::get(&state.db, &provider_id).await?.is_some();
    let adapter = state.scheduler.registry().get(&provider_id);
    if !saved && !adapter.is_some_and(|p| p.uses_profile()) {
        return Err(ApiError::not_found());
    }
    // Only the provider's adapter can say which exact values it applies.
    let provider = state
        .scheduler
        .registry()
        .get(&provider_id)
        .filter(|p| p.uses_profile())
        .ok_or_else(|| {
            ApiError::unavailable(
                "provider_unavailable",
                "This provider's adapter is not enabled, so the profile cannot be validated.",
            )
        })?
        .clone();
    if !saved {
        // A first profile is set explicitly and completely: nothing is
        // inherited from a CLI default.
        let (Some(model), Some(reasoning_effort), Some(service_tier)) =
            (&body.model, &body.reasoning_effort, &body.service_tier)
        else {
            return Err(ApiError::bad_request(
                "profile_required",
                "This provider has no saved profile yet. Send model, reasoning_effort, and \
                 service_tier to create it.",
            ));
        };
        let profile = crate::app::providers::Profile {
            model: model.clone(),
            reasoning_effort: reasoning_effort.clone(),
            service_tier: service_tier.clone(),
        };
        let created = profiles::create(&state.db, &provider_id, &profile, |p| {
            provider.validate_profile(p)
        })
        .await
        .map_err(|err| match err {
            ProfileError::Invalid(reason) => ApiError::bad_request("invalid_profile", reason),
            ProfileError::Db(err) => err.into(),
            _ => ApiError::not_found(),
        })?;
        if let Some(record) = created {
            return Ok(Json(json!({ "profile": profile_json(&record) })));
        }
    }
    let patch = ProfilePatch {
        model: body.model,
        reasoning_effort: body.reasoning_effort,
        service_tier: body.service_tier,
    };
    let record = profiles::update(
        &state.db,
        &provider_id,
        patch,
        body.expected_revision,
        |p| provider.validate_profile(p),
    )
    .await
    .map_err(|err| match err {
        ProfileError::NotFound => ApiError::not_found(),
        ProfileError::Invalid(reason) => ApiError::bad_request("invalid_profile", reason),
        ProfileError::RevisionMismatch(_) => ApiError::new(
            StatusCode::CONFLICT,
            "revision_conflict",
            "The profile changed. Reload it and try again.",
        ),
        ProfileError::Db(err) => err.into(),
    })?;
    Ok(Json(json!({ "profile": profile_json(&record) })))
}
