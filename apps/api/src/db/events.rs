//! The per-bot event journal.
//!
//! Mutations append events inside their own immediate transaction, so event
//! sequences follow commit order. EventHub observes committed data versions;
//! rows remain authoritative and streams also poll as a fallback.

use else_wire::conversation::{BotUpdated, Event, ItemDelta, ItemUpsert, TurnUpdated};
use serde_json::Value;
use sqlx::SqliteConnection;
use uuid::Uuid;

use crate::db::turns::{ConversationKind, ItemKind};
use crate::db::{bots, turns};

async fn append(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    event: Event,
) -> Result<i64, sqlx::Error> {
    let kind = event.kind().as_str();
    let data = match &event {
        Event::ItemUpsert(data) => serde_json::to_value(data),
        Event::ItemDelta(data) => serde_json::to_value(data),
        Event::TurnUpdated(data) => serde_json::to_value(data),
        Event::BotUpdated(data) => serde_json::to_value(data),
    }
    .map_err(|err| sqlx::Error::Encode(Box::new(err)))?;
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
    Ok(())
}

/// The bot's current state, with its routines and their last runs, its
/// social and mail triggers, its webhooks and active watchers, and its rules version: every routine, trigger,
/// inbox, and turn change journals one.
pub async fn bot_updated(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<(), sqlx::Error> {
    if let Some(mut bot) = bots::find_in(conn, bot_id).await? {
        bot.metadata_revision =
            sqlx::query_scalar("SELECT last_event_seq + 1 FROM bots WHERE id = ?1")
                .bind(bot_id)
                .fetch_one(&mut *conn)
                .await?;
        let extras = super::conversation::bot_extras(conn, bot_id).await?;
        append(
            conn,
            bot_id,
            Event::BotUpdated(Box::new(BotUpdated {
                bot_id: Some(bot_id),
                bot: (&bot).into(),
                extras,
            })),
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
    if item.hidden
        || matches!(item.kind, ItemKind::Routine | ItemKind::Connector)
        || is_task(conn, item.conversation_id).await?
    {
        return Ok(());
    }
    let event = Event::ItemUpsert(Box::new(ItemUpsert {
        bot_id: Some(bot_id),
        conversation_id: Some(item.conversation_id),
        item: (&item).into(),
    }));
    append(conn, bot_id, event).await?;
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
    let event = Event::ItemDelta(ItemDelta {
        bot_id: Some(bot_id),
        conversation_id: Some(conversation_id),
        item_id,
        offset_utf8: u64::try_from(offset).unwrap_or_default(),
        text: text.to_owned(),
    });
    append(conn, bot_id, event).await?;
    Ok(())
}

async fn is_task(conn: &mut SqliteConnection, conversation_id: Uuid) -> Result<bool, sqlx::Error> {
    let kind: Option<ConversationKind> =
        sqlx::query_scalar("SELECT kind FROM conversations WHERE id = ?1")
            .bind(conversation_id)
            .fetch_optional(&mut *conn)
            .await?;
    Ok(kind == Some(ConversationKind::Task))
}

/// A turn's status or outcome changed; the bot's activity changes with it.
pub async fn turn_changed(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    let turn = turns::load_turn(conn, turn_id).await?;
    let owner_conversation_id: Uuid =
        sqlx::query_scalar("SELECT id FROM conversations WHERE bot_id = ?1 AND kind = 'owner'")
            .bind(bot_id)
            .fetch_one(&mut *conn)
            .await?;
    let event = Event::TurnUpdated(Box::new(TurnUpdated {
        bot_id: Some(bot_id),
        conversation_id: Some(owner_conversation_id),
        turn: (&turn).into(),
    }));
    append(conn, bot_id, event).await?;
    bot_updated(conn, bot_id).await?;
    crate::db::delegations::publish_reply(conn, turn_id).await?;
    root_changed(conn, turn_id).await?;
    reply_drafts_changed(conn, turn_id).await
}

/// `task_turn`'s declared need or one of its asks changed: republish the
/// rows that show them, its receipt in its bot's chat, its reply in the
/// delegating chat, and the reply one hop further up, which carries the asks
/// of the tasks below it.
pub async fn exchange_changed(
    conn: &mut SqliteConnection,
    task_turn: Uuid,
) -> Result<(), sqlx::Error> {
    let shown: Vec<(Uuid, Uuid)> = sqlx::query_as(
        // The task whose attempt (or takeover) `task_turn` is, and the task
        // that delegated it, whichever of their attempts did.
        "WITH task AS (
             SELECT d.* FROM delegations d
             WHERE d.conversation_id = (SELECT conversation_id FROM turns WHERE id = ?1)
                OR d.takeover_turn_id = ?1)
         SELECT t.bot_id, i.id FROM items i JOIN turns t ON t.id = i.turn_id
         JOIN conversations c ON c.id = i.conversation_id AND c.kind = 'owner'
         WHERE i.turn_id = ?1 AND i.kind = 'status' AND EXISTS (SELECT 1 FROM task)
         UNION
         SELECT task.parent_bot_id, task.id FROM task
         WHERE EXISTS (SELECT 1 FROM items WHERE id = task.id)
         UNION
         SELECT a.parent_bot_id, a.id FROM task JOIN delegations a
             ON a.conversation_id = (SELECT conversation_id FROM turns WHERE id = task.root_turn_id)
             OR a.takeover_turn_id = task.root_turn_id
         WHERE EXISTS (SELECT 1 FROM items WHERE id = a.id)",
    )
    .bind(task_turn)
    .fetch_all(&mut *conn)
    .await?;
    for (bot_id, item_id) in shown {
        item_upsert(conn, bot_id, item_id).await?;
    }
    Ok(())
}

/// Republish the turns showing drafts handed to `reply_turn`, so each card
/// follows the reply that holds it until that reply closes it.
async fn reply_drafts_changed(
    conn: &mut SqliteConnection,
    reply_turn: Uuid,
) -> Result<(), sqlx::Error> {
    let shown: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT d.bot_id, d.turn_id, c.id FROM outgoing_drafts d
         JOIN conversations c ON c.bot_id = d.bot_id AND c.kind = 'owner'
         WHERE d.context_turn_id = ?1 AND d.turn_id <> ?1",
    )
    .bind(reply_turn)
    .fetch_all(&mut *conn)
    .await?;
    for (bot_id, turn_id, conversation_id) in shown {
        let turn = turns::load_turn(conn, turn_id).await?;
        let event = Event::TurnUpdated(Box::new(TurnUpdated {
            bot_id: Some(bot_id),
            conversation_id: Some(conversation_id),
            turn: (&turn).into(),
        }));
        append(conn, bot_id, event).await?;
    }
    Ok(())
}

/// A delegated task finished a message: its handoff shows the new progress.
pub async fn task_progressed(
    conn: &mut SqliteConnection,
    task_turn: Uuid,
) -> Result<(), sqlx::Error> {
    root_changed(conn, task_turn).await
}

/// Republish the delegating turns of `task_turn`, a child turn or the owner
/// chat turn that took tasks over, so each handoff card follows its task
/// from queued to running to its outcome.
async fn root_changed(conn: &mut SqliteConnection, task_turn: Uuid) -> Result<(), sqlx::Error> {
    let roots: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT DISTINCT d.parent_bot_id, d.root_turn_id FROM delegations d
         JOIN bots p ON p.id = d.parent_bot_id
         WHERE (d.child_turn_id = ?1 OR d.takeover_turn_id = ?1) AND p.status = 'active'",
    )
    .bind(task_turn)
    .fetch_all(&mut *conn)
    .await?;
    for (bot_id, root_turn) in roots {
        let turn = turns::load_turn(conn, root_turn).await?;
        let event = Event::TurnUpdated(Box::new(TurnUpdated {
            bot_id: Some(bot_id),
            conversation_id: Some(turn.conversation_id),
            turn: (&turn).into(),
        }));
        append(conn, bot_id, event).await?;
    }
    Ok(())
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
