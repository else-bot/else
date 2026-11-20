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
/// immediate transaction. `None` when `attempt` is no longer the turn's
/// active attempt, so a stale finish records nothing.
pub async fn cut(
    tx: &mut SqliteConnection,
    turn_id: Uuid,
    bot_id: Uuid,
    attempt: i64,
) -> Result<Option<Cut>, sqlx::Error> {
    let row: Option<(i64, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT (SELECT count(*) FROM turn_limit_cuts WHERE turn_id = ?1),
                (SELECT count(*) FROM turn_limit_cuts c JOIN turns t ON t.id = c.turn_id
                 WHERE t.bot_id = ?2
                   AND c.created_at > strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-1 day'))
         FROM turns WHERE id = ?1 AND attempt = ?3 AND status IN {}
           AND NOT EXISTS (SELECT 1 FROM turn_limit_cuts WHERE turn_id = ?1 AND attempt = ?3)",
        super::turns::ACTIVE_STATUSES
    )))
    .bind(turn_id)
    .bind(bot_id)
    .bind(attempt)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((continued, today)) = row else {
        return Ok(None);
    };
    if continued >= PER_REQUEST || today >= PER_BOT_DAY {
        return Ok(Some(Cut::Capped { continued }));
    }
    sqlx::query("INSERT INTO turn_limit_cuts (turn_id, attempt) VALUES (?1, ?2)")
        .bind(turn_id)
        .bind(attempt)
        .execute(&mut *tx)
        .await?;
    Ok(Some(Cut::Continued))
}

/// The number of the continuation the turn's next attempt is, while no
/// attempt the provider took has been told about its latest cut.
pub async fn undelivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<Option<i64>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM turn_limit_cuts WHERE turn_id = ?1)
         WHERE EXISTS (SELECT 1 FROM turn_limit_cuts
                       WHERE turn_id = ?1 AND delivered_attempt IS NULL)",
    )
    .bind(turn_id)
    .fetch_optional(&mut *conn)
    .await
}

/// Record that `attempt`, which the provider took, was told about the cuts.
pub async fn mark_delivered(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE turn_limit_cuts SET delivered_attempt = ?2
         WHERE turn_id = ?1 AND delivered_attempt IS NULL",
    )
    .bind(turn_id)
    .bind(attempt)
    .execute(&mut *conn)
    .await?;
    Ok(())
}
