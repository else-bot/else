use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use else_plain_text::find::Needle;
use else_wire::routines::{Routine, Watcher, Webhook};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::error::{ApiError, ApiPath};
use super::turns::bot_deleted;
use super::{AppState, Routes};
use crate::db::bots::BotStatus;
use crate::db::conversation::{self, WindowQuery};
use crate::views::{item_json, turn_json};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;
const MAX_QUERY_CHARS: usize = 200;

pub fn routes() -> Routes {
    Routes::open(
        Router::new()
            .route("/bots/{bot_id}/conversation", get(snapshot))
            .route("/bots/{bot_id}/conversation/search", get(search))
            .route("/bots/{bot_id}/conversation/window", get(window)),
    )
}

/// `<server_instance_id>:<bot_id>:<seq>`. Binding the cursor to this API
/// process forces a fresh snapshot after any restart or restore.
pub fn encode_cursor(instance: Uuid, bot_id: Uuid, seq: i64) -> String {
    format!("{instance}:{bot_id}:{seq}")
}

pub fn parse_cursor(text: &str) -> Option<(Uuid, Uuid, i64)> {
    let mut parts = text.split(':');
    let instance = parts.next()?.parse().ok()?;
    let bot = parts.next()?.parse().ok()?;
    let seq: i64 = parts.next()?.parse().ok()?;
    (parts.next().is_none() && seq >= 0).then_some((instance, bot, seq))
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
        "activity": else_wire::bots::Activity::from(bot),
        "older_before_seq": snapshot.older_before_seq,
        "routines": snapshot.routines.iter().map(Routine::from).collect::<Vec<_>>(),
        "social_triggers": snapshot.social_triggers,
        "mail_triggers": snapshot.mail_triggers,
        "draft_grants": snapshot.draft_grants,
        "webhooks": snapshot.webhooks.iter().map(Webhook::from).collect::<Vec<_>>(),
        "watchers": snapshot.watchers.iter().map(Watcher::from).collect::<Vec<_>>(),
        "rules_version": snapshot.rules_version,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    q: String,
}

/// Every owner message and bot reply that shows `q`, newest first, with
/// how often it shows in each.
async fn search(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    query: Result<Query<SearchQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) =
        query.map_err(|_| ApiError::bad_request("invalid_query", "Pass the search text as q."))?;
    if query.q.chars().count() > MAX_QUERY_CHARS {
        return Err(ApiError::bad_request(
            "invalid_query",
            "Search for at most 200 characters.",
        ));
    }
    let mut conn = state.db.acquire().await?;
    let (conversation_id, status) = conversation::owner_conversation(&mut conn, bot_id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    drop(conn);
    if status != BotStatus::Active {
        return Err(bot_deleted());
    }
    let hits = match Needle::new(&query.q) {
        Some(needle) => conversation::search(&state.db, conversation_id, needle).await?,
        None => Vec::new(),
    };
    Ok(Json(json!({
        "total": hits.iter().map(|hit| hit.count).sum::<usize>(),
        "hits": hits
            .iter()
            .map(|hit| json!({ "item_id": hit.item_id, "seq": hit.seq, "count": hit.count }))
            .collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WindowParams {
    anchor_seq: i64,
    older: Option<i64>,
    newer: Option<i64>,
    after_seq: Option<i64>,
    before_seq: Option<i64>,
}

/// The shown items around `anchor_seq` that lie strictly between
/// `after_seq` and `before_seq`, the loaded items on either side, and how
/// many shown items each side still misses.
async fn window(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    query: Result<Query<WindowParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let invalid = || {
        ApiError::bad_request(
            "invalid_query",
            "Pass anchor_seq, older and newer from 0 to 200, and after_seq below before_seq.",
        )
    };
    let Query(params) = query.map_err(|_| invalid())?;
    let older = params.older.unwrap_or(DEFAULT_LIMIT / 2);
    let newer = params.newer.unwrap_or(DEFAULT_LIMIT / 2);
    if !(0..=MAX_LIMIT).contains(&older)
        || !(0..=MAX_LIMIT).contains(&newer)
        || params.anchor_seq < 0
        || params
            .after_seq
            .zip(params.before_seq)
            .is_some_and(|(after, before)| after >= before)
    {
        return Err(invalid());
    }
    let window = conversation::window(
        &state.db,
        bot_id,
        WindowQuery {
            anchor_seq: params.anchor_seq,
            older,
            newer,
            after_seq: params.after_seq,
            before_seq: params.before_seq,
        },
    )
    .await?
    .ok_or_else(ApiError::not_found)?;
    if window.status != BotStatus::Active {
        return Err(bot_deleted());
    }
    Ok(Json(json!({
        "items": window.items.iter().map(item_json).collect::<Vec<_>>(),
        "turns": window.turns.iter().map(turn_json).collect::<Vec<_>>(),
        "older_gap": window.older_gap,
        "newer_gap": window.newer_gap,
    })))
}

pub async fn bot_is_active(
    state: &AppState,
    bot_id: Uuid,
) -> Result<Option<(bool, i64)>, sqlx::Error> {
    let row: Option<(BotStatus, i64)> =
        sqlx::query_as("SELECT status, last_event_seq FROM bots WHERE id = ?1")
            .bind(bot_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(row.map(|(status, seq)| (status.is_active(), seq)))
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
