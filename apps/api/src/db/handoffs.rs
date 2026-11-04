//! Provider handoff records (POC-020). See migration `0017_handoffs`.

use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::app::providers::{HandoffContext, JobReceipt, Selection};

/// Version of the checkpoint request and the handoff block in packets.
pub const PROMPT_VERSION: &str = "else-handoff-v1";

/// The provider of the owner turn just before `turn_no`, if any.
pub async fn previous_provider(
    pool: &SqlitePool,
    conversation_id: Uuid,
    turn_no: i64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT t.provider_id FROM turns t JOIN items i ON i.id = t.owner_item_id
         WHERE t.conversation_id = ?1 AND t.turn_no < ?2 AND i.kind = 'user'
         ORDER BY t.turn_no DESC LIMIT 1",
    )
    .bind(conversation_id)
    .bind(turn_no)
    .fetch_optional(pool)
    .await
}

/// The handoff already prepared for this attempt (a restarted dispatch
/// reuses the frozen checkpoint rather than asking again).
pub async fn existing(
    pool: &SqlitePool,
    turn_id: Uuid,
    attempt: i32,
) -> Result<Option<HandoffContext>, sqlx::Error> {
    let row: Option<(Uuid, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, source_provider_id, checkpoint, reason FROM handoffs
         WHERE turn_id = ?1 AND attempt = ?2",
    )
    .bind(turn_id)
    .bind(attempt)
    .fetch_optional(pool)
    .await?;
    Ok(
        row.map(|(id, source_provider, checkpoint, reason)| HandoffContext {
            id,
            source_provider,
            checkpoint,
            reason,
        }),
    )
}

/// Whether any attempt of this turn already handed off and was accepted
/// (a continuation does not hand off again).
pub async fn accepted_for_turn(pool: &SqlitePool, turn_id: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM handoffs WHERE turn_id = ?1 AND state = 'accepted')",
    )
    .bind(turn_id)
    .fetch_one(pool)
    .await
}

pub struct NewHandoff<'a> {
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub turn_id: Uuid,
    pub attempt: i32,
    pub source_provider: &'a str,
    pub target_provider: &'a str,
    pub boundary_turn_no: i64,
    pub checkpoint: Option<String>,
    pub reason: Option<String>,
}

/// Persist a prepared handoff while its turn is still active. Returns it,
/// or `None` if the turn ended meanwhile.
pub async fn insert(
    pool: &SqlitePool,
    new: NewHandoff<'_>,
) -> Result<Option<HandoffContext>, sqlx::Error> {
    let id = Uuid::new_v4();
    let kind = if new.checkpoint.is_some() {
        "llm"
    } else {
        "fallback"
    };
    let inserted = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO handoffs (id, bot_id, conversation_id, turn_id, attempt, source_provider_id,
             target_provider_id, source_session_id, boundary_turn_no, kind, reason, checkpoint,
             prompt_version)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7,
             (SELECT id FROM provider_sessions WHERE conversation_id = ?3
                AND provider_id = ?6 AND status = 'active'),
             ?8, ?9, ?10, ?11, ?12
         WHERE EXISTS (SELECT 1 FROM turns WHERE id = ?4 AND status IN {})
         ON CONFLICT (turn_id, attempt) DO NOTHING",
        crate::db::turns::ACTIVE_STATUSES
    )))
    .bind(id)
    .bind(new.bot_id)
    .bind(new.conversation_id)
    .bind(new.turn_id)
    .bind(new.attempt)
    .bind(new.source_provider)
    .bind(new.target_provider)
    .bind(new.boundary_turn_no)
    .bind(kind)
    .bind(
        new.reason
            .as_deref()
            .map(|r| r.chars().take(500).collect::<String>()),
    )
    .bind(&new.checkpoint)
    .bind(PROMPT_VERSION)
    .execute(pool)
    .await?
    .rows_affected();
    if inserted == 0 {
        return existing(pool, new.turn_id, new.attempt).await;
    }
    Ok(Some(HandoffContext {
        id,
        source_provider: new.source_provider.to_owned(),
        checkpoint: new.checkpoint,
        reason: new.reason,
    }))
}

/// Record what the packet carried, before it is sent.
pub async fn record_packet(
    pool: &SqlitePool,
    turn_id: Uuid,
    attempt: i32,
    selection: &Selection,
    payload: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE handoffs SET selected_turns = ?3, omitted_turns = ?4, payload_sha256 = ?5,
             updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE turn_id = ?1 AND attempt = ?2 AND state = 'prepared'",
    )
    .bind(turn_id)
    .bind(attempt)
    .bind(sqlx::types::Json(&selection.selected))
    .bind(sqlx::types::Json(&selection.omitted))
    .bind(Sha256::digest(payload.as_bytes()).to_vec())
    .execute(pool)
    .await?;
    Ok(())
}

/// Move this attempt's handoff between delivery states, inside the
/// caller's transaction (the turn's own transition).
pub async fn transition(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
    attempt: i32,
    from: &str,
    to: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE handoffs SET state = ?4, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
             target_session_id = coalesce(target_session_id,
                 (SELECT provider_session_id FROM turns WHERE id = ?1))
         WHERE turn_id = ?1 AND attempt = ?2 AND state = ?3",
    )
    .bind(turn_id)
    .bind(attempt)
    .bind(from)
    .bind(to)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A handoff still `sending` whose turn has ended without acknowledgment
/// (restart or orphan recovery) has an unknown delivery.
pub async fn settle_ended(
    conn: &mut SqliteConnection,
    turn_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE handoffs AS h SET state = 'uncertain', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         FROM turns t
         WHERE h.turn_id = t.id AND h.state = 'sending'
           AND t.status IN ('completed', 'interrupted', 'failed', 'uncertain')
           AND (?1 IS NULL OR h.turn_id = ?1)",
    )
    .bind(turn_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The bot's jobs that may still be running or whose outcome is unknown
/// (`lost` within a week), newest first, at most 20.
pub async fn job_receipts(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<Vec<JobReceipt>, sqlx::Error> {
    let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT id, status, substr(command, 1, 300), started_at
         FROM guest_jobs
         WHERE bot_id = ?1 AND (status IN ('starting', 'running', 'cancelling')
               OR (status = 'lost' AND started_at > strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-7 days')))
         ORDER BY started_at DESC LIMIT 20",
    )
    .bind(bot_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, status, command, started_at)| JobReceipt {
            id,
            status,
            command,
            started_at,
        })
        .collect())
}
