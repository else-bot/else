//! The owner's usage report: each bot's share of every provider's
//! subscription windows.

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::Utc;

use super::AppState;
use super::error::ApiError;
use crate::app::usage::{self, Report};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/usage", get(report))
}

async fn report(State(state): State<Arc<AppState>>) -> Result<Json<Report>, ApiError> {
    let now = Utc::now();
    let (turns, readings) = tokio::try_join!(
        crate::db::usage::turns(&state.db, now),
        crate::db::usage::readings(&state.db, now),
    )?;
    Ok(Json(usage::report(now, &turns, &readings)))
}
