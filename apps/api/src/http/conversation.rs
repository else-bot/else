use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiPath};
use super::turns::bot_deleted;
use crate::db::bots::BotStatus;
use crate::db::conversation;
use crate::views::{activity_json, item_json, routine_json, turn_json};

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/bots/{bot_id}/conversation", get(snapshot))
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
        "activity": activity_json(bot),
        "older_before_seq": snapshot.older_before_seq,
        "routines": snapshot.routines.iter().map(routine_json).collect::<Vec<_>>(),
    })))
}

pub async fn bot_is_active(
    state: &AppState,
    bot_id: Uuid,
) -> Result<Option<(bool, i64)>, sqlx::Error> {
    let row: Option<(String, i64)> =
        sqlx::query_as("SELECT status, last_event_seq FROM bots WHERE id = ?1")
            .bind(bot_id)
            .fetch_optional(&state.db)
            .await?;
    Ok(row.map(|(status, seq)| (status == "active", seq)))
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
