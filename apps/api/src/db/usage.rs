//! The turns and limit readings behind the usage report.

use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::app::providers::LimitWindow;
use crate::app::usage::{Reading, Turn};

/// The longest window the report covers, plus slack for readings taken
/// shortly before it started.
const HISTORY: Duration = Duration::days(8);

/// Turns that reported tokens and started within the history, as the
/// provider and model that ran them.
pub async fn turns(db: &SqlitePool, now: DateTime<Utc>) -> sqlx::Result<Vec<Turn>> {
    sqlx::query_as(
        "SELECT t.bot_id, b.name AS bot_name, t.provider_id,
                coalesce(json_extract(t.reported_profile, '$.model'),
                         json_extract(t.profile, '$.model')) AS model,
                -- What started the turn, most specific first. Origin in
                -- db::delegations answers a different question (who may
                -- see the turn), so the two are kept apart.
                CASE
                    WHEN w.id IS NOT NULL THEN 'webhook'
                    WHEN rr.id IS NOT NULL THEN 'routine'
                    WHEN EXISTS (SELECT 1 FROM connector_deliveries x WHERE x.turn_id = t.id)
                        THEN 'mail'
                    WHEN EXISTS (SELECT 1 FROM social_deliveries x WHERE x.turn_id = t.id)
                        THEN 'social'
                    WHEN c.kind = 'task' THEN 'delegated'
                    ELSE 'chat'
                END AS source,
                coalesce(t.started_at, t.created_at) AS started_at, t.finished_at,
                coalesce(t.input_tokens, 0) AS input_tokens,
                coalesce(t.cached_input_tokens, 0) AS cached_input_tokens,
                coalesce(t.cache_write_tokens, 0) AS cache_write_tokens,
                coalesce(t.output_tokens, 0) AS output_tokens
         FROM turns t
         JOIN bots b ON b.id = t.bot_id
         JOIN conversations c ON c.id = t.conversation_id
         LEFT JOIN webhook_runs w ON w.turn_id = t.id
         LEFT JOIN routine_runs rr ON rr.turn_id = t.id
         WHERE (t.input_tokens IS NOT NULL OR t.output_tokens IS NOT NULL)
           AND coalesce(t.started_at, t.created_at) >= ?1",
    )
    .bind(since(now))
    .fetch_all(db)
    .await
}

pub async fn readings(db: &SqlitePool, now: DateTime<Utc>) -> sqlx::Result<Vec<Reading>> {
    sqlx::query_as(
        "SELECT report_id, provider_id, window_minutes, used_percent, resets_at, observed_at
         FROM usage_readings WHERE observed_at >= ?1",
    )
    .bind(since(now))
    .fetch_all(db)
    .await
}

/// Stores the windows one turn's provider reported, and prunes readings
/// older than the history in the same transaction.
pub async fn record(db: &SqlitePool, turn: Uuid, windows: &[LimitWindow]) -> sqlx::Result<()> {
    let report = Uuid::new_v4();
    let mut tx = db.begin().await?;
    for window in windows {
        sqlx::query(
            "INSERT INTO usage_readings
                 (report_id, provider_id, window_minutes, used_percent, resets_at)
             SELECT ?2, provider_id, ?3, ?4, ?5 FROM turns WHERE id = ?1",
        )
        .bind(turn)
        .bind(report)
        .bind(window.minutes)
        .bind(window.used_percent())
        .bind(text(window.resets_at))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM usage_readings WHERE observed_at < ?1")
        .bind(since(Utc::now()))
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

/// Timestamps are stored as text, so bounds and values match their format.
fn text(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

fn since(now: DateTime<Utc>) -> String {
    text(now - HISTORY)
}
