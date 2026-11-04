//! The per-bot event journal.
//!
//! Mutations append events inside their own immediate transaction, so event
//! sequences follow commit order. EventHub observes committed data versions;
//! rows remain authoritative and streams also poll as a fallback.

use serde_json::{Value, json};
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::db::{bots, routines, turns};
use crate::views::{bot_json, item_json, routine_json, turn_json};

async fn append(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    kind: &str,
    data: Value,
) -> Result<i64, sqlx::Error> {
    let seq: i64 = sqlx::query_scalar(
        "UPDATE bots SET last_event_seq = last_event_seq + 1 WHERE id = ?1 RETURNING last_event_seq",
    )
    .bind(bot_id)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query("INSERT INTO bot_events (bot_id, seq, kind, data) VALUES (?1, ?2, ?3, ?4)")
        .bind(bot_id)
        .bind(seq)
        .bind(kind)
        .bind(data)
        .execute(&mut *conn)
        .await?;
    notify(conn, bot_id).await?;
    Ok(seq)
}

/// Wake streams for this bot once the transaction commits.
pub async fn notify(_conn: &mut SqliteConnection, _bot_id: Uuid) -> Result<(), sqlx::Error> {
    // EventHub observes committed database versions. Never wake on uncommitted state.
    Ok(())
}

/// The bot's current state, with its routines and their last runs: every
/// routine change and every turn change journals one.
pub async fn bot_updated(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<(), sqlx::Error> {
    if let Some(mut bot) = bots::find_in(conn, bot_id).await? {
        bot.metadata_revision =
            sqlx::query_scalar("SELECT last_event_seq + 1 FROM bots WHERE id = ?1")
                .bind(bot_id)
                .fetch_one(&mut *conn)
                .await?;
        let routines: Vec<Value> = routines::list(conn, bot_id)
            .await?
            .iter()
            .map(routine_json)
            .collect();
        append(
            conn,
            bot_id,
            "bot.updated",
            json!({ "bot_id": bot_id, "bot": bot_json(&bot), "routines": routines }),
        )
        .await?;
    }
    Ok(())
}

pub async fn item_upsert(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    item_id: Uuid,
) -> Result<(), sqlx::Error> {
    let item = turns::load_item(conn, item_id).await?;
    if item.kind == "routine" || is_task(conn, item.conversation_id).await? {
        return Ok(());
    }
    let data = json!({
        "bot_id": bot_id,
        "conversation_id": item.conversation_id,
        "item": item_json(&item),
    });
    append(conn, bot_id, "item.upsert", data).await?;
    Ok(())
}

pub async fn item_delta(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    conversation_id: Uuid,
    item_id: Uuid,
    offset: i64,
    text: &str,
) -> Result<(), sqlx::Error> {
    if is_task(conn, conversation_id).await? {
        return Ok(());
    }
    let data = json!({
        "bot_id": bot_id,
        "conversation_id": conversation_id,
        "item_id": item_id,
        "offset_utf8": offset,
        "text": text,
    });
    append(conn, bot_id, "item.delta", data).await?;
    Ok(())
}

/// Whether a conversation holds delegated task work.
async fn is_task(conn: &mut SqliteConnection, conversation_id: Uuid) -> Result<bool, sqlx::Error> {
    let kind: Option<String> = sqlx::query_scalar("SELECT kind FROM conversations WHERE id = ?1")
        .bind(conversation_id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(kind.as_deref() == Some("task"))
}

/// A turn's status or outcome changed; the bot's activity changes with it.
pub async fn turn_changed(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    let turn = turns::load_turn(conn, turn_id).await?;
    // Delegated work is not part of the owner's conversation; only the
    // bot's activity changes visibly.
    if !is_task(conn, turn.conversation_id).await? {
        let data = json!({
            "bot_id": bot_id,
            "conversation_id": turn.conversation_id,
            "turn": turn_json(&turn),
        });
        append(conn, bot_id, "turn.updated", data).await?;
    }
    bot_updated(conn, bot_id).await
}

/// A new owner message: its item, its queued turn, and the bot's activity
/// and provider selection.
pub async fn turn_accepted(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    let turn = turns::load_turn(conn, turn_id).await?;
    item_upsert(conn, bot_id, turn.source_item_id).await?;
    turn_changed(conn, bot_id, turn_id).await
}

/// Committed events after `after`, oldest first.
pub async fn after(
    pool: &sqlx::SqlitePool,
    bot_id: Uuid,
    after: i64,
    limit: i64,
) -> Result<Vec<(i64, String, Value)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT seq, kind, data FROM bot_events WHERE bot_id = ?1 AND seq > ?2
         ORDER BY seq LIMIT ?3",
    )
    .bind(bot_id)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
}
