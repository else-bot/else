//! The owner's usage report: each bot's share of every provider's
//! subscription windows.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;

use super::error::ApiError;
use super::{AppState, Routes};
use else_wire::usage::UsageReport;

use crate::app::usage;

pub fn routes() -> Routes {
    Routes::open(Router::new().route("/usage", get(report)))
}

async fn report(State(state): State<Arc<AppState>>) -> Result<Json<UsageReport>, ApiError> {
    let now = Utc::now();
    let (turns, readings) = tokio::try_join!(
        crate::db::usage::turns(&state.db, now),
        crate::db::usage::readings(&state.db, now),
    )?;
    Ok(Json(usage::report(now, &turns, &readings)))
}
