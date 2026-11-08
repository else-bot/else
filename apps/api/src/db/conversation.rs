use sqlx::SqlitePool;
use uuid::Uuid;

use super::bots::{self, BotRecord};
use super::turns::{ITEM_COLUMNS, ItemRecord, TURN_COLUMNS, TurnRecord};

pub struct Snapshot {
    pub bot: BotRecord,
    pub last_event_seq: i64,
    /// Ascending by `seq`.
    pub items: Vec<ItemRecord>,
    /// Turns of the returned items plus every unfinished turn of the bot,
    /// delegated tasks included.
    pub turns: Vec<TurnRecord>,
    /// Pass as `before_seq` to load the previous page; `None` at the start.
    pub older_before_seq: Option<i64>,
    /// The bot's routines, at the same snapshot.
    pub routines: Vec<super::routines::Routine>,
    /// The bot's social triggers, at the same snapshot.
    pub social_triggers: Vec<crate::socials::types::Trigger>,
    /// The bot's current rules version, at the same snapshot.
    pub rules_version: i64,
}

/// Read history, turns, routines, and the event cursor at one database snapshot, so a
/// stream started from the cursor replays exactly what came after.
pub async fn snapshot(
    pool: &SqlitePool,
    bot_id: Uuid,
    limit: i64,
    before_seq: Option<i64>,
) -> Result<Option<Snapshot>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(bot) = bots::find_in(&mut tx, bot_id).await? else {
        return Ok(None);
    };
    let last_event_seq: i64 = sqlx::query_scalar("SELECT last_event_seq FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_one(&mut *tx)
        .await?;
    let mut items: Vec<ItemRecord> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN turns t ON t.id = i.turn_id
         WHERE i.conversation_id = ?1 AND (?2 IS NULL OR i.seq < ?2) AND i.kind NOT IN ('routine','connector') AND NOT i.hidden
         ORDER BY i.seq DESC LIMIT ?3"
    )))
    .bind(bot.conversation_id)
    .bind(before_seq)
    .bind(limit + 1)
    .fetch_all(&mut *tx)
    .await?;
    let has_older = items.len() as i64 > limit;
    items.truncate(limit as usize);
    items.reverse();
    let older_before_seq = has_older.then(|| items[0].seq);
    let turn_ids: Vec<Uuid> = items.iter().map(|item| item.turn_id).collect();
    let mut turns: Vec<TurnRecord> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {TURN_COLUMNS} FROM turns t
         WHERE t.bot_id = ?1
           AND (lower(hex(t.id)) IN (SELECT replace(value, '-', '') FROM json_each(?2)) OR t.status IN ('queued', 'preparing', 'sending', 'running', 'cancelling', 'waiting'))
         ORDER BY t.turn_no"
    )))
    .bind(bot_id)
    .bind(sqlx::types::Json(&turn_ids))
    .fetch_all(&mut *tx)
    .await?;
    super::turns::attach_owner_requests(&mut tx, &mut turns).await?;
    let routines = super::routines::list(&mut tx, bot_id).await?;
    let social_triggers = crate::socials::store::triggers(&mut tx, bot_id).await?;
    let rules_version = super::rules::version(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(Some(Snapshot {
        bot,
        last_event_seq,
        items,
        turns,
        older_before_seq,
        routines,
        social_triggers,
        rules_version,
    }))
}
