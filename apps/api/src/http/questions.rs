//! Owner routes for a bot's questions and standing permissions. Only the
//! owner token reaches them; a task grant never answers a question.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use super::turns::active_bot;
use crate::app::questions::Answer;
use crate::db::owner_questions::{self, SettleError};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/bots/{bot_id}/questions/{question_id}/answer",
            post(answer),
        )
        .route("/bots/{bot_id}/permissions", get(permissions))
        .route(
            "/bots/{bot_id}/permissions/{scope}",
            axum::routing::delete(revoke),
        )
        .route(
            "/bots/{bot_id}/permissions/routines/{routine_id}",
            axum::routing::delete(revoke_routine),
        )
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum AnswerBody {
    Single { choice: String },
    Multiple { choices: Vec<String> },
}

fn settle_error(err: SettleError) -> ApiError {
    match err {
        SettleError::NotFound => ApiError::not_found(),
        SettleError::Settled(status) => ApiError::new(
            StatusCode::CONFLICT,
            "question_settled",
            format!("This question was already {}.", status.as_str()),
        ),
        SettleError::InvalidChoice => ApiError::bad_request(
            "invalid_choice",
            "Choose valid options for this question's selection mode.",
        ),
        SettleError::RoutineChanged => ApiError::new(
            StatusCode::CONFLICT,
            "routine_changed",
            SettleError::RoutineChanged.to_string(),
        ),
        SettleError::Db(err) => err.into(),
    }
}

/// The owner submits a selection. A retry of the same answer succeeds without
/// changing anything; a stale card answering a settled question gets 409.
async fn answer(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, question_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<AnswerBody>,
) -> Result<Json<Value>, ApiError> {
    let answer = match body {
        AnswerBody::Single { choice } => Answer::Single(choice),
        AnswerBody::Multiple { choices } => Answer::Multiple(choices),
    };
    let settled = owner_questions::answer(&state.db, bot_id, question_id, &answer)
        .await
        .map_err(settle_error)?;
    if settled.requeued {
        state.scheduler.wake();
    }
    Ok(Json(
        json!({ "question": settled.question, "requeued": settled.requeued }),
    ))
}

async fn permissions(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    active_bot(&state, bot_id).await?;
    let (permissions, routine_grants) = tokio::join!(
        owner_questions::permissions(&state.db, bot_id),
        owner_questions::routine_grants(&state.db, bot_id),
    );
    Ok(Json(
        json!({ "permissions": permissions?, "routine_grants": routine_grants? }),
    ))
}

async fn revoke_routine(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, routine_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    active_bot(&state, bot_id).await?;
    if owner_questions::revoke_routine_grant(&state.db, bot_id, routine_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, scope)): ApiPath<(Uuid, String)>,
) -> Result<StatusCode, ApiError> {
    active_bot(&state, bot_id).await?;
    if owner_questions::revoke_permission(&state.db, bot_id, &scope).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}
