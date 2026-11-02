//! The per-bot event journal.
//!
//! Every mutation of visible history or turn state appends events inside its
//! own transaction: it increments `bots.last_event_seq` (which row-locks the
//! bot, so cursors follow commit order per bot) and inserts the event rows.
//! `pg_notify` is delivered only after commit, so it is a safe wake hint for
//! streams; rows remain authoritative.

use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::db::{bots, turns};
use crate::views::{bot_json, item_json, turn_json};

/// Notification channel carrying a bot ID after each committed change.
pub const CHANNEL: &str = "else_bot_events";

async fn append(
    conn: &mut PgConnection,
    bot_id: Uuid,
    kind: &str,
    data: Value,
) -> Result<i64, sqlx::Error> {
    let seq: i64 = sqlx::query_scalar(
        "UPDATE bots SET last_event_seq = last_event_seq + 1 WHERE id = $1 RETURNING last_event_seq",
    )
    .bind(bot_id)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query("INSERT INTO bot_events (bot_id, seq, kind, data) VALUES ($1, $2, $3, $4)")
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
pub async fn notify(conn: &mut PgConnection, bot_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_notify($1, $2::text)")
        .bind(CHANNEL)
        .bind(bot_id)
        .execute(conn)
        .await?;
    Ok(())
}

async fn bot_updated(conn: &mut PgConnection, bot_id: Uuid) -> Result<(), sqlx::Error> {
    if let Some(bot) = bots::find_in(conn, bot_id).await? {
        append(
            conn,
            bot_id,
            "bot.updated",
            json!({ "bot_id": bot_id, "bot": bot_json(&bot) }),
        )
        .await?;
    }
    Ok(())
}

pub async fn item_upsert(
    conn: &mut PgConnection,
    bot_id: Uuid,
    item_id: Uuid,
) -> Result<(), sqlx::Error> {
    let item = turns::load_item(conn, item_id).await?;
    let data = json!({
        "bot_id": bot_id,
        "conversation_id": item.conversation_id,
        "item": item_json(&item),
    });
    append(conn, bot_id, "item.upsert", data).await?;
    Ok(())
}

pub async fn item_delta(
    conn: &mut PgConnection,
    bot_id: Uuid,
    conversation_id: Uuid,
    item_id: Uuid,
    offset: i64,
    text: &str,
) -> Result<(), sqlx::Error> {
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

/// A turn's status or outcome changed; the bot's activity changes with it.
pub async fn turn_changed(
    conn: &mut PgConnection,
    bot_id: Uuid,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    let turn = turns::load_turn(conn, turn_id).await?;
    let data = json!({
        "bot_id": bot_id,
        "conversation_id": turn.conversation_id,
        "turn": turn_json(&turn),
    });
    append(conn, bot_id, "turn.updated", data).await?;
    bot_updated(conn, bot_id).await
}

/// A new owner message: its item, its queued turn, and the bot's activity
/// and provider selection.
pub async fn turn_accepted(
    conn: &mut PgConnection,
    bot_id: Uuid,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    let turn = turns::load_turn(conn, turn_id).await?;
    item_upsert(conn, bot_id, turn.owner_item_id).await?;
    turn_changed(conn, bot_id, turn_id).await
}

/// Committed events after `after`, oldest first.
pub async fn after(
    pool: &sqlx::PgPool,
    bot_id: Uuid,
    after: i64,
    limit: i64,
) -> Result<Vec<(i64, String, Value)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT seq, kind, data FROM bot_events WHERE bot_id = $1 AND seq > $2
         ORDER BY seq LIMIT $3",
    )
    .bind(bot_id)
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await
}
