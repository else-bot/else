//! Owner routes for a bot's outgoing drafts (emails, messages, posts).
//! Only the owner token reaches them; a task grant never approves, edits,
//! or discards a draft.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use crate::app::outgoing::Edits;
use crate::db::outgoing_drafts::{self, DraftError, Settled};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/outgoing-drafts/{draft_id}", put(edit))
        .route(
            "/bots/{bot_id}/outgoing-drafts/{draft_id}/approve",
            post(approve),
        )
        .route(
            "/bots/{bot_id}/outgoing-drafts/{draft_id}/discard",
            post(discard),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionedEdits {
    version: i64,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

impl VersionedEdits {
    fn edits(&self) -> Edits<'_> {
        Edits {
            subject: self.subject.as_deref(),
            body: self.body.as_deref(),
        }
    }
}

pub(super) fn draft_error(err: DraftError) -> ApiError {
    let conflict = |code, message: String| ApiError::new(StatusCode::CONFLICT, code, message);
    match err {
        DraftError::NotFound => ApiError::not_found(),
        DraftError::Changed(version) => conflict(
            "draft_changed",
            format!("This draft changed after version {version}. Reload it and try again."),
        ),
        DraftError::Settled(status) => conflict(
            "draft_settled",
            format!("This draft is already {}.", status.as_str()),
        ),
        DraftError::Inactive => conflict(
            "draft_inactive",
            "The request that drafted this has ended. Reply in chat to send it.".into(),
        ),
        DraftError::Invalid(message) => ApiError::bad_request("invalid_draft", message),
        DraftError::Db(err) => err.into(),
        other => {
            tracing::warn!(error = %other, "unexpected outgoing draft error on an owner route");
            ApiError::internal()
        }
    }
}

fn settled(state: &AppState, settled: Settled) -> Json<Value> {
    if settled.requeued {
        state.scheduler.wake();
    }
    Json(json!({ "draft": settled.draft, "requeued": settled.requeued }))
}

/// Save the owner's edits to a pending draft they saw at `version`.
async fn edit(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, draft_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<VersionedEdits>,
) -> Result<Json<Value>, ApiError> {
    let done = outgoing_drafts::edit(&state.db, bot_id, draft_id, body.version, body.edits())
        .await
        .map_err(draft_error)?;
    Ok(settled(&state, done))
}

/// Approve the version the owner saw, with their final edits. A retry of
/// the same approval succeeds without changing anything.
async fn approve(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, draft_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<VersionedEdits>,
) -> Result<Json<Value>, ApiError> {
    let done = outgoing_drafts::approve(&state.db, bot_id, draft_id, body.version, body.edits())
        .await
        .map_err(draft_error)?;
    Ok(settled(&state, done))
}

async fn discard(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, draft_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let done = outgoing_drafts::discard(&state.db, bot_id, draft_id)
        .await
        .map_err(draft_error)?;
    Ok(settled(&state, done))
}
