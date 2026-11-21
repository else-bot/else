//! Owner routes for a bot's computer: its state, an explicit stop that
//! prevents implicit wake, resume (which starts nothing by itself), and an
//! upgrade to the configured image that keeps the computer's data. And for
//! all of them: every computer with its use, and Upgrade all.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use else_wire::computers::{
    CancelledComputers, Computer, ComputerState, ComputerStateHeader, ComputerUpgrade, Computers,
    DesiredState, QueuedComputers, RuntimeState, SettledHandoff,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::computers::{ComputerError, Desired, Observed, UpgradeState};
use crate::db::computer_handoffs as handoffs;

use super::error::{ApiError, ApiPath};
use super::turns::bot_deleted;
use super::{AppState, Routes};
use crate::db::bots::{self, BotStatus};

pub fn routes() -> Routes {
    Routes {
        open: Router::new()
            .route("/computers", get(list))
            .route("/bots/{bot_id}/computer", get(show))
            .route("/bots/{bot_id}/computer/stop", post(stop))
            // The stream enters the gate itself, and only while it wakes.
            .route(
                "/bots/{bot_id}/desktop/stream",
                get(super::desktop_stream::stream),
            ),
        // Opening a desktop wakes the computer, so it waits like a write.
        gated: Router::new()
            .route("/computers/upgrade-all", post(upgrade_all))
            .route("/computers/upgrade-all/cancel", post(cancel_upgrade_all))
            .route("/bots/{bot_id}/computer/resume", post(resume))
            .route("/bots/{bot_id}/computer/upgrade", post(upgrade))
            .route("/bots/{bot_id}/computer/memory", post(set_memory))
            .route("/bots/{bot_id}/desktop", get(desktop).delete(close_desktop))
            .route(
                "/bots/{bot_id}/computer-handoffs/{handoff_id}/take",
                post(take_handoff),
            )
            .route(
                "/bots/{bot_id}/computer-handoffs/{handoff_id}/hand-back",
                post(hand_back_handoff),
            )
            .route(
                "/bots/{bot_id}/computer-handoffs/{handoff_id}/dismiss",
                post(dismiss_handoff),
            ),
    }
}

async fn take_handoff(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, handoff_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<SettledHandoff>, ApiError> {
    settle_handoff(&state, bot_id, handoff_id, handoffs::Action::Take).await
}

async fn hand_back_handoff(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, handoff_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<SettledHandoff>, ApiError> {
    settle_handoff(&state, bot_id, handoff_id, handoffs::Action::HandBack).await
}

async fn dismiss_handoff(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, handoff_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<SettledHandoff>, ApiError> {
    settle_handoff(&state, bot_id, handoff_id, handoffs::Action::Dismiss).await
}

async fn settle_handoff(
    state: &AppState,
    bot_id: Uuid,
    handoff_id: Uuid,
    action: handoffs::Action,
) -> Result<Json<SettledHandoff>, ApiError> {
    match handoffs::settle(&state.db, bot_id, handoff_id, action).await {
        Ok(handoff) => Ok(Json(SettledHandoff {
            handoff: (&handoff).into(),
        })),
        Err(handoffs::SettleError::NotFound) => Err(ApiError::not_found()),
        Err(handoffs::SettleError::Settled(handoff)) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "computer_handoff_settled",
            format!(
                "This request is already {}.",
                handoff.status.as_str().replace('_', " ")
            ),
        )),
        Err(handoffs::SettleError::Db(err)) => Err(err.into()),
    }
}

pub(super) fn manager(
    state: &AppState,
) -> Result<&Arc<crate::computers::ComputerManager>, ApiError> {
    state.computers.manager().ok_or_else(|| {
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
    match bots::status(&state.db, bot_id).await? {
        None => Err(ApiError::not_found()),
        Some(BotStatus::Active) => Ok(()),
        Some(_) if active => Err(bot_deleted()),
        Some(_) => Ok(()),
    }
}

async fn show(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Response, ApiError> {
    require_bot(&state, bot_id, false).await?;
    // Before the row: an upgrade writes its row before its status ends, so
    // a status that has ended is never paired with the row it replaced.
    let status = match state.computers.manager() {
        Some(computers) => computers.upgrade_status(bot_id).await,
        None => None,
    };
    #[derive(sqlx::FromRow)]
    struct ComputerRow {
        runtime_name: String,
        image: String,
        cpus: i32,
        memory_mib: i32,
        desired_state: Desired,
        observed_state: Observed,
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
        .manager()
        .is_some_and(|computers| computers.config().desktop);
    let default_memory = state
        .computers
        .manager()
        .map(|computers| computers.config().memory_mib);
    let (latest_image, upgrade) = match state.computers.manager() {
        Some(computers) => (
            Some(computers.config().image.clone()),
            match status {
                None => last_upgrade(&state, bot_id).await?,
                Some(status) => Some(status.into()),
            },
        ),
        None => (None, None),
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
        return Ok(Json(ComputerStateHeader {
            bot_id: Some(bot_id),
            state: RuntimeState::Absent,
            desired_state: DesiredState::Auto,
            desktop_available: desktop,
            default_memory_mib: default_memory,
        })
        .into_response());
    };
    Ok(Json(ComputerState {
        status: ComputerStateHeader {
            bot_id: Some(bot_id),
            default_memory_mib: default_memory,
            desired_state: desired.into(),
            state: observed.into(),
            desktop_available: desktop && has_desktop,
        },
        runtime_name: Some(name),
        image: Some(image),
        cpus: Some(cpus),
        memory_mib: Some(memory as u32),
        active_leases: Some(leases),
        last_used_at: last_used,
        last_error: error,
        latest_image,
        upgrade,
    })
    .into_response())
}

const MIB: u64 = 1024 * 1024;

/// Every created computer of an active bot, oldest bot first, with its
/// upgrade and its use: CPU, memory and the disk's size only while it runs,
/// as the guest reports them; otherwise the disk's host blocks, from a scan
/// at most a minute old. `rollback_bytes` is the disk still held by the
/// computers that upgrades and rollbacks kept.
async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Computers>, ApiError> {
    let computers = manager(&state)?;
    // Before the rows: an upgrade writes its row before its status ends, so
    // a status that has ended is never paired with the row it replaced.
    let statuses = computers.upgrade_statuses().await;
    #[derive(sqlx::FromRow)]
    struct Row {
        bot_id: Uuid,
        runtime_name: String,
        image: String,
        cpus: i32,
        memory_mib: i32,
        desired_state: Desired,
        observed_state: Observed,
        last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let rows = sqlx::query_as::<_, Row>(
        "SELECT c.bot_id, c.runtime_name, c.image, c.cpus, c.memory_mib, c.desired_state,
                c.observed_state, c.last_used_at
         FROM bot_computers c JOIN bots b ON b.id = c.bot_id
         WHERE b.status = 'active' AND c.observed_state <> 'absent'
         ORDER BY b.created_at, b.id",
    )
    .fetch_all(&state.db);
    let replaced = sqlx::query_scalar::<_, String>(
        "SELECT from_runtime_name FROM computer_upgrades WHERE state IN ('completed', 'rolled_back')
         UNION SELECT to_runtime_name FROM computer_upgrades WHERE state IN ('completed', 'rolled_back')
         EXCEPT SELECT runtime_name FROM bot_computers",
    )
    .fetch_all(&state.db);
    let (rows, replaced, recorded, usage, disks) = tokio::join!(
        rows,
        replaced,
        recorded_upgrades(&state, None),
        computers.usage(),
        computers.disks(),
    );
    let (rows, replaced, mut recorded) = (rows?, replaced?, recorded?);
    let latest = &computers.config().image;
    let rollback_bytes = replaced
        .iter()
        .filter_map(|name| disks.get(name).copied())
        .reduce(|a, b| a + b);
    let mut listed = Vec::with_capacity(rows.len());
    for row in rows {
        let upgrade = match statuses.get(&row.bot_id) {
            Some(status) => Some(status.clone().into()),
            None => recorded.remove(&row.bot_id),
        };
        let live = usage
            .get(&row.runtime_name)
            .filter(|_| row.observed_state == Observed::Running);
        let cpus = live
            .and_then(|usage| usage.cpus)
            .unwrap_or(row.cpus.max(1) as u32);
        let (disk_used, disk_size) = match live.and_then(|usage| usage.disk) {
            Some((used, size)) => (Some(used), Some(size)),
            None => (disks.get(&row.runtime_name).copied(), None),
        };
        listed.push(Computer {
            bot_id: row.bot_id,
            state: row.observed_state.into(),
            desired_state: row.desired_state.into(),
            image: row.image,
            latest_image: Some(latest.clone()),
            upgrade,
            disk_used_bytes: disk_used,
            disk_size_bytes: disk_size,
            cpus,
            cpu_percent: live
                .map(|usage| f64::from((usage.cpu_percent / cpus as f32).clamp(0.0, 100.0))),
            memory_used_bytes: live.and_then(|usage| usage.memory_bytes),
            memory_limit_bytes: live
                .map(|usage| usage.memory_limit_bytes)
                .unwrap_or(row.memory_mib.max(0) as u64 * MIB),
            last_used_at: row.last_used_at,
        });
    }
    Ok(Json(Computers {
        latest_image: Some(latest.clone()),
        rollback_bytes,
        computers: listed,
    }))
}

async fn upgrade_all(
    State(state): State<Arc<AppState>>,
) -> Result<(StatusCode, Json<QueuedComputers>), ApiError> {
    let queued = manager(&state)?
        .upgrade_all(state.gate.clone())
        .await
        .map_err(computer_error)?;
    Ok((StatusCode::ACCEPTED, Json(QueuedComputers { queued })))
}

async fn cancel_upgrade_all(
    State(state): State<Arc<AppState>>,
) -> Result<Json<CancelledComputers>, ApiError> {
    let cancelled = manager(&state)?.cancel_upgrade_all().await;
    Ok(Json(CancelledComputers { cancelled }))
}

/// The newest recorded upgrade, when it failed (one a restart interrupted,
/// or one run offline), or when it made the current computer and left some
/// of the bot's APT packages uninstalled.
async fn last_upgrade(state: &AppState, bot_id: Uuid) -> Result<Option<ComputerUpgrade>, ApiError> {
    Ok(recorded_upgrades(state, Some(bot_id))
        .await?
        .remove(&bot_id))
}

/// [`last_upgrade`] of one bot, or of every bot that has one.
async fn recorded_upgrades(
    state: &AppState,
    bot_id: Option<Uuid>,
) -> Result<std::collections::HashMap<Uuid, ComputerUpgrade>, ApiError> {
    #[derive(sqlx::FromRow)]
    struct Recorded {
        bot_id: Uuid,
        state: UpgradeState,
        error: Option<String>,
        unrestored_packages: Option<String>,
        current: bool,
    }
    let newest: Vec<Recorded> = sqlx::query_as(
        "SELECT bot_id, state, error, unrestored_packages, current FROM (
             SELECT u.bot_id, u.state, u.error, u.unrestored_packages,
                    u.to_runtime_name = c.runtime_name AS current,
                    row_number() OVER (
                        PARTITION BY u.bot_id ORDER BY u.created_at DESC, u.rowid DESC
                    ) AS newest
             FROM computer_upgrades u JOIN bot_computers c ON c.bot_id = u.bot_id
             WHERE ?1 IS NULL OR u.bot_id = ?1
         ) WHERE newest = 1",
    )
    .bind(bot_id)
    .fetch_all(&state.db)
    .await?;
    Ok(newest
        .into_iter()
        .filter_map(|recorded| {
            let Recorded {
                bot_id,
                state,
                error,
                unrestored_packages: packages,
                current,
            } = recorded;
            let shown = match (state, packages) {
                (UpgradeState::Failed, _) => ComputerUpgrade::Failed {
                    error: error.unwrap_or_else(|| "interrupted".into()),
                },
                (UpgradeState::Completed, Some(packages)) if current => {
                    ComputerUpgrade::Completed {
                        unrestored_packages: serde_json::from_str(&packages).unwrap_or_default(),
                    }
                }
                _ => return None,
            };
            Some((bot_id, shown))
        })
        .collect())
}

async fn upgrade(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_bot(&state, bot_id, true).await?;
    let computers = manager(&state)?;
    let Some(admission) = state.gate.enter() else {
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetMemory {
    /// One of the offered sizes; `null` returns to the configured size.
    memory_mib: Option<u32>,
}

/// Set the memory the computer boots with. A running computer restarts at
/// the new size; its jobs stop and its files stay.
async fn set_memory(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    Json(body): Json<SetMemory>,
) -> Result<Json<Value>, ApiError> {
    require_bot(&state, bot_id, true).await?;
    let computers = manager(&state)?;
    let Some(_admission) = state.gate.enter() else {
        return Err(ApiError::unavailable(
            "maintenance",
            "A backup or an update is in progress; try again shortly.",
        ));
    };
    let memory = body
        .memory_mib
        .filter(|memory| *memory != computers.config().memory_mib);
    match computers.resize_memory(bot_id, memory).await {
        Ok(resized) => Ok(Json(json!({
            "memory_mib": resized.memory_mib,
            "restarted": resized.restarted,
        }))),
        Err(ComputerError::Refused(reason)) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "memory_refused",
            reason,
        )),
        Err(ComputerError::Busy(reason)) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "computer_busy",
            reason.to_string(),
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
