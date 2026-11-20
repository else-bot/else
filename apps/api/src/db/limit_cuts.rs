//! Attempts cut at the attempt limit ([`crate::providers::cli::attempt::TURN_DEADLINE`])
//! whose turn continues in a new attempt, within a per-request and a
//! per-bot daily cap.

use sqlx::SqliteConnection;
use uuid::Uuid;

/// Continuations one request may take after the attempt limit cut it.
pub const PER_REQUEST: i64 = 4;
/// Continuations one bot may take in a day, across its requests.
pub const PER_BOT_DAY: i64 = 12;

/// What an attempt cut at the limit leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// The turn continues in a new attempt (recorded).
    Continued,
    /// A cap was reached: the turn fails after `continued` continuations.
    Capped { continued: i64 },
}

/// Decide and record the continuation of `attempt`, inside the caller's
/// immediate transaction. A repeat for the same attempt keeps its first
/// decision.
pub async fn cut(
    tx: &mut SqliteConnection,
    turn_id: Uuid,
    bot_id: Uuid,
    attempt: i64,
) -> Result<Cut, sqlx::Error> {
    let (continued, recorded, today): (i64, bool, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM turn_limit_cuts WHERE turn_id = ?1),
                EXISTS (SELECT 1 FROM turn_limit_cuts WHERE turn_id = ?1 AND attempt = ?3),
                (SELECT count(*) FROM turn_limit_cuts c JOIN turns t ON t.id = c.turn_id
                 WHERE t.bot_id = ?2
                   AND c.created_at > strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-1 day'))",
    )
    .bind(turn_id)
    .bind(bot_id)
    .bind(attempt)
    .fetch_one(&mut *tx)
    .await?;
    if recorded {
        return Ok(Cut::Continued);
    }
    if continued >= PER_REQUEST || today >= PER_BOT_DAY {
        return Ok(Cut::Capped { continued });
    }
    sqlx::query("INSERT INTO turn_limit_cuts (turn_id, attempt) VALUES (?1, ?2)")
        .bind(turn_id)
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
    Ok(Cut::Continued)
}

/// The number of the continuation `attempt` is, when the attempt before it
/// was cut at the limit.
pub async fn continuation(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i64,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM turn_limit_cuts WHERE turn_id = ?1)
         WHERE EXISTS (SELECT 1 FROM turn_limit_cuts WHERE turn_id = ?1 AND attempt = ?2 - 1)",
    )
    .bind(turn_id)
    .bind(attempt)
    .fetch_optional(&mut *conn)
    .await
}
