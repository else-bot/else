//! Owner memory routes: read a bot's original log, record an explicit
//! correction, and configure which bots may read another bot's memory.
//! Corrections exist only here, so model or tool text can never create one.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use super::turns::bot_deleted;
use crate::db::memory::{self, AppendError, Kind, NewEntry};

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/memory", get(read))
        .route("/bots/{bot_id}/memory/corrections", post(correct))
        .route("/bots/{bot_id}/memory-access", get(access))
        .route(
            "/bots/{bot_id}/memory-access/{target_id}",
            put(grant).delete(revoke),
        )
}

/// `Some(true)` active, `Some(false)` deleted, `None` unknown.
async fn bot_state(state: &AppState, bot_id: Uuid) -> Result<Option<bool>, ApiError> {
    Ok(
        sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = $1")
            .bind(bot_id)
            .fetch_optional(&state.db)
            .await?,
    )
}

async fn require_active(state: &AppState, bot_id: Uuid) -> Result<(), ApiError> {
    match bot_state(state, bot_id).await? {
        Some(true) => Ok(()),
        Some(false) => Err(bot_deleted()),
        None => Err(ApiError::not_found()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadQuery {
    start_seq: Option<i64>,
    count: Option<i64>,
}

async fn read(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    query: Result<Query<ReadQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) =
        query.map_err(|_| ApiError::bad_request("invalid_query", "Use start_seq and count."))?;
    require_active(&state, bot_id).await?;
    let start = query.start_seq.unwrap_or(0).max(0);
    let count = query.count.unwrap_or(50).clamp(1, 200);
    let rows = memory::read(&state.db, bot_id, start, count).await?;
    let length = memory::length(&state.db, bot_id).await?;
    let entries: Vec<Value> = rows
        .iter()
        .map(|e| {
            json!({
                "seq": e.seq, "kind": e.kind, "authority": e.authority, "body": e.body,
                "source": e.source, "part": { "index": e.part_index, "count": e.part_count },
                "supersedes_seq": e.supersedes_seq, "superseded_by": e.superseded_by,
                "created_at": e.created_at,
            })
        })
        .collect();
    Ok(Json(json!({ "entries": entries, "length": length })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorrectionBody {
    request_id: Uuid,
    supersedes_seq: i64,
    text: String,
}

async fn correct(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<CorrectionBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if body.text.trim().is_empty() || body.text.contains('\0') {
        return Err(ApiError::bad_request(
            "invalid_input",
            "Text must not be empty.",
        ));
    }
    if body.text.len() > crate::app::messages::MAX_TEXT_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            "Text is too long.",
        ));
    }
    require_active(&state, bot_id).await?;
    let key = format!("correction:{}", body.request_id);
    let entry = NewEntry {
        request_key: &key,
        kind: Kind::OwnerCorrection,
        body: &body.text,
        source: json!({ "request_id": body.request_id }),
        supersedes_seq: Some(body.supersedes_seq),
    };
    let mut tx = state.db.begin().await?;
    let seqs = match memory::append(&mut tx, bot_id, &entry).await {
        Ok(seqs) => seqs,
        Err(AppendError::Conflict) => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "request_conflict",
                "This request ID was already used with different input.",
            ));
        }
        Err(AppendError::InvalidCorrection) => {
            return Err(ApiError::bad_request(
                "invalid_correction",
                "supersedes_seq must name an earlier owner message or correction of this bot.",
            ));
        }
        Err(AppendError::BotInactive) => return Err(bot_deleted()),
        Err(AppendError::Empty) => {
            return Err(ApiError::bad_request(
                "invalid_input",
                "Text must not be empty.",
            ));
        }
        Err(AppendError::Db(err)) => return Err(err.into()),
    };
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "seq": seqs[0], "seqs": seqs, "supersedes_seq": body.supersedes_seq })),
    ))
}

async fn access(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_active(&state, bot_id).await?;
    let targets: Vec<Uuid> = sqlx::query_scalar(
        "SELECT g.target_bot_id FROM memory_read_grants g JOIN bots b ON b.id = g.target_bot_id
         WHERE g.reader_bot_id = $1 AND b.status = 'active' ORDER BY g.created_at",
    )
    .bind(bot_id)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(json!({ "bot_id": bot_id, "can_read": targets })))
}

async fn grant(
    State(state): State<Arc<AppState>>,
    ApiPath((reader, target)): ApiPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    if reader == target {
        return Err(ApiError::bad_request(
            "invalid_input",
            "A bot always reads its own memory.",
        ));
    }
    require_active(&state, reader).await?;
    require_active(&state, target).await?;
    sqlx::query(
        "INSERT INTO memory_read_grants (reader_bot_id, target_bot_id) VALUES ($1, $2)
         ON CONFLICT DO NOTHING",
    )
    .bind(reader)
    .bind(target)
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn revoke(
    State(state): State<Arc<AppState>>,
    ApiPath((reader, target)): ApiPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    if bot_state(&state, reader).await?.is_none() {
        return Err(ApiError::not_found());
    }
    sqlx::query("DELETE FROM memory_read_grants WHERE reader_bot_id = $1 AND target_bot_id = $2")
        .bind(reader)
        .bind(target)
        .execute(&state.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
