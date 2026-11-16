//! The turns and limit readings behind the usage report.

use chrono::{DateTime, Duration, Utc};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::app::usage::{Reading, Source, Turn};

/// The longest window the report covers, plus slack for readings taken
/// shortly before it started.
const HISTORY: Duration = Duration::days(8);

#[derive(sqlx::FromRow)]
struct TurnRow {
    id: Uuid,
    bot_id: Uuid,
    bot_name: String,
    provider_id: String,
    model: Option<String>,
    source: Source,
    label: Option<String>,
    routine_id: Option<Uuid>,
    started_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    input_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: i64,
    output_tokens: i64,
}

/// Turns that reported tokens and started within the history, as the
/// provider and model that ran them.
pub async fn turns(db: &SqlitePool, now: DateTime<Utc>) -> sqlx::Result<Vec<Turn>> {
    let rows: Vec<TurnRow> = sqlx::query_as(
        "SELECT t.id, t.bot_id, b.name AS bot_name, t.provider_id,
                coalesce(json_extract(t.reported_profile, '$.model'),
                         json_extract(t.profile, '$.model')) AS model,
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
                CASE
                    WHEN w.id IS NOT NULL THEN w.name
                    WHEN rr.id IS NOT NULL THEN r.name
                    WHEN c.kind = 'task' THEN (
                        SELECT parent.name FROM delegations d
                        JOIN bots parent ON parent.id = d.parent_bot_id
                        WHERE d.conversation_id = t.conversation_id
                        LIMIT 1)
                END AS label,
                CASE WHEN w.id IS NULL THEN r.id END AS routine_id,
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
         LEFT JOIN routines r ON r.id = rr.routine_id
         WHERE (t.input_tokens IS NOT NULL OR t.output_tokens IS NOT NULL)
           AND coalesce(t.started_at, t.created_at) >= ?1",
    )
    .bind(since(now))
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Turn {
            id: row.id,
            bot_id: row.bot_id,
            bot_name: row.bot_name,
            provider_id: row.provider_id,
            model: row.model,
            source: row.source,
            label: row.label,
            routine_id: row.routine_id,
            started_at: row.started_at,
            finished_at: row.finished_at,
            input_tokens: row.input_tokens,
            cached_input_tokens: row.cached_input_tokens,
            cache_write_tokens: row.cache_write_tokens,
            output_tokens: row.output_tokens,
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct ReadingRow {
    provider_id: String,
    window_minutes: i64,
    used_percent: f64,
    resets_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
}

pub async fn readings(db: &SqlitePool, now: DateTime<Utc>) -> sqlx::Result<Vec<Reading>> {
    let rows: Vec<ReadingRow> = sqlx::query_as(
        "SELECT provider_id, window_minutes, used_percent, resets_at, observed_at
         FROM usage_readings WHERE observed_at >= ?1",
    )
    .bind(since(now))
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Reading {
            provider_id: row.provider_id,
            window_minutes: row.window_minutes,
            used_percent: row.used_percent,
            resets_at: row.resets_at,
            observed_at: row.observed_at,
        })
        .collect())
}

/// Timestamps are stored as text, so the bound matches their format.
fn since(now: DateTime<Utc>) -> String {
    (now - HISTORY).to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}
