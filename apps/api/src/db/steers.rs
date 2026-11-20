//! Owner messages that join the work already under way instead of stopping
//! it.
//!
//! Every owner message is placed ([`place`]): it joins the conversation's
//! unfinished reply, or a delegated task that waits on the owner here, or
//! it queues as its own reply. Only Stop cancels work.
//!
//! A joined message keeps its own turn, `queued` but never claimed while it
//! has a steer row. It reaches the reply one of two ways. A running attempt
//! whose provider takes input mid-turn marks it `sending` before it writes
//! it ([`sending`]) and `delivered` on the provider's echo ([`deliver`]),
//! unless the attempt has parked by then; a provider that answered that it
//! took nothing returns it to `pending` ([`refuse`]). A parked attempt
//! refuses every tool call, so it takes no message: none is written into
//! it, and one echoed after the park returns to `pending`. Otherwise it waits `pending` until the reply's next
//! attempt, whose request carries it ([`fold`]). A delivered message stays
//! part of that reply for good: it ends with the reply's outcome
//! ([`settle`]) and keeps its row, so the chat shows one answer to both.

use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::turns::{ACTIVE_STATUSES, TurnStatus};
use crate::app::messages::MessagePart;
use crate::app::providers::Steer;
use crate::db::in_ids;

/// How far the message got into the reply it joined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, strum::IntoStaticStr, serde::Serialize)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Nothing was written to the provider.
    Pending,
    /// Written, or possibly written, with no echo yet: delivery is unknown.
    Sending,
    /// The provider took it into the reply.
    Delivered,
}

/// SQL condition on `turns t`: the turn is steered into another one, so
/// the scheduler must not claim it.
pub const STEERED: &str = "EXISTS (SELECT 1 FROM turn_steers s WHERE s.turn_id = t.id)";

/// SQL expression: how many turns of the bot that the SQL expression `bot`
/// names wait to be claimed. A joined message is never claimed, so it does
/// not count toward the queue limit.
pub fn queued_runs(bot: &str) -> String {
    format!(
        "(SELECT count(*) FROM turns q WHERE q.bot_id = {bot} AND q.status = 'queued'
          AND NOT EXISTS (SELECT 1 FROM turn_steers s WHERE s.turn_id = q.id))"
    )
}

/// SQL condition on `turns t`: the turn waits on an owner card (question,
/// secret, draft, connection, skill install, or computer handoff).
pub const WAITS_ON_OWNER: &str = "t.id IN (
    SELECT turn_id FROM owner_questions WHERE status = 'pending'
    UNION ALL SELECT turn_id FROM secret_requests WHERE status = 'pending'
    UNION ALL SELECT turn_id FROM outgoing_drafts WHERE status = 'pending'
    UNION ALL SELECT turn_id FROM social_setups WHERE status = 'pending'
    UNION ALL SELECT turn_id FROM skill_installs WHERE status = 'pending'
    UNION ALL SELECT turn_id FROM computer_handoffs WHERE status = 'pending' AND attempt > 0)";

/// SQL subquery: the turns of the owner messages delivered into the turn
/// that the SQL expression `turn` names. Their authority (a quote, an open
/// draft) is that turn's own.
pub fn delivered_into(turn: &str) -> String {
    format!(
        "SELECT s.turn_id FROM turn_steers s
         WHERE s.into_turn_id = {turn} AND s.status = 'delivered'"
    )
}

/// SQL subquery: the turns of the owner messages joined to the turn that
/// the SQL expression `turn` names and still part of it, whether given to
/// the provider yet or not.
pub fn joined_into(turn: &str) -> String {
    format!(
        "SELECT s.turn_id FROM turn_steers s JOIN turns m ON m.id = s.turn_id
         WHERE s.into_turn_id = {turn} AND s.status IN ('pending', 'delivered')
           AND m.status = 'queued'"
    )
}

/// Where a new owner message goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// It joined this unfinished turn, which was `status` before the join.
    Joined { into: Uuid, status: TurnStatus },
    /// It runs as its own reply once the bot is free.
    Queued,
}

impl Placement {
    /// The running reply to wake so it takes the message now. A parked one
    /// reads it when it continues.
    pub fn wakes(self) -> Option<Uuid> {
        match self {
            Self::Joined { into, status } if status.is_active() => Some(into),
            _ => None,
        }
    }
}

/// The new owner message `turn_id` (number `turn_no` of `conversation_id`,
/// bound to `provider_id`) joins the work under way, in order:
///
/// 1. the conversation's newest unfinished reply to an owner message, when
///    it runs on the same provider (another provider queues the message:
///    the owner chose a different one for it);
/// 2. with no such reply, the one delegated task of this bot on the same
///    provider whose card waits on the owner;
/// 3. otherwise it queues.
///
/// A message that may answer a card (`typed`: it targets no notice, draft
/// or reaction) and says only to always allow first answers the card it
/// replies to with its Always choice
/// ([`super::owner_questions::answer_typed`]). Joining skips
/// the target's other pending questions and computer handoff, as an owner
/// who types instead of clicking does, and continues a waiting target at
/// once with every other hold kept. Runs in the accepting immediate
/// transaction.
pub async fn place(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    conversation_id: Uuid,
    turn_no: i64,
    turn_id: Uuid,
    provider_id: &str,
    typed: bool,
) -> Result<Placement, sqlx::Error> {
    super::delegations::lock_targets_of_bot(&mut *conn, bot_id).await?;
    let target = target(conn, bot_id, conversation_id, turn_no, provider_id).await?;
    let text: Option<String> = sqlx::query_scalar(
        "SELECT i.text FROM turns t JOIN items i ON i.id = t.owner_item_id
         WHERE t.id = ?1 AND i.kind = 'user' AND NOT i.hidden",
    )
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(text) = text.filter(|_| typed) {
        let joined = target.as_ref().map(|t| t.id);
        super::owner_questions::answer_typed(conn, bot_id, turn_id, joined, &text).await?;
    }
    let Some(target) = target else {
        return Ok(Placement::Queued);
    };
    sqlx::query("INSERT INTO turn_steers (turn_id, into_turn_id) VALUES (?1, ?2)")
        .bind(turn_id)
        .bind(target.id)
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "UPDATE owner_questions SET status = 'skipped',
             settled_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE turn_id = ?1 AND status = 'pending'",
    )
    .bind(target.id)
    .execute(&mut *conn)
    .await?;
    super::computer_handoffs::skip_pending_of(&mut *conn, target.id).await?;
    if target.status == TurnStatus::Waiting {
        super::turns::continue_waiting(&mut *conn, target.id).await?;
    }
    super::events::turn_changed(&mut *conn, target.bot_id, target.id).await?;
    Ok(Placement::Joined {
        into: target.id,
        status: target.status,
    })
}

/// The unfinished turn a new owner message joins, as [`place`] orders them.
#[derive(sqlx::FromRow)]
struct Target {
    id: Uuid,
    bot_id: Uuid,
    status: TurnStatus,
    provider_id: String,
}

async fn target(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    conversation_id: Uuid,
    turn_no: i64,
    provider_id: &str,
) -> Result<Option<Target>, sqlx::Error> {
    let reply: Option<Target> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT t.id, t.bot_id, t.status, t.provider_id FROM turns t
         JOIN items i ON i.id = t.owner_item_id AND i.kind = 'user' AND NOT i.hidden
         WHERE t.conversation_id = ?1 AND t.turn_no < ?2 AND NOT t.deferred AND NOT {STEERED}
           AND t.status IN ('queued', 'preparing', 'sending', 'running', 'waiting')
         ORDER BY t.turn_no DESC LIMIT 1"
    )))
    .bind(conversation_id)
    .bind(turn_no)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(reply) = reply {
        return Ok((reply.provider_id == provider_id).then_some(reply));
    }
    let mut tasks: Vec<Target> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT t.id, t.bot_id, t.status, t.provider_id FROM delegations d JOIN turns t ON t.id = d.child_turn_id
         WHERE d.target_bot_id = ?1 AND d.status IN ('queued', 'running')
           AND d.takeover_turn_id IS NULL AND NOT {STEERED}
           AND t.status IN ('queued', 'preparing', 'sending', 'running', 'waiting')
           AND t.provider_id = ?2 AND {WAITS_ON_OWNER}
         LIMIT 2"
    )))
    .bind(bot_id)
    .bind(provider_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok((tasks.len() == 1).then(|| tasks.remove(0)))
}

/// The pending messages steered into running turn `into`, oldest first,
/// whose turns are still queued, with their files. None while its attempt
/// is parked: that attempt refuses every tool call, so the message waits
/// for the next attempt, which starts as soon as the parked one ends.
pub async fn pending(pool: &SqlitePool, into: Uuid) -> Result<Vec<Steer>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let rows = joined(&mut conn, into).await?;
    let mut steers = Vec::with_capacity(rows.len());
    for row in rows {
        if row.status != Status::Pending || !row.reply_running {
            continue;
        }
        let ids: Vec<Uuid> = file_ids(&row.parts.0).collect();
        let files = super::attachments::load(&mut conn, row.bot_id, &ids).await?;
        steers.push(row.into_steer(files));
    }
    Ok(steers)
}

/// The messages joined to `into` that its next request carries, oldest
/// first: those already delivered (`seen` by the provider in an earlier
/// attempt) and those still pending, which [`fold`] marks delivered when
/// the request is sent. Their files are left for the caller to load.
pub async fn for_request(
    conn: &mut SqliteConnection,
    into: Uuid,
) -> Result<Vec<crate::app::providers::Steered>, sqlx::Error> {
    Ok(joined(conn, into)
        .await?
        .into_iter()
        .map(|row| crate::app::providers::Steered {
            seen: row.status == Status::Delivered,
            message: row.into_steer(Vec::new()),
        })
        .collect())
}

/// The messages joined to `into` and not sent yet or taken ([`joined_into`]),
/// oldest first.
async fn joined(conn: &mut SqliteConnection, into: Uuid) -> Result<Vec<Row>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT t.id AS turn_id, t.bot_id, i.text, coalesce(i.parts, '[]') AS parts, s.status,
                a.status = 'running' AND NOT {} AS reply_running
         FROM turn_steers s
         JOIN turns t ON t.id = s.turn_id AND t.status = 'queued'
         JOIN items i ON i.id = t.owner_item_id
         JOIN turns a ON a.id = s.into_turn_id
         WHERE s.into_turn_id = ?1 AND s.status IN ('pending', 'delivered')
         ORDER BY t.turn_no",
        super::turns::parked("a.id", "a.attempt")
    )))
    .bind(into)
    .fetch_all(&mut *conn)
    .await
}

#[derive(sqlx::FromRow)]
struct Row {
    turn_id: Uuid,
    bot_id: Uuid,
    text: String,
    parts: Json<Vec<MessagePart>>,
    status: Status,
    reply_running: bool,
}

impl Row {
    fn into_steer(self, files: Vec<crate::app::providers::InputFile>) -> Steer {
        Steer {
            turn_id: self.turn_id,
            text: self.text,
            parts: self.parts.0,
            files,
        }
    }
}

/// The files a message's `parts` name.
pub fn file_ids(parts: &[MessagePart]) -> impl Iterator<Item = Uuid> + '_ {
    parts.iter().filter_map(|part| match part {
        MessagePart::File { file } => Some(file.id),
        MessagePart::Text { .. } => None,
    })
}

/// The request of `into` that carries the pending messages `turn_ids` is
/// being sent: they are part of it from here, as if echoed. Runs in the
/// transaction that moves `into` to `sending`.
pub async fn fold(
    conn: &mut SqliteConnection,
    into: Uuid,
    turn_ids: &[Uuid],
) -> Result<(), sqlx::Error> {
    if turn_ids.is_empty() {
        return Ok(());
    }
    let folded: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "UPDATE turn_steers SET status = 'delivered'
         WHERE into_turn_id = ?1 AND status = 'pending'
           AND {}
           AND EXISTS (SELECT 1 FROM turns WHERE id = turn_steers.turn_id AND status = 'queued')
         RETURNING turn_id",
        in_ids("turn_id", "?2")
    )))
    .bind(into)
    .bind(Json(turn_ids))
    .fetch_all(&mut *conn)
    .await?;
    for turn_id in folded {
        changed(conn, turn_id).await?;
    }
    Ok(())
}

/// Republish message `turn_id`, whose steer moved.
async fn changed(conn: &mut SqliteConnection, turn_id: Uuid) -> Result<(), sqlx::Error> {
    let bot_id: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(turn_id)
        .fetch_one(&mut *conn)
        .await?;
    super::events::turn_changed(conn, bot_id, turn_id).await
}

/// Record, before writing it, that message `turn_id` goes to running turn
/// `into`. From here a crash leaves its delivery unknown. False when it is
/// no longer pending there (the owner stopped it, or the reply ended) or
/// the reply's attempt has parked.
pub async fn sending(pool: &SqlitePool, turn_id: Uuid, into: Uuid) -> Result<bool, sqlx::Error> {
    advance(pool, turn_id, into, Status::Pending, Status::Sending).await
}

/// The provider echoed message `turn_id` into running turn `into`. An
/// attempt that has parked refuses every tool call, so it cannot act on
/// the message: it returns to `pending`, and the next attempt, which starts
/// as soon as this one ends, carries it again. False when it was not
/// `sending` there.
pub async fn deliver(pool: &SqlitePool, turn_id: Uuid, into: Uuid) -> Result<bool, sqlx::Error> {
    advance(pool, turn_id, into, Status::Sending, Status::Delivered).await
}

/// The provider answered that it did not take message `turn_id` (its turn
/// had already ended): it is pending again, for the reply's next attempt.
/// False when it was not `sending` there.
pub async fn refuse(pool: &SqlitePool, turn_id: Uuid, into: Uuid) -> Result<bool, sqlx::Error> {
    advance(pool, turn_id, into, Status::Sending, Status::Pending).await
}

async fn advance(
    pool: &SqlitePool,
    turn_id: Uuid,
    into: Uuid,
    from: Status,
    to: Status,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let parked = super::turns::parked("a.id", "a.attempt");
    let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE turn_steers SET status = CASE
             WHEN ?4 = 'delivered' AND (SELECT {parked} FROM turns a WHERE a.id = ?2)
             THEN 'pending' ELSE ?4 END
         WHERE turn_id = ?1 AND into_turn_id = ?2 AND status = ?3
           AND EXISTS (SELECT 1 FROM turns WHERE id = ?1 AND status = 'queued')
           AND EXISTS (SELECT 1 FROM turns a WHERE a.id = ?2 AND a.status = 'running'
                       AND (?4 <> 'sending' OR NOT {parked}))"
    )))
    .bind(turn_id)
    .bind(into)
    .bind(from)
    .bind(to)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if updated {
        changed(&mut tx, turn_id).await?;
    }
    tx.commit().await?;
    Ok(updated)
}

/// How many messages joined to `into` still wait for it to take them. A
/// reply that parks with any continues at once instead of waiting.
pub async fn waiting_for(conn: &mut SqliteConnection, into: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) FROM turn_steers s JOIN turns t ON t.id = s.turn_id
         WHERE s.into_turn_id = ?1 AND s.status = 'pending' AND t.status = 'queued'",
    )
    .bind(into)
    .fetch_one(&mut *conn)
    .await
}

/// What [`settle`] does with a steer whose message or reply moved on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// The steer goes: the message runs on its own, or had already ended.
    Released,
    /// It may have reached the provider: the message ends uncertain.
    Uncertain,
    /// The reply took it and ended: the message ends the same way.
    EndsWithReply,
}

/// Settle the steers whose message or reply is no longer where the steer
/// left it. A message that already ended (the owner stopped it) drops its
/// steer unless the reply had taken it. While the reply is parked, a
/// pending or delivered message stays with it for its next attempt; one
/// still `sending` may have reached the provider, so its turn ends
/// uncertain and is never resent. Once the reply has ended, a pending
/// message is released and runs on its own, and a delivered one ends with
/// the reply's outcome. Idempotent.
pub async fn settle(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let settled: Vec<(Uuid, Uuid, TurnStatus, Status, Uuid, TurnStatus)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT t.id, t.bot_id, t.status, s.status, a.id, a.status FROM turn_steers s
             JOIN turns t ON t.id = s.turn_id
             JOIN turns a ON a.id = s.into_turn_id
             WHERE (a.status NOT IN {ACTIVE_STATUSES} OR t.status <> 'queued')
               AND NOT (s.status = 'delivered' AND t.status <> 'queued')"
        )))
        .fetch_all(&mut *conn)
        .await?;
    for (turn_id, bot_id, own, status, into, into_status) in settled {
        let fate = match (own, status, into_status.is_ended()) {
            (TurnStatus::Queued, Status::Pending | Status::Delivered, false) => continue,
            (TurnStatus::Queued, Status::Sending, _) => Fate::Uncertain,
            (TurnStatus::Queued, Status::Delivered, true) => Fate::EndsWithReply,
            _ => Fate::Released,
        };
        if fate != Fate::EndsWithReply {
            sqlx::query("DELETE FROM turn_steers WHERE turn_id = ?1")
                .bind(turn_id)
                .execute(&mut *conn)
                .await?;
        }
        let updated = match fate {
            Fate::Released => {
                crate::db::events::turn_changed(&mut *conn, bot_id, turn_id).await?;
                continue;
            }
            Fate::Uncertain => sqlx::query(
                "UPDATE turns SET status = 'uncertain', effect_uncertain = true,
                     error_code = 'steer_uncertain',
                     error_message = 'This message was sent into the reply in progress, which \
did not confirm it. It was not resent; send a follow-up to continue.',
                     finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1 AND status = 'queued'",
            )
            .bind(turn_id),
            Fate::EndsWithReply => sqlx::query(
                "UPDATE turns SET status = a.status, error_code = a.error_code,
                     error_message = a.error_message,
                     effect_uncertain = turns.effect_uncertain OR a.effect_uncertain,
                     finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 FROM turns a
                 WHERE turns.id = ?1 AND turns.status = 'queued' AND a.id = ?2",
            )
            .bind(turn_id)
            .bind(into),
        }
        .execute(&mut *conn)
        .await?
        .rows_affected();
        if updated == 1 {
            crate::db::events::turn_changed(&mut *conn, bot_id, turn_id).await?;
            crate::db::memory::log_turn_end(&mut *conn, turn_id).await?;
        }
    }
    Ok(())
}
