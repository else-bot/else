//! Owner routes for a bot's computer: its state, an explicit stop that
//! prevents implicit wake, resume (which starts nothing by itself), and an
//! upgrade to the configured image that keeps the computer's data.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::computers::ComputerError;
use crate::computers::upgrade::UpgradeStatus;

use super::AppState;
use super::error::{ApiError, ApiPath};
use super::turns::bot_deleted;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/computer", get(show))
        .route("/bots/{bot_id}/computer/stop", post(stop))
        .route("/bots/{bot_id}/computer/resume", post(resume))
        .route("/bots/{bot_id}/computer/upgrade", post(upgrade))
        .route("/bots/{bot_id}/desktop", get(desktop).delete(close_desktop))
        .route(
            "/bots/{bot_id}/desktop/stream",
            get(super::desktop_stream::stream),
        )
}

pub(super) fn manager(
    state: &AppState,
) -> Result<&Arc<crate::computers::ComputerManager>, ApiError> {
    state.computers.as_ref().ok_or_else(|| {
        ApiError::unavailable(
            "computers_unavailable",
            "Bot computers are not available on this host.",
        )
    })
}

pub(super) async fn require_bot(
    state: &AppState,
    bot_id: Uuid,
    active: bool,
) -> Result<(), ApiError> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
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
    #[derive(sqlx::FromRow)]
    struct ComputerRow {
        runtime_name: String,
        image: String,
        cpus: i32,
        memory_mib: i32,
        desired_state: String,
        observed_state: String,
        last_used_at: Option<chrono::DateTime<chrono::Utc>>,
        last_error: Option<String>,
        leases: i64,
        has_desktop: bool,
    }
    let row: Option<ComputerRow> = sqlx::query_as(
        "SELECT runtime_name, image, cpus, memory_mib, desired_state, observed_state,
                    last_used_at, last_error,
                    (SELECT count(*) FROM computer_leases l
                     WHERE l.bot_id = c.bot_id AND l.released_at IS NULL) AS leases,
                    desktop_port IS NOT NULL OR observed_state IN ('absent', 'creating') AS has_desktop
             FROM bot_computers c WHERE bot_id = ?1",
    )
    .bind(bot_id)
    .fetch_optional(&state.db)
    .await?;
    let desktop = state
        .computers
        .as_ref()
        .is_some_and(|computers| computers.config().desktop);
    let (latest_image, upgrade) = match &state.computers {
        Some(computers) => (
            Some(computers.config().image.clone()),
            match computers.upgrade_status(bot_id).await {
                None => last_failed_upgrade(&state, bot_id).await?,
                Some(UpgradeStatus::Running) => json!({ "state": "running" }),
                Some(UpgradeStatus::Failed(error)) => json!({ "state": "failed", "error": error }),
            },
        ),
        None => (None, Value::Null),
    };
    let Some(ComputerRow {
        runtime_name: name,
        image,
        cpus,
        memory_mib: memory,
        desired_state: desired,
        observed_state: observed,
        last_used_at: last_used,
        last_error: error,
        leases,
        has_desktop,
    }) = row
    else {
        return Ok(Json(json!({
            "bot_id": bot_id,
            "state": "absent",
            "desired_state": "auto",
            "desktop_available": desktop,
        })));
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
        "desktop_available": desktop && has_desktop,
        "latest_image": latest_image,
        "upgrade": upgrade,
    })))
}

/// The newest recorded upgrade, when it failed (one a restart interrupted,
/// or one run offline).
async fn last_failed_upgrade(state: &AppState, bot_id: Uuid) -> Result<Value, ApiError> {
    let last: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT state, error FROM computer_upgrades WHERE bot_id = ?1
         ORDER BY created_at DESC, rowid DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(&state.db)
    .await?;
    Ok(match last {
        Some((upgrade, error)) if upgrade == "failed" => json!({
            "state": "failed",
            "error": error.unwrap_or_else(|| "interrupted".into()),
        }),
        _ => Value::Null,
    })
}

async fn upgrade(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_bot(&state, bot_id, true).await?;
    let computers = manager(&state)?;
    let Some(admission) = state.scheduler.gate().enter() else {
        return Err(ApiError::unavailable(
            "maintenance",
            "A backup or an update is in progress; try again shortly.",
        ));
    };
    match computers.start_upgrade(bot_id, admission).await {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(ComputerError::Refused(reason)) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "upgrade_refused",
            reason,
        )),
        Err(err) => Err(computer_error(err)),
    }
}

async fn stop(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_bot(&state, bot_id, false).await?;
    manager(&state)?
        .owner_stop(bot_id)
        .await
        .map_err(computer_error)?;
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
        .map_err(computer_error)?;
    Ok(StatusCode::NO_CONTENT)
}

fn computer_error(err: crate::computers::ComputerError) -> ApiError {
    match err {
        crate::computers::ComputerError::Db(err) => err.into(),
        other => {
            tracing::warn!(error = %other, "computer operation failed");
            ApiError::unavailable(
                "computer_unavailable",
                "The computer could not be changed now.",
            )
        }
    }
}

/// Owner access to a bot's desktop: holds the computer with a renewable
/// lease (call again to renew) and returns the loopback noVNC address and
/// the computer's VNC password. Nothing is reachable without the password,
/// and nothing listens beyond host loopback.
async fn desktop(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_bot(&state, bot_id, true).await?;
    match manager(&state)?
        .desktop(bot_id)
        .await
        .map_err(computer_error)?
    {
        Ok(desktop) => Ok(Json(json!({
            "url": desktop.url,
            "password": desktop.password,
            "lease_seconds": desktop.lease_seconds,
        }))),
        Err(crate::computers::Acquire::OwnerStopped) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "computer_stopped",
            "The owner stopped this computer; resume it first.",
        )),
        Err(other) => Err(ApiError::unavailable(
            "computer_unavailable",
            format!("The desktop is not available now: {other:?}"),
        )),
    }
}

async fn close_desktop(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_bot(&state, bot_id, false).await?;
    manager(&state)?
        .close_desktop(bot_id)
        .await
        .map_err(computer_error)?;
    Ok(StatusCode::NO_CONTENT)
}
