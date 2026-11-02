//! Owner routes for a bot's computer: its state, an explicit stop that
//! prevents implicit wake, and resume (which starts nothing by itself).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiPath};
use super::turns::bot_deleted;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/computer", get(show))
        .route("/bots/{bot_id}/computer/stop", post(stop))
        .route("/bots/{bot_id}/computer/resume", post(resume))
}

fn manager(state: &AppState) -> Result<&Arc<crate::computers::ComputerManager>, ApiError> {
    state.computers.as_ref().ok_or_else(|| {
        ApiError::unavailable(
            "computers_unavailable",
            "Bot computers are not available on this host.",
        )
    })
}

async fn require_bot(state: &AppState, bot_id: Uuid, active: bool) -> Result<(), ApiError> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bots WHERE id = $1")
        .bind(bot_id)
        .fetch_optional(&state.db)
        .await?;
    match status.as_deref() {
        None => Err(ApiError::not_found()),
        Some("active") => Ok(()),
        Some(_) if active => Err(bot_deleted()),
        Some(_) => Ok(()),
    }
}

async fn show(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_bot(&state, bot_id, false).await?;
    let row: Option<(
        String,
        String,
        i32,
        i32,
        String,
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
        i64,
    )> = sqlx::query_as(
        "SELECT runtime_name, image, cpus, memory_mib, desired_state, observed_state,
                    last_used_at, last_error,
                    (SELECT count(*) FROM computer_leases l
                     WHERE l.bot_id = c.bot_id AND l.released_at IS NULL)
             FROM bot_computers c WHERE bot_id = $1",
    )
    .bind(bot_id)
    .fetch_optional(&state.db)
    .await?;
    let Some((name, image, cpus, memory, desired, observed, last_used, error, leases)) = row else {
        return Ok(Json(
            json!({ "bot_id": bot_id, "state": "absent", "desired_state": "auto" }),
        ));
    };
    Ok(Json(json!({
        "bot_id": bot_id,
        "runtime_name": name,
        "image": image,
        "cpus": cpus,
        "memory_mib": memory,
        "desired_state": desired,
        "state": observed,
        "active_leases": leases,
        "last_used_at": last_used,
        "last_error": error,
    })))
}

async fn stop(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_bot(&state, bot_id, false).await?;
    manager(&state)?
        .owner_stop(bot_id)
        .await
        .map_err(|crate::computers::ComputerError::Db(err)| ApiError::from(err))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn resume(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_bot(&state, bot_id, true).await?;
    manager(&state)?
        .owner_resume(bot_id)
        .await
        .map_err(|crate::computers::ComputerError::Db(err)| ApiError::from(err))?;
    Ok(StatusCode::NO_CONTENT)
}
