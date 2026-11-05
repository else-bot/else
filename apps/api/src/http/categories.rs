//! Owner routes for sidebar categories. Everything under `/v1`
//! needs the owner token, so a task grant can never change them.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use super::turns::bot_deleted;
use crate::db::categories::{self, CategoryError, Created};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/categories", get(list).post(create))
        .route("/categories/{category_id}", patch(rename).delete(remove))
        .route("/bots/{bot_id}/category", patch(assign))
}

impl From<CategoryError> for ApiError {
    fn from(err: CategoryError) -> Self {
        match err {
            CategoryError::InvalidName => ApiError::bad_request("invalid_name", err.to_string()),
            CategoryError::Duplicate => {
                ApiError::new(StatusCode::CONFLICT, "duplicate_category", err.to_string())
            }
            CategoryError::RequestConflict => ApiError::new(
                StatusCode::CONFLICT,
                "request_conflict",
                "This request ID was already used with different input.",
            ),
            CategoryError::Gone => {
                ApiError::new(StatusCode::GONE, "category_deleted", err.to_string())
            }
            CategoryError::NotFound => ApiError::new(
                StatusCode::NOT_FOUND,
                "category_not_found",
                "No such category.",
            ),
            CategoryError::BotNotFound => ApiError::not_found(),
            CategoryError::BotDeleted => bot_deleted(),
            CategoryError::Db(err) => err.into(),
        }
    }
}

async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({ "categories": categories::list(&state.db).await? }),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    request_id: Uuid,
    name: String,
}

async fn create(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<CreateBody>,
) -> Result<Response, ApiError> {
    match categories::create(&state.db, body.request_id, &body.name).await {
        Ok(Created::New(category)) => {
            let location = format!("/v1/categories/{}", category.id);
            Ok((
                StatusCode::CREATED,
                [(header::LOCATION, location)],
                Json(json!({ "category": category })),
            )
                .into_response())
        }
        Ok(Created::Existing(category)) => {
            Ok(Json(json!({ "category": category })).into_response())
        }
        Err(err) => Err(ApiError::from(err).with_request_id(body.request_id)),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBody {
    name: String,
}

async fn rename(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<RenameBody>,
) -> Result<Json<Value>, ApiError> {
    let category = categories::rename(&state.db, id, &body.name).await?;
    Ok(Json(json!({ "category": category })))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let unassigned = categories::delete(&state.db, id).await?;
    Ok(Json(
        json!({ "deleted": id, "unassigned_bots": unassigned }),
    ))
}

async fn assign(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let invalid = || {
        ApiError::bad_request(
            "invalid_input",
            "Send {\"category_id\": \"<uuid>\"} or {\"category_id\": null}.",
        )
    };
    let object = body.as_object().ok_or_else(invalid)?;
    if object.len() != 1 {
        return Err(invalid());
    }
    let category = match object.get("category_id") {
        Some(Value::Null) => None,
        Some(Value::String(id)) => Some(id.parse::<Uuid>().map_err(|_| invalid())?),
        _ => return Err(invalid()),
    };
    categories::assign(&state.db, bot_id, category).await?;
    let bot = crate::db::bots::find(&state.db, bot_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({ "bot": crate::views::bot_json(&bot) })))
}
