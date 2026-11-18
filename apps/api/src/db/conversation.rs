use else_plain_text::find::Needle;
use sqlx::SqlitePool;
use uuid::Uuid;

use super::bots::{self, BotRecord};
use super::turns::{ITEM_COLUMNS, ItemRecord, TURN_COLUMNS, TurnRecord, UNFINISHED_STATUSES};

/// The items a conversation shows the owner.
const SHOWN: &str = "i.kind NOT IN ('routine','connector') AND NOT i.hidden";

pub struct Snapshot {
    pub bot: BotRecord,
    pub last_event_seq: i64,
    /// Ascending by `seq`.
    pub items: Vec<ItemRecord>,
    /// Turns of the returned items plus every unfinished turn of the bot,
    /// delegated tasks included, and any turn with a pending computer handoff.
    pub turns: Vec<TurnRecord>,
    /// Pass as `before_seq` to load the previous page; `None` at the start.
    pub older_before_seq: Option<i64>,
    /// The bot's routines, at the same snapshot.
    pub routines: Vec<super::routines::Routine>,
    /// The bot's social triggers, at the same snapshot.
    pub social_triggers: Vec<crate::socials::types::Trigger>,
    /// The bot's mail triggers, at the same snapshot.
    pub mail_triggers: Vec<crate::connectors::types::MailTrigger>,
    /// The kinds of drafts the owner approves always.
    pub draft_grants: Vec<crate::app::outgoing::DraftKind>,
    /// The bot's webhooks, at the same snapshot.
    pub webhooks: Vec<super::webhooks::Webhook>,
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
         WHERE i.conversation_id = ?1 AND (?2 IS NULL OR i.seq < ?2) AND {SHOWN}
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
           AND (lower(hex(t.id)) IN (SELECT replace(value, '-', '') FROM json_each(?2)) OR t.status IN {UNFINISHED_STATUSES}
                OR EXISTS (SELECT 1 FROM computer_handoffs h WHERE h.turn_id = t.id AND h.status IN ('pending', 'taken'))
                OR EXISTS (SELECT 1 FROM owner_notices n WHERE n.turn_id = t.id AND n.status = 'pending'))
         ORDER BY t.turn_no"
    )))
    .bind(bot_id)
    .bind(sqlx::types::Json(&turn_ids))
    .fetch_all(&mut *tx)
    .await?;
    super::turns::attach_owner_requests(&mut tx, &mut turns).await?;
    let routines = super::routines::list(&mut tx, bot_id).await?;
    let social_triggers = crate::socials::store::triggers(&mut tx, bot_id).await?;
    let mail_triggers = crate::connectors::store::mail_triggers(&mut tx, bot_id).await?;
    let webhooks = super::webhooks::list(&mut tx, bot_id).await?;
    let draft_grants = super::outgoing_drafts::always_approved(&mut tx, bot_id).await?;
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
        mail_triggers,
        draft_grants,
        webhooks,
        rules_version,
    }))
}

/// The owner conversation of `bot_id` and the bot's status, without the
/// activity a full bot record computes.
pub async fn owner_conversation(
    conn: &mut sqlx::SqliteConnection,
    bot_id: Uuid,
) -> Result<Option<(Uuid, bots::BotStatus)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT c.id, b.status FROM bots b
         JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner' WHERE b.id = ?1",
    )
    .bind(bot_id)
    .fetch_optional(conn)
    .await
}

/// One message that matches a search.
pub struct Hit {
    pub item_id: Uuid,
    pub seq: i64,
    /// How often the query shows in the message.
    pub count: usize,
}

/// The owner messages and bot replies of `conversation_id` that contain
/// `needle` as rendered, newest first.
pub async fn search(
    pool: &SqlitePool,
    conversation_id: Uuid,
    needle: Needle,
) -> Result<Vec<Hit>, sqlx::Error> {
    let rows: Vec<(Uuid, i64, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT i.id, i.seq, i.kind, i.text FROM items i
         WHERE i.conversation_id = ?1 AND i.kind IN ('user','assistant') AND {SHOWN}
         ORDER BY i.seq DESC"
    )))
    .bind(conversation_id)
    .fetch_all(pool)
    .await?;
    let hits = tokio::task::spawn_blocking(move || {
        rows.into_iter()
            .filter_map(|(item_id, seq, kind, text)| {
                let shown = match kind.as_str() {
                    "user" => quoted_reply_body(&text).unwrap_or(&text),
                    _ => &text,
                };
                let count = needle.count_in_markdown(shown);
                (count > 0).then_some(Hit {
                    item_id,
                    seq,
                    count,
                })
            })
            .collect()
    })
    .await
    .map_err(|err| sqlx::Error::Protocol(format!("search task failed: {err}")))?;
    Ok(hits)
}

/// The body of an owner reply that quotes one earlier message, which the
/// app shows under a folded quote card. Matches the app's `QuotedReply`.
fn quoted_reply_body(text: &str) -> Option<&str> {
    let rest = [
        "Replying to my earlier message:\n",
        "Replying to your earlier message:\n",
    ]
    .into_iter()
    .find_map(|prefix| text.strip_prefix(prefix))?;
    let (quote, body) = rest.split_once("\n\n")?;
    (!quote.is_empty()
        && !body.trim().is_empty()
        && quote
            .lines()
            .all(|line| line == ">" || line.starts_with("> ")))
    .then_some(body)
}

/// A stretch of shown items to load between two the owner already has.
pub struct WindowQuery {
    /// Up to `older` items at or before this seq, and up to `newer` after it.
    pub anchor_seq: i64,
    pub older: i64,
    pub newer: i64,
    /// The loaded item just below the stretch, if any.
    pub after_seq: Option<i64>,
    /// The loaded item just above the stretch, if any.
    pub before_seq: Option<i64>,
}

pub struct Window {
    pub status: bots::BotStatus,
    /// Ascending by `seq`.
    pub items: Vec<ItemRecord>,
    /// The turns of the returned items.
    pub turns: Vec<TurnRecord>,
    /// Shown items still missing between `after_seq` and the first item.
    pub older_gap: i64,
    /// Shown items still missing between the last item and `before_seq`.
    pub newer_gap: i64,
}

/// Read the shown items around one message, and how many are still missing
/// on each side, at one database snapshot.
pub async fn window(
    pool: &SqlitePool,
    bot_id: Uuid,
    query: WindowQuery,
) -> Result<Option<Window>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some((conversation, status)) = owner_conversation(&mut tx, bot_id).await? else {
        return Ok(None);
    };
    let low = query.after_seq.unwrap_or(0);
    let high = query.before_seq.unwrap_or(i64::MAX);
    let mut items: Vec<ItemRecord> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN turns t ON t.id = i.turn_id
         WHERE i.conversation_id = ?1 AND i.seq > ?2 AND i.seq <= ?3 AND i.seq < ?4 AND {SHOWN}
         ORDER BY i.seq DESC LIMIT ?5"
    )))
    .bind(conversation)
    .bind(low)
    .bind(query.anchor_seq)
    .bind(high)
    .bind(query.older)
    .fetch_all(&mut *tx)
    .await?;
    items.reverse();
    let newer: Vec<ItemRecord> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN turns t ON t.id = i.turn_id
         WHERE i.conversation_id = ?1 AND i.seq > ?2 AND i.seq > ?3 AND i.seq < ?4 AND {SHOWN}
         ORDER BY i.seq LIMIT ?5"
    )))
    .bind(conversation)
    .bind(low)
    .bind(query.anchor_seq)
    .bind(high)
    .bind(query.newer)
    .fetch_all(&mut *tx)
    .await?;
    items.extend(newer);
    let shown_between = |low: i64, high: i64| {
        sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM items i
             WHERE i.conversation_id = ?1 AND i.seq > ?2 AND i.seq < ?3 AND {SHOWN}"
        )))
        .bind(conversation)
        .bind(low)
        .bind(high)
    };
    let (older_gap, newer_gap) = match (items.first(), items.last()) {
        (Some(first), Some(last)) => (
            shown_between(low, first.seq).fetch_one(&mut *tx).await?,
            shown_between(last.seq, high).fetch_one(&mut *tx).await?,
        ),
        _ => (shown_between(low, high).fetch_one(&mut *tx).await?, 0),
    };
    let turn_ids: Vec<Uuid> = items.iter().map(|item| item.turn_id).collect();
    let mut turns: Vec<TurnRecord> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {TURN_COLUMNS} FROM turns t
         WHERE t.bot_id = ?1
           AND lower(hex(t.id)) IN (SELECT replace(value, '-', '') FROM json_each(?2))
         ORDER BY t.turn_no"
    )))
    .bind(bot_id)
    .bind(sqlx::types::Json(&turn_ids))
    .fetch_all(&mut *tx)
    .await?;
    super::turns::attach_owner_requests(&mut tx, &mut turns).await?;
    tx.commit().await?;
    Ok(Some(Window {
        status,
        items,
        turns,
        older_gap,
        newer_gap,
    }))
}
