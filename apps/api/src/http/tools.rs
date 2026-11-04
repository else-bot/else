//! The task-scoped tool API used by provider sessions. Every handler
//! derives bot and conversation from the grant and rechecks current state.

use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiPath};
use crate::db::turns::{ITEM_COLUMNS, ItemRecord};
use crate::grants::{Grant, Tool};
use crate::views::item_json;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/history", get(history))
        .route("/items/{item_id}", get(item))
        .route(
            "/mcp",
            axum::routing::post(super::mcp::endpoint).get(super::mcp::no_stream),
        )
}

fn revoked() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_task_grant",
        "The task grant is no longer valid.",
    )
}

/// The grant's turn must still be running and its bot active.
pub(crate) async fn check(state: &AppState, grant: &Grant, tool: Tool) -> Result<(), ApiError> {
    if !grant.allows(tool) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "tool_not_allowed",
            "This task grant does not allow that tool.",
        ));
    }
    let live: Option<bool> = sqlx::query_scalar(
        "SELECT b.status = 'active' AND t.status IN ('sending', 'running')
         FROM turns t JOIN bots b ON b.id = t.bot_id WHERE t.id = ?1 AND t.bot_id = ?2",
    )
    .bind(grant.turn_id)
    .bind(grant.bot_id)
    .fetch_optional(&state.db)
    .await?;
    if live != Some(true) {
        state.scheduler.grants().revoke_turn(grant.turn_id).await;
        return Err(revoked());
    }
    if state.scheduler.grants().is_parked(grant.turn_id).await {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "attempt_parked",
            "This attempt is waiting on delegated work or the owner; end it now. The answer arrives in a continuation.",
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    before_seq: Option<i64>,
    limit: Option<i64>,
}

async fn history(
    State(state): State<Arc<AppState>>,
    Extension(grant): Extension<Grant>,
    query: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query
        .map_err(|_| ApiError::bad_request("invalid_query", "Use before_seq and limit only."))?;
    check(&state, &grant, Tool::ReadHistory).await?;
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let items: Vec<ItemRecord> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN turns t ON t.id = i.turn_id
         WHERE i.conversation_id = ?1 AND (?2 IS NULL OR i.seq < ?2)
         ORDER BY i.seq DESC LIMIT ?3"
    )))
    .bind(grant.conversation_id)
    .bind(query.before_seq)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        json!({ "items": items.iter().rev().map(item_json).collect::<Vec<_>>() }),
    ))
}

async fn item(
    State(state): State<Arc<AppState>>,
    Extension(grant): Extension<Grant>,
    ApiPath(item_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    check(&state, &grant, Tool::ReadHistory).await?;
    // A foreign object is indistinguishable from a missing one.
    let item: Option<ItemRecord> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN turns t ON t.id = i.turn_id
         WHERE i.id = ?1 AND i.conversation_id = ?2"
    )))
    .bind(item_id)
    .bind(grant.conversation_id)
    .fetch_optional(&state.db)
    .await?;
    let item = item.ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({ "item": item_json(&item) })))
}
