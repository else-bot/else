//! Conversation snapshots and the authenticated per-bot event stream.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiPath};
use super::turns::bot_deleted;
use crate::db::bots::BotStatus;
use crate::db::{conversation, events};
use crate::views::{activity_json, item_json, turn_json};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;
const BATCH: i64 = 200;
const POLL_FALLBACK: Duration = Duration::from_secs(1);
const HEARTBEAT: Duration = Duration::from_secs(15);

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/conversation", get(snapshot))
        .route("/bots/{bot_id}/events", get(stream))
}

/// `<server_instance_id>:<bot_id>:<seq>`. Binding the cursor to this API
/// process forces a fresh snapshot after any restart or restore.
pub fn encode_cursor(instance: Uuid, bot_id: Uuid, seq: i64) -> String {
    format!("{instance}:{bot_id}:{seq}")
}

fn parse_cursor(text: &str) -> Option<(Uuid, Uuid, i64)> {
    let mut parts = text.split(':');
    let instance = parts.next()?.parse().ok()?;
    let bot = parts.next()?.parse().ok()?;
    let seq: i64 = parts.next()?.parse().ok()?;
    (parts.next().is_none() && seq >= 0).then_some((instance, bot, seq))
}

fn reset_required() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "cursor_reset_required",
        "The event cursor is no longer valid. Load a fresh snapshot.",
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotQuery {
    limit: Option<i64>,
    before_seq: Option<i64>,
}

async fn snapshot(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    query: Result<Query<SnapshotQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request(
            "invalid_query",
            "Use limit and before_seq as whole numbers.",
        )
    })?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) || query.before_seq.is_some_and(|seq| seq < 1) {
        return Err(ApiError::bad_request(
            "invalid_query",
            "limit must be 1 to 200 and before_seq at least 1.",
        ));
    }
    let snapshot = conversation::snapshot(&state.db, bot_id, limit, query.before_seq)
        .await?
        .ok_or_else(ApiError::not_found)?;
    if snapshot.bot.status != BotStatus::Active {
        return Err(bot_deleted());
    }
    let bot = &snapshot.bot;
    Ok(Json(json!({
        "conversation_id": bot.conversation_id,
        "bot_id": bot.id,
        "selected_provider_id": bot.selected_provider_id,
        "event_cursor": encode_cursor(state.server_instance_id, bot.id, snapshot.last_event_seq),
        "items": snapshot.items.iter().map(item_json).collect::<Vec<_>>(),
        "turns": snapshot.turns.iter().map(turn_json).collect::<Vec<_>>(),
        "activity": activity_json(bot),
        "older_before_seq": snapshot.older_before_seq,
    })))
}

async fn bot_is_active(state: &AppState, bot_id: Uuid) -> Result<Option<(bool, i64)>, sqlx::Error> {
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT status, last_event_seq FROM bots WHERE id = $1")
            .bind(bot_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(row.map(|(status, seq)| (status == "active", seq)))
}

async fn stream(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let Some((active, last_seq)) = bot_is_active(&state, bot_id).await? else {
        return Err(ApiError::not_found());
    };
    if !active {
        return Err(bot_deleted());
    }
    let cursor = headers
        .get("last-event-id")
        .ok_or_else(|| {
            ApiError::bad_request(
                "cursor_required",
                "Send the snapshot's event_cursor as Last-Event-ID.",
            )
        })?
        .to_str()
        .ok()
        .and_then(parse_cursor)
        .ok_or_else(|| ApiError::bad_request("invalid_cursor", "The event cursor is malformed."))?;
    let (instance, cursor_bot, after) = cursor;
    if cursor_bot != bot_id {
        return Err(ApiError::bad_request(
            "invalid_cursor",
            "The event cursor belongs to a different bot.",
        ));
    }
    if instance != state.server_instance_id || after > last_seq {
        return Err(reset_required());
    }

    // Subscribe before the first read so no wake between them is lost.
    let wake = state.events.subscribe();
    let (tx, rx) = mpsc::channel::<Event>(64);
    tokio::spawn(pump(state.clone(), bot_id, after, wake, tx));
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|event| (Ok::<_, Infallible>(event), rx))
    });
    let mut response = Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(HEARTBEAT))
        .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    Ok(response)
}

/// Send committed journal rows after `after` in order, then wait for a wake
/// hint or the polling fallback. Ends when the client disconnects or the bot
/// stops being active (deletion closes access immediately).
async fn pump(
    state: Arc<AppState>,
    bot_id: Uuid,
    mut after: i64,
    mut wake: broadcast::Receiver<Uuid>,
    tx: mpsc::Sender<Event>,
) {
    loop {
        match bot_is_active(&state, bot_id).await {
            Ok(Some((true, _))) => {}
            Ok(_) => return,
            Err(err) => {
                tracing::warn!(error = %err, "event stream lost the database");
                return;
            }
        }
        let rows = match events::after(&state.db, bot_id, after, BATCH).await {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(error = %err, "reading events failed");
                return;
            }
        };
        let full = rows.len() as i64 == BATCH;
        for (seq, kind, data) in rows {
            let event = Event::default()
                .id(encode_cursor(state.server_instance_id, bot_id, seq))
                .event(kind)
                .data(data.to_string());
            if tx.send(event).await.is_err() {
                return;
            }
            after = seq;
        }
        if full {
            continue;
        }
        tokio::select! {
            _ = tx.closed() => return,
            _ = tokio::time::sleep(POLL_FALLBACK) => {}
            received = wake.recv() => match received {
                Ok(id) if id == bot_id || id.is_nil() => {}
                // Lagged or another bot: re-poll anyway, it is cheap.
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    tokio::time::sleep(POLL_FALLBACK).await;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_round_trip_and_rejections() {
        let (instance, bot) = (Uuid::new_v4(), Uuid::new_v4());
        let text = encode_cursor(instance, bot, 42);
        assert_eq!(parse_cursor(&text), Some((instance, bot, 42)));
        for bad in [
            "",
            "1:2:3",
            &format!("{instance}:{bot}"),
            &format!("{instance}:{bot}:-1"),
            &format!("{instance}:{bot}:1:2"),
            &format!("{instance}:{bot}:x"),
        ] {
            assert_eq!(parse_cursor(bad), None, "{bad}");
        }
    }
}
