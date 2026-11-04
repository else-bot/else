//! Owner bot management routes.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use crate::app::bots::{Avatar, BotName};
use crate::db::bots::{self, BotError, BotStatus, Created};
use crate::db::reads::{self, ReadCommand, ReadError};
use crate::views::*;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots", get(list).post(create))
        .route("/bots/{bot_id}", get(show).patch(update).delete(delete))
        .route("/bots/{bot_id}/profile", get(show_profile).put(set_profile))
        .route("/bots/{bot_id}/read", post(mark_read))
}

impl From<ReadError> for ApiError {
    fn from(err: ReadError) -> Self {
        match err {
            ReadError::NotFound => ApiError::not_found(),
            ReadError::Deleted => super::turns::bot_deleted(),
            ReadError::InvalidPosition => ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_position",
                "The position is not within one of the bot's replies.",
            ),
            ReadError::Db(err) => err.into(),
        }
    }
}

/// Move the owner's read position forward, or set or clear the explicit
/// unread flag. Repeating a command changes nothing further.
async fn mark_read(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    ApiJson(command): ApiJson<ReadCommand>,
) -> Result<Json<Value>, ApiError> {
    let bot = reads::apply(&state.db, bot_id, command).await?;
    Ok(Json(json!({ "bot": bot_json(&bot) })))
}

impl From<BotError> for ApiError {
    fn from(err: BotError) -> Self {
        match err {
            BotError::NotFound => ApiError::not_found(),
            BotError::CategoryNotFound => ApiError::new(
                StatusCode::NOT_FOUND,
                "category_not_found",
                "No such category.",
            ),
            BotError::MainBot => ApiError::new(
                StatusCode::CONFLICT,
                "main_bot",
                "The main bot cannot be deleted.",
            ),
            BotError::Deleted => super::turns::bot_deleted(),
            BotError::RequestConflict => ApiError::new(
                StatusCode::CONFLICT,
                "request_conflict",
                "This request ID was already used with different input.",
            ),
            BotError::Refused(err) => {
                ApiError::new(StatusCode::FORBIDDEN, "not_allowed", err.to_string())
            }
            BotError::Db(err) => err.into(),
        }
    }
}

fn invalid_bot(err: crate::app::bots::InvalidBot) -> ApiError {
    ApiError::bad_request("invalid_input", err.to_string())
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
        "categories": listing.categories,
        "bots": listing.bots.iter().map(bot_json).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBot {
    request_id: Uuid,
    name: String,
    /// The bot's own provider and model; without one it uses the defaults.
    #[serde(default)]
    profile: Option<super::settings::ProfileChoice>,
    #[serde(default)]
    category_id: Option<Uuid>,
}

async fn create(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<CreateBot>,
) -> Result<Response, ApiError> {
    let name = BotName::parse(&body.name)
        .map_err(|err| invalid_bot(err).with_request_id(body.request_id))?;
    let choice = match &body.profile {
        Some(choice) => Some((
            super::settings::validate_choice(&state, choice)
                .map_err(|err| err.with_request_id(body.request_id))?,
            choice.profile(),
        )),
        None => None,
    };
    let created = bots::create(
        &state.db,
        body.request_id,
        bots::NewBot {
            name: &name,
            profile: choice
                .as_ref()
                .map(|(provider, profile)| (*provider, profile)),
            category_id: body.category_id,
        },
    )
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateBot {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar: Option<Avatar>,
}

async fn update(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<UpdateBot>,
) -> Result<Json<Value>, ApiError> {
    if body.name.is_none() && body.avatar.is_none() {
        return Err(ApiError::bad_request(
            "invalid_input",
            "Send a name, an avatar, or both.",
        ));
    }
    let name = body
        .name
        .as_deref()
        .map(BotName::parse)
        .transpose()
        .map_err(invalid_bot)?;
    let patch = bots::Patch {
        name: name.as_ref(),
        avatar: body.avatar,
    };
    let bot = bots::update(&state.db, bot_id, patch).await?;
    Ok(Json(json!({ "bot": bot_json(&bot) })))
}

async fn delete(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    let deleted = bots::delete(&state.db, bot_id).await?;
    state.scheduler.grants().revoke_bot(bot_id).await;
    if let Some(turn_id) = deleted.interrupt {
        state.scheduler.interrupt(turn_id).await;
    }
    let bot = deleted.bot;
    let status = match bot.status {
        BotStatus::Deleted => StatusCode::OK,
        _ => StatusCode::ACCEPTED,
    };
    Ok((status, Json(json!({ "bot": bot_json(&bot) }))).into_response())
}

impl From<crate::db::bot_profiles::BotProfileError> for ApiError {
    fn from(err: crate::db::bot_profiles::BotProfileError) -> Self {
        match err {
            crate::db::bot_profiles::BotProfileError::NotFound => ApiError::not_found(),
            crate::db::bot_profiles::BotProfileError::Db(err) => err.into(),
        }
    }
}

/// The bot's provider and model: its own (`own: true`), else the defaults
/// it uses (`null` when there are none yet).
async fn show_profile(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    bots::find(&state.db, bot_id)
        .await?
        .filter(|bot| bot.status == BotStatus::Active)
        .ok_or_else(ApiError::not_found)?;
    if let Some(own) = crate::db::bot_profiles::get(&state.db, bot_id).await? {
        let mut profile = super::settings::choice_json(&own.provider_id, &own.profile);
        profile["own"] = json!(true);
        profile["revision"] = json!(own.revision);
        return Ok(Json(json!({ "profile": profile })));
    }
    let mut defaults = super::settings::defaults(&state).await?;
    if let Some(profile) = defaults.as_mut() {
        profile["own"] = json!(false);
    }
    Ok(Json(json!({ "profile": defaults })))
}

async fn set_profile(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<super::settings::ProfileChoice>,
) -> Result<Json<Value>, ApiError> {
    let provider_id = super::settings::validate_choice(&state, &body)?;
    let saved =
        crate::db::bot_profiles::put(&state.db, bot_id, provider_id, &body.profile()).await?;
    let mut profile = super::settings::choice_json(&saved.provider_id, &saved.profile);
    profile["own"] = json!(true);
    profile["revision"] = json!(saved.revision);
    Ok(Json(json!({ "profile": profile })))
}
