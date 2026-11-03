//! The welcome screen's one step: name the main bot and choose its
//! provider and model, which also become the defaults new bots start with.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::error::{ApiError, ApiJson};
use super::settings::{ProfileChoice, choice_json, validate_choice};
use crate::app::bots::BotName;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/onboarding", post(complete))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Onboarding {
    name: String,
    profile: ProfileChoice,
}

/// Whether the welcome screen is still to be shown.
pub async fn required(state: &AppState) -> Result<bool, sqlx::Error> {
    let done: Option<bool> =
        sqlx::query_scalar("SELECT onboarded_at IS NOT NULL FROM installation")
            .fetch_optional(&state.db)
            .await?;
    Ok(done == Some(false))
}

/// Name the main bot, give it this provider and model, and make them the
/// defaults. Completion is recorded last, so a failed attempt is retried
/// whole; repeating it after success changes the same things again.
async fn complete(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<Onboarding>,
) -> Result<Json<Value>, ApiError> {
    let name = BotName::parse(&body.name)
        .map_err(|err| ApiError::bad_request("invalid_input", err.to_string()))?;
    let provider_id = validate_choice(&state, &body.profile)?;
    let profile = body.profile.profile();
    let main: Option<uuid::Uuid> = sqlx::query_scalar(
        "SELECT i.default_bot_id FROM installation i
         JOIN bots b ON b.id = i.default_bot_id AND b.status = 'active'",
    )
    .fetch_optional(&state.db)
    .await?;
    let main = main.ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "no_main_bot",
            "This installation has no main bot; run else-api setup.",
        )
    })?;
    super::settings::save_defaults(&state, provider_id, &profile).await?;
    crate::db::bot_profiles::put(&state.db, main, provider_id, &profile).await?;
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE bots SET name = $2 WHERE id = $1")
        .bind(main)
        .bind(name.as_str())
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE installation SET onboarded_at = coalesce(onboarded_at, now())")
        .execute(&mut *tx)
        .await?;
    // The renamed bot reaches every client's list.
    crate::db::events::notify(&mut tx, main).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "main_bot_id": main,
        "name": name.as_str(),
        "profile": choice_json(provider_id, &profile),
    })))
}
