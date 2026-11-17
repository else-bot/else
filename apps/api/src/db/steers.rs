//! Owner messages that join the conversation's running reply instead of
//! stopping it.
//!
//! A steered message keeps its own turn, `queued` but never claimed while
//! it has a steer row. The running attempt marks a pending steer `sending`
//! before it writes it ([`sending`]) and `delivered` on the provider's echo
//! ([`deliver`]). A delivered message stays part of that reply, through its
//! continuations, until the reply ends ([`settle`]).

use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::turns::{ACTIVE_STATUSES, SendRequest, TurnStatus, UNFINISHED_STATUSES};
use crate::app::messages::MessagePart;
use crate::app::providers::Steer;

/// How far the message got into the running reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, sqlx::Type, strum::IntoStaticStr)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Status {
    /// Nothing was written to the provider.
    Pending,
    /// Written, or possibly written, with no echo yet: delivery is unknown.
    Sending,
    /// The provider echoed it into the reply.
    Delivered,
}

/// SQL condition on `turns t`: the turn is steered into another one, so
/// the scheduler must not claim it.
pub const STEERED: &str = "EXISTS (SELECT 1 FROM turn_steers s WHERE s.turn_id = t.id)";

/// Join owner message `req`, accepted as turn `turn_id` (number `turn_no`
/// of `conversation_id`, with profile `revision`), to the conversation's
/// running reply instead of stopping it. Returns that reply, or `None` when
/// the message must stop it as before.
///
/// Only a plain request on a provider that steers joins: no files,
/// reaction, open draft, answer to an ask, or setup flag. The reply must run
/// on the same provider and profile, be the conversation's only unfinished
/// work apart from messages already steered into it, wait on no owner
/// card, and the bot must run no delegated task an owner message would
/// take over.
pub async fn join(
    conn: &mut SqliteConnection,
    req: &SendRequest<'_>,
    conversation_id: Uuid,
    turn_no: i64,
    turn_id: Uuid,
    revision: Option<i64>,
) -> Result<Option<Uuid>, sqlx::Error> {
    let plain = req.steers
        && !req.hidden
        && req.reaction.is_none()
        && req.open_draft.is_none()
        && req.answers_notice.is_none()
        && req
            .parts
            .iter()
            .all(|part| matches!(part, MessagePart::Text { .. }));
    if !plain {
        return Ok(None);
    }
    let into: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT t.id FROM turns t
         JOIN items i ON i.id = t.owner_item_id AND i.kind = 'user' AND NOT i.hidden
         WHERE t.conversation_id = ?1 AND t.turn_no < ?2 AND t.status = 'running'
           AND t.provider_id = ?3 AND t.profile_revision IS ?4
           AND NOT EXISTS (
               SELECT 1 FROM turns o
               WHERE o.conversation_id = ?1 AND o.turn_no < ?2 AND o.id <> t.id
                 AND o.status IN {UNFINISHED_STATUSES}
                 AND NOT EXISTS (SELECT 1 FROM turn_steers s
                                 WHERE s.turn_id = o.id AND s.into_turn_id = t.id))
           AND NOT EXISTS (SELECT 1 FROM turn_holds h
                           WHERE h.turn_id = t.id AND h.pending AND h.kind = 'owner')
           AND NOT EXISTS (SELECT 1 FROM owner_questions q
                           WHERE q.turn_id = t.id AND q.status = 'pending')
           AND NOT EXISTS (SELECT 1 FROM delegations d
                           WHERE d.target_bot_id = ?5 AND d.status IN ('queued', 'running'))"
    )))
    .bind(conversation_id)
    .bind(turn_no)
    .bind(req.provider_id)
    .bind(revision)
    .bind(req.bot_id)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(into) = into {
        sqlx::query("INSERT INTO turn_steers (turn_id, into_turn_id) VALUES (?1, ?2)")
            .bind(turn_id)
            .bind(into)
            .execute(&mut *conn)
            .await?;
    }
    Ok(into)
}

/// The pending messages steered into running turn `into`, oldest first,
/// whose turns are still queued.
pub async fn pending(pool: &SqlitePool, into: Uuid) -> Result<Vec<Steer>, sqlx::Error> {
    sqlx::query_as(
        "SELECT t.id AS turn_id, i.text FROM turn_steers s
         JOIN turns t ON t.id = s.turn_id AND t.status = 'queued'
         JOIN items i ON i.id = t.owner_item_id
         JOIN turns a ON a.id = s.into_turn_id AND a.status = 'running'
         WHERE s.into_turn_id = ?1 AND s.status = 'pending'
         ORDER BY t.turn_no",
    )
    .bind(into)
    .fetch_all(pool)
    .await
}

/// Record, before writing it, that message `turn_id` goes to running turn
/// `into`. From here a crash leaves its delivery unknown. False when it is
/// no longer pending there (the owner stopped it, or the reply ended).
pub async fn sending(pool: &SqlitePool, turn_id: Uuid, into: Uuid) -> Result<bool, sqlx::Error> {
    advance(pool, turn_id, into, Status::Pending, Status::Sending).await
}

/// The provider echoed message `turn_id` into running turn `into`. False
/// when it was not `sending` there.
pub async fn deliver(pool: &SqlitePool, turn_id: Uuid, into: Uuid) -> Result<bool, sqlx::Error> {
    advance(pool, turn_id, into, Status::Sending, Status::Delivered).await
}

async fn advance(
    pool: &SqlitePool,
    turn_id: Uuid,
    into: Uuid,
    from: Status,
    to: Status,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE turn_steers SET status = ?4
         WHERE turn_id = ?1 AND into_turn_id = ?2 AND status = ?3
           AND EXISTS (SELECT 1 FROM turns WHERE id = ?1 AND status = 'queued')
           AND EXISTS (SELECT 1 FROM turns WHERE id = ?2 AND status = 'running')",
    )
    .bind(turn_id)
    .bind(into)
    .bind(from)
    .bind(to)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(updated == 1)
}

/// The texts of the messages delivered into `into`, oldest first. They are
/// part of its request when it continues.
pub async fn delivered(
    conn: &mut SqliteConnection,
    into: Uuid,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT i.text FROM turn_steers s
         JOIN turns t ON t.id = s.turn_id
         JOIN items i ON i.id = t.owner_item_id
         WHERE s.into_turn_id = ?1 AND s.status = 'delivered'
         ORDER BY t.turn_no",
    )
    .bind(into)
    .fetch_all(&mut *conn)
    .await
}

/// Settle the steers into turns that are no longer running. A pending
/// message is released and runs on its own. A `sending` one may have
/// reached the provider, so its turn ends uncertain and is never resent. A
/// delivered one waits while its reply parks and continues, and ends with
/// that reply's outcome. Steers of turns that already ended (the owner
/// stopped them) are dropped. Idempotent.
pub async fn settle(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    let settled: Vec<(Uuid, Uuid, Status, Uuid, TurnStatus)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT t.id, t.bot_id, s.status, a.id, a.status FROM turn_steers s
             JOIN turns t ON t.id = s.turn_id
             JOIN turns a ON a.id = s.into_turn_id
             WHERE a.status NOT IN {ACTIVE_STATUSES}"
        )))
        .fetch_all(&mut *conn)
        .await?;
    for (turn_id, bot_id, status, into, into_status) in settled {
        if status == Status::Delivered && !into_status.is_ended() {
            continue;
        }
        sqlx::query("DELETE FROM turn_steers WHERE turn_id = ?1")
            .bind(turn_id)
            .execute(&mut *conn)
            .await?;
        let updated = match status {
            Status::Pending => {
                crate::db::events::turn_changed(&mut *conn, bot_id, turn_id).await?;
                continue;
            }
            Status::Sending => sqlx::query(
                "UPDATE turns SET status = 'uncertain', effect_uncertain = true,
                     error_code = 'steer_uncertain',
                     error_message = 'This message was sent into the reply in progress, which \
ended before confirming it. It was not resent; send a follow-up to continue.',
                     finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE id = ?1 AND status = 'queued'",
            )
            .bind(turn_id),
            Status::Delivered => sqlx::query(
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
