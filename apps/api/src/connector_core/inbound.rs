//! Turning recorded inbound events into bot turns.
//!
//! Each admission is one immediate transaction for one bot and one account.
//! It revokes deliveries the bot may no longer receive, then queues one turn
//! whose source is a hidden `connector` item listing up to [`MAX_PER_TURN`]
//! new events, and marks those deliveries delivered with that turn.
//! Deliveries stay pending while the bot has an unfinished turn for the same
//! account, its queue is full, the account is paused, or no provider can run
//! it, so later events join the next turn instead of queueing empty ones.

use sha2::{Digest, Sha256};
use sqlx::sqlite::SqliteRow;
use sqlx::{Encode, FromRow, Sqlite, SqliteConnection, SqlitePool, Type};
use uuid::Uuid;

use crate::app::messages::MAX_QUEUED_TURNS;
use crate::db::turns::UNFINISHED_STATUSES;
use crate::db::{bot_profiles, events};
use crate::providers::Registry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, strum::IntoStaticStr)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub(crate) enum DeliveryStatus {
    Pending,
    Delivered,
    Revoked,
    Cancelled,
}

/// Events one connector turn carries.
pub const MAX_PER_TURN: i64 = 16;
/// Bot and account pairs one scan considers.
const SCAN: i64 = 128;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')";

/// One kind of connected account whose events become turns. SQL fragments
/// bind `?1` to the bot and `?2` to the account.
pub trait Source {
    /// A delivery's event ID, as its table stores it.
    type Event: AsRef<[u8]> + for<'q> Encode<'q, Sqlite> + Type<Sqlite> + Sync;
    /// A pending delivery with what its notice lists.
    type Pending: for<'r> FromRow<'r, SqliteRow> + Send + Unpin;
    /// The deliveries table.
    const DELIVERIES: &'static str;
    /// The deliveries' account column.
    const KEY: &'static str;
    /// The accounts table.
    const ACCOUNTS: &'static str;
    /// A condition on a pending delivery the bot may no longer receive.
    const REVOKED: &'static str;
    /// The account's label, from the accounts table as `a`.
    const LABEL: &'static str;
    /// Up to `?3` pending deliveries, oldest first.
    const PENDING: &'static str;

    fn event_id(pending: &Self::Pending) -> &Self::Event;
    fn notice(label: &str, account: Uuid, pending: &[Self::Pending]) -> String;
}

/// Cancel every pending delivery, as on a restored database.
pub async fn cancel_pending<S: Source>(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {} SET status = ?1, settled_at = {NOW} WHERE status = 'pending'",
        S::DELIVERIES
    )))
    .bind(DeliveryStatus::Cancelled)
    .execute(conn)
    .await?;
    Ok(())
}

/// Queue turns for up to `limit` bot and account pairs with pending events.
/// Returns how many turns were queued. The caller holds a maintenance
/// admission.
pub async fn admit_pending<S: Source>(
    pool: &SqlitePool,
    registry: &Registry,
    limit: i64,
) -> Result<usize, sqlx::Error> {
    if limit <= 0 {
        return Ok(0);
    }
    let (deliveries, key, accounts) = (S::DELIVERIES, S::KEY, S::ACCOUNTS);
    let queued = crate::db::steers::queued_runs("d.bot_id");
    let ready: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT d.bot_id, d.{key} FROM {deliveries} d
         JOIN bots b ON b.id = d.bot_id JOIN {accounts} a ON a.id = d.{key}
         WHERE d.status = 'pending' AND b.status = 'active' AND NOT b.cancel_pending
           AND a.state = 'active'
           AND {queued} < ?2
           AND NOT EXISTS (SELECT 1 FROM {deliveries} x JOIN turns t ON t.id = x.turn_id
                           WHERE x.bot_id = d.bot_id AND x.{key} = d.{key}
                             AND t.status IN {UNFINISHED_STATUSES})
         GROUP BY d.bot_id, d.{key} ORDER BY min(d.created_at), d.bot_id, d.{key}
         LIMIT ?1"
    )))
    .bind(SCAN)
    .bind(MAX_QUEUED_TURNS)
    .fetch_all(pool)
    .await?;
    let mut queued = 0;
    for (bot_id, account_id) in ready {
        if admit::<S>(pool, registry, bot_id, account_id).await? {
            queued += 1;
            if queued as i64 >= limit {
                break;
            }
        }
    }
    Ok(queued)
}

async fn admit<S: Source>(
    pool: &SqlitePool,
    registry: &Registry,
    bot_id: Uuid,
    account_id: Uuid,
) -> Result<bool, sqlx::Error> {
    let (deliveries, key, accounts) = (S::DELIVERIES, S::KEY, S::ACCOUNTS);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE {deliveries} SET status = ?3, settled_at = {NOW}
         WHERE bot_id = ?1 AND {key} = ?2 AND status = 'pending' AND ({})",
        S::REVOKED
    )))
    .bind(bot_id)
    .bind(account_id)
    .bind(DeliveryStatus::Revoked)
    .execute(&mut *tx)
    .await?;
    let target: Option<(Uuid, i64, i64, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT c.id, c.last_turn_no, c.last_item_seq, {}
         FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
         JOIN {accounts} a ON a.id = ?2
         WHERE b.id = ?1 AND b.status = 'active' AND NOT b.cancel_pending AND a.state = 'active'",
        S::LABEL
    )))
    .bind(bot_id)
    .bind(account_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((conversation_id, last_turn_no, last_item_seq, label)) = target else {
        tx.commit().await?;
        return Ok(false);
    };
    let queued = crate::db::steers::queued_runs("?1");
    let busy: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT EXISTS (SELECT 1 FROM {deliveries} x JOIN turns t ON t.id = x.turn_id
                        WHERE x.bot_id = ?1 AND x.{key} = ?2 AND t.status IN {UNFINISHED_STATUSES})
             OR {queued} >= ?3"
    )))
    .bind(bot_id)
    .bind(account_id)
    .bind(MAX_QUEUED_TURNS)
    .fetch_one(&mut *tx)
    .await?;
    let chosen = if busy {
        None
    } else {
        bot_profiles::for_admission(&mut tx, registry, bot_id).await?
    };
    let Some(bot_profiles::TurnProvider {
        provider_id,
        profile,
    }) = chosen
    else {
        tx.commit().await?;
        return Ok(false);
    };
    let pending: Vec<S::Pending> = sqlx::query_as(sqlx::AssertSqlSafe(S::PENDING))
        .bind(bot_id)
        .bind(account_id)
        .bind(MAX_PER_TURN)
        .fetch_all(&mut *tx)
        .await?;
    if pending.is_empty() {
        tx.commit().await?;
        return Ok(false);
    }
    let turn_id = Uuid::new_v4();
    let item_id = Uuid::new_v4();
    let request_id = Uuid::new_v4();
    let turn_no = last_turn_no + 1;
    let seq = last_item_seq + 1;
    let mut hasher = Sha256::new();
    hasher.update(b"connector-delivery\0");
    for p in &pending {
        hasher.update(S::event_id(p).as_ref());
    }
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, profile_revision, profile)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(turn_id)
    .bind(bot_id)
    .bind(conversation_id)
    .bind(request_id)
    .bind(hasher.finalize().to_vec())
    .bind(turn_no)
    .bind(item_id)
    .bind(provider_id)
    .bind(profile.as_ref().map(|(revision, _)| *revision))
    .bind(profile.as_ref().map(|(_, profile)| profile))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, ?3, ?4, 'connector', ?5)",
    )
    .bind(item_id)
    .bind(conversation_id)
    .bind(seq)
    .bind(turn_id)
    .bind(S::notice(&label, account_id, &pending))
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET last_turn_no = ?2, last_item_seq = ?3 WHERE id = ?1")
        .bind(conversation_id)
        .bind(turn_no)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    for p in &pending {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {deliveries} SET status = ?5, turn_id = ?3, settled_at = {NOW}
             WHERE event_id = ?1 AND bot_id = ?2 AND {key} = ?4 AND status = 'pending'"
        )))
        .bind(S::event_id(p))
        .bind(bot_id)
        .bind(turn_id)
        .bind(account_id)
        .bind(DeliveryStatus::Delivered)
        .execute(&mut *tx)
        .await?;
    }
    events::turn_accepted(&mut tx, bot_id, turn_id).await?;
    tx.commit().await?;
    Ok(true)
}
