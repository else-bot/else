//! Owner-only provider profile settings. Task grants never reach these
//! routes; prompt text and model output cannot change settings.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use crate::app::providers::Availability;
use crate::db::profiles::{self, ProfileError, ProfilePatch, ProfileRecord};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/providers/{provider_id}", get(show).patch(update))
        .route("/settings/defaults", get(show_defaults).put(set_defaults))
        .route("/settings/server", put(name_server))
}

/// A provider and a complete profile for it, as the owner's pickers send.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileChoice {
    pub provider_id: String,
    pub model: String,
    pub reasoning_effort: String,
    pub service_tier: String,
}

impl ProfileChoice {
    pub fn profile(&self) -> crate::app::providers::Profile {
        crate::app::providers::Profile {
            model: self.model.clone(),
            reasoning_effort: self.reasoning_effort.clone(),
            service_tier: self.service_tier.clone(),
        }
    }
}

/// Check a choice with the provider's own adapter: it must exist, take a
/// profile, and apply this one exactly. Returns the provider's ID.
pub fn validate_choice(state: &AppState, choice: &ProfileChoice) -> Result<&'static str, ApiError> {
    let provider = state
        .scheduler
        .registry()
        .get(&choice.provider_id)
        .filter(|p| p.uses_profile())
        .ok_or_else(|| ApiError::bad_request("unknown_provider", "Unknown provider."))?;
    provider
        .validate_profile(&choice.profile())
        .map_err(|reason| ApiError::bad_request("invalid_profile", reason))?;
    Ok(provider.id())
}

pub fn choice_json(provider_id: &str, profile: &crate::app::providers::Profile) -> Value {
    json!({
        "provider_id": provider_id,
        "model": profile.model,
        "reasoning_effort": profile.reasoning_effort,
        "service_tier": profile.service_tier,
    })
}

/// The defaults new bots start with: the installation's default provider
/// (else the first available one that takes a profile) and that provider's saved profile.
pub async fn defaults(state: &AppState) -> Result<Option<Value>, ApiError> {
    let registry = state.scheduler.registry();
    let mut conn = state.db.acquire().await?;
    let chosen = crate::db::bot_profiles::default_provider(&mut conn)
        .await?
        .and_then(|id| registry.get(&id).map(|p| p.id()));
    let first = registry
        .all()
        .iter()
        .find(|p| p.uses_profile() && p.availability() == Availability::Available)
        .map(|p| p.id());
    let Some(provider_id) = chosen.or(first) else {
        return Ok(None);
    };
    Ok(profiles::get(&state.db, provider_id)
        .await?
        .map(|record| choice_json(provider_id, &record.profile())))
}

async fn show_defaults(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({ "defaults": defaults(&state).await? })))
}

async fn set_defaults(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<ProfileChoice>,
) -> Result<Json<Value>, ApiError> {
    let provider_id = validate_choice(&state, &body)?;
    let profile = body.profile();
    save_defaults(&state, provider_id, &profile).await?;
    Ok(Json(
        json!({ "defaults": choice_json(provider_id, &profile) }),
    ))
}

/// Make `provider_id` the default provider with this (validated) profile.
pub async fn save_defaults(
    state: &AppState,
    provider_id: &str,
    profile: &crate::app::providers::Profile,
) -> Result<(), ApiError> {
    let created = profiles::create(&state.db, provider_id, profile, |_| Ok(()))
        .await
        .map_err(profile_error)?;
    if created.is_none() {
        let patch = ProfilePatch {
            model: Some(profile.model.clone()),
            reasoning_effort: Some(profile.reasoning_effort.clone()),
            service_tier: Some(profile.service_tier.clone()),
        };
        profiles::update(&state.db, provider_id, patch, None, |_| Ok(()))
            .await
            .map_err(profile_error)?;
    }
    let mut conn = state.db.acquire().await?;
    crate::db::bot_profiles::set_default_provider(&mut conn, provider_id).await?;
    Ok(())
}

fn profile_error(err: ProfileError) -> ApiError {
    match err {
        ProfileError::NotFound => ApiError::not_found(),
        ProfileError::Invalid(reason) => ApiError::bad_request("invalid_profile", reason),
        ProfileError::RevisionMismatch(_) => ApiError::new(
            StatusCode::CONFLICT,
            "revision_conflict",
            "The profile changed. Reload it and try again.",
        ),
        ProfileError::Db(err) => err.into(),
    }
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerBody {
    name: String,
    /// Keep a name another desktop already gave the server.
    #[serde(default)]
    only_if_unnamed: bool,
}

/// Name this server; every desktop shows the name from its capabilities.
async fn name_server(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<ServerBody>,
) -> Result<Json<Value>, ApiError> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "invalid_input",
            "Give the server a name of 1 to 80 characters, without control characters.",
        ));
    }
    let kept =
        crate::db::installation::set_server_name(&state.db, name, body.only_if_unnamed).await?;
    Ok(Json(json!({ "server": { "name": kept } })))
}
