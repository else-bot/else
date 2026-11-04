//! Durable owner messages and turn attempts.
//!
//! Acceptance writes the owner item and its queued turn in one transaction
//! inside the immediate transaction. The scheduler claims turns with short
//! transactions; provider progress is persisted through [`super::turn_sink::SqliteSink`], whose
//! writes are fenced on the turn still being active and the bot still active.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use crate::app::providers::{HistoryEntry, Profile, SessionState, TurnInput};

/// Turn statuses past the queue that hold the bot's single active slot.
pub const ACTIVE_STATUSES: &str = "('preparing', 'sending', 'running', 'cancelling')";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TurnRecord {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub request_id: Uuid,
    pub turn_no: i64,
    /// The item the turn answers: the owner's message, or the assistant's
    /// own greeting for a turn that started the conversation.
    pub source_item_id: Uuid,
    pub provider_id: String,
    pub profile_revision: Option<i64>,
    pub profile: Option<Json<Profile>>,
    pub status: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub effect_uncertain: bool,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub attempt: i32,
    /// The bot's requests for secrets during this turn (metadata only).
    #[sqlx(skip)]
    pub secret_requests: Vec<crate::db::secret_requests::RequestInfo>,
}

pub const TURN_COLUMNS: &str = "t.id, t.bot_id, t.conversation_id, t.request_id, t.turn_no,
    t.owner_item_id AS source_item_id, t.provider_id, t.profile_revision, t.profile, t.status,
    t.error_code, t.error_message, t.effect_uncertain, t.created_at, t.finished_at, t.attempt";

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ItemRecord {
    pub id: Uuid,
    pub conversation_id: Uuid,
    pub seq: i64,
    pub turn_id: Uuid,
    pub kind: String,
    pub text: String,
    pub provider_id: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Set for the owner's message: the request that created it.
    pub request_id: Option<Uuid>,
}

pub const ITEM_COLUMNS: &str = "i.id, i.conversation_id, i.seq, i.turn_id, i.kind, i.text,
    i.provider_id, i.created_at,
    CASE WHEN i.kind = 'user' THEN t.request_id END AS request_id";

#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    #[error("bot not found")]
    NotFound,
    #[error("bot deleted")]
    Deleted,
    #[error("request ID reused with different input")]
    RequestConflict,
    #[error("too many queued messages")]
    QueueFull,
    #[error("work is still stopping")]
    CancellationPending,
    #[error("no saved profile for provider {0}")]
    ProfileMissing(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub struct Accepted {
    /// False when this was a matching retry.
    pub created: bool,
    pub turn: TurnRecord,
    pub item: ItemRecord,
    /// An active turn on another provider this message switched away from;
    /// it was moved to `cancelling` and must be interrupted.
    pub preempted: Option<Uuid>,
}

pub struct SendRequest<'a> {
    pub bot_id: Uuid,
    pub request_id: Uuid,
    pub text: &'a str,
    /// The provider named by the request, as sent (part of the request hash).
    pub requested_provider: Option<&'a str>,
    /// The provider the turn is bound to.
    pub provider_id: &'a str,
    pub uses_profile: bool,
    pub max_queued: i64,
}

pub fn request_hash(req: &SendRequest<'_>) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"turn\0");
    hasher.update(req.requested_provider.unwrap_or("").as_bytes());
    hasher.update(b"\0");
    hasher.update(req.text.as_bytes());
    hasher.finalize().to_vec()
}

pub async fn accept(pool: &SqlitePool, req: SendRequest<'_>) -> Result<Accepted, AcceptError> {
    let hash = request_hash(&req);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot: Option<(String, Uuid, i64, i64, bool)> = sqlx::query_as(
        "SELECT b.status, c.id, c.last_turn_no, c.last_item_seq, b.cancel_pending
         FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
         WHERE b.id = ?1",
    )
    .bind(req.bot_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((status, conversation_id, last_turn_no, last_item_seq, cancel_pending)) = bot else {
        return Err(AcceptError::NotFound);
    };
    if status != "active" {
        return Err(AcceptError::Deleted);
    }
    let existing: Option<(Uuid, Vec<u8>)> = sqlx::query_as(
        "SELECT id, request_hash FROM turns WHERE conversation_id = ?1 AND request_id = ?2",
    )
    .bind(conversation_id)
    .bind(req.request_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((turn_id, saved)) = existing {
        if saved != hash {
            return Err(AcceptError::RequestConflict);
        }
        let (turn, item) = load_turn_and_item(&mut tx, turn_id).await?;
        tx.commit().await?;
        return Ok(Accepted {
            created: false,
            turn,
            item,
            preempted: None,
        });
    }
    if cancel_pending {
        return Err(AcceptError::CancellationPending);
    }
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turns WHERE bot_id = ?1 AND status = 'queued'")
            .bind(req.bot_id)
            .fetch_one(&mut *tx)
            .await?;
    if queued >= req.max_queued {
        return Err(AcceptError::QueueFull);
    }
    // The bot's own profile for this provider, else the provider's saved one.
    let profile: Option<(i64, Json<Profile>)> = if req.uses_profile {
        Some(
            crate::db::bot_profiles::effective(&mut tx, req.bot_id, req.provider_id)
                .await?
                .ok_or_else(|| AcceptError::ProfileMissing(req.provider_id.to_owned()))?,
        )
    } else {
        None
    };

    let turn_id = Uuid::new_v4();
    let item_id = Uuid::new_v4();
    let turn_no = last_turn_no + 1;
    let seq = last_item_seq + 1;
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, profile_revision, profile)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(turn_id)
    .bind(req.bot_id)
    .bind(conversation_id)
    .bind(req.request_id)
    .bind(&hash)
    .bind(turn_no)
    .bind(item_id)
    .bind(req.provider_id)
    .bind(profile.as_ref().map(|(revision, _)| *revision))
    .bind(profile.as_ref().map(|(_, profile)| profile))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, ?3, ?4, 'user', ?5)",
    )
    .bind(item_id)
    .bind(conversation_id)
    .bind(seq)
    .bind(turn_id)
    .bind(req.text)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET last_turn_no = ?2, last_item_seq = ?3 WHERE id = ?1")
        .bind(conversation_id)
        .bind(turn_no)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE bots SET selected_provider_id = ?2 WHERE id = ?1")
        .bind(req.bot_id)
        .bind(req.provider_id)
        .execute(&mut *tx)
        .await?;
    crate::db::events::turn_accepted(&mut tx, req.bot_id, turn_id).await?;
    // Switching provider stops the active turn on the other provider (its
    // output is recorded as it drains; guest jobs keep running), so the
    // switch is not stuck behind it. Children's bots are locked before the
    // turn, in the usual order.
    crate::db::delegations::lock_targets_of_bot(&mut tx, req.bot_id).await?;
    let preempted: Option<Uuid> = sqlx::query_scalar(
        "UPDATE turns SET status = 'cancelling', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE bot_id = ?1 AND conversation_id = ?2 AND turn_no < ?3 AND provider_id <> ?4
           AND status IN ('preparing', 'sending', 'running')
         RETURNING id",
    )
    .bind(req.bot_id)
    .bind(conversation_id)
    .bind(turn_no)
    .bind(req.provider_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(active) = preempted {
        crate::db::events::turn_changed(&mut tx, req.bot_id, active).await?;
    }
    // The owner's message enters the bot's memory with its acceptance.
    crate::db::memory::log_item(&mut tx, item_id).await?;
    let (turn, item) = load_turn_and_item(&mut tx, turn_id).await?;
    tx.commit().await?;
    Ok(Accepted {
        created: true,
        turn,
        item,
        preempted,
    })
}

pub async fn load_turn(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<TurnRecord, sqlx::Error> {
    let mut turn: TurnRecord = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {TURN_COLUMNS} FROM turns t WHERE t.id = ?1"
    )))
    .bind(turn_id)
    .fetch_one(&mut *conn)
    .await?;
    attach_secret_requests(conn, std::slice::from_mut(&mut turn)).await?;
    Ok(turn)
}

/// Fill in each turn's secret requests.
pub async fn attach_secret_requests(
    conn: &mut SqliteConnection,
    turns: &mut [TurnRecord],
) -> Result<(), sqlx::Error> {
    let ids: Vec<Uuid> = turns.iter().map(|t| t.id).collect();
    let mut by_turn = crate::db::secret_requests::for_turns(conn, &ids).await?;
    for turn in turns {
        turn.secret_requests = by_turn.remove(&turn.id).unwrap_or_default();
    }
    Ok(())
}

pub async fn load_item(
    conn: &mut SqliteConnection,
    item_id: Uuid,
) -> Result<ItemRecord, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN turns t ON t.id = i.turn_id WHERE i.id = ?1"
    )))
    .bind(item_id)
    .fetch_one(conn)
    .await
}

async fn load_turn_and_item(
    conn: &mut SqliteConnection,
    turn_id: Uuid,
) -> Result<(TurnRecord, ItemRecord), sqlx::Error> {
    let turn = load_turn(conn, turn_id).await?;
    let item = load_item(conn, turn.source_item_id).await?;
    Ok((turn, item))
}

/// A previously accepted send with this request ID, if any. `Err` when the
/// ID was used with a different payload. Lets a matching retry succeed even
/// if its provider has since become unavailable.
pub async fn existing_send(
    pool: &SqlitePool,
    req: &SendRequest<'_>,
) -> Result<Option<Accepted>, AcceptError> {
    let hash = request_hash(req);
    let mut conn = pool.acquire().await?;
    let row: Option<(Uuid, Vec<u8>, String)> = sqlx::query_as(
        "SELECT t.id, t.request_hash, b.status FROM turns t JOIN bots b ON b.id = t.bot_id
         WHERE t.bot_id = ?1 AND t.request_id = ?2",
    )
    .bind(req.bot_id)
    .bind(req.request_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        None => Ok(None),
        Some((_, _, status)) if status != "active" => Err(AcceptError::Deleted),
        Some((_, saved, _)) if saved != hash => Err(AcceptError::RequestConflict),
        Some((turn_id, _, _)) => {
            let (turn, item) = load_turn_and_item(&mut conn, turn_id).await?;
            Ok(Some(Accepted {
                created: false,
                turn,
                item,
                preempted: None,
            }))
        }
    }
}

/// Resolve a lost send response by its request ID.
pub async fn find_by_request(
    pool: &SqlitePool,
    bot_id: Uuid,
    request_id: Uuid,
) -> Result<Option<(TurnRecord, ItemRecord)>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let turn_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM turns WHERE bot_id = ?1 AND request_id = ?2")
            .bind(bot_id)
            .bind(request_id)
            .fetch_optional(&mut *conn)
            .await?;
    match turn_id {
        Some(id) => Ok(Some(load_turn_and_item(&mut conn, id).await?)),
        None => Ok(None),
    }
}

/// Claim the oldest queued turn of the next eligible bot, oldest first
/// across bots. A bot is eligible when it is active, not stopping work, and
/// has no turn in an active status. The claimed turn moves to `preparing`
/// and is returned; the caller must start it before claiming again, so a
/// later failure never strands an already committed claim.
pub async fn claim_next(pool: &SqlitePool, skip: &[Uuid]) -> Result<Option<Uuid>, sqlx::Error> {
    let candidates: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, bot_id FROM (
            SELECT t.id, t.bot_id, t.created_at,
                   row_number() OVER (PARTITION BY t.bot_id ORDER BY t.turn_no) AS queue_rank
            FROM turns t JOIN bots b ON b.id = t.bot_id
            WHERE t.status = 'queued' AND b.status = 'active' AND NOT b.cancel_pending
              AND NOT (lower(hex(t.bot_id)) IN (SELECT replace(value, '-', '') FROM json_each(?1)))
              AND NOT EXISTS (SELECT 1 FROM turns a
                              WHERE a.bot_id = t.bot_id AND a.status IN {ACTIVE_STATUSES})
              -- A root task waiting for delegated work holds the bot's queue.
              AND NOT EXISTS (SELECT 1 FROM turns w
                              WHERE w.bot_id = t.bot_id AND w.status = 'waiting')
         ) oldest WHERE queue_rank = 1 ORDER BY created_at, id LIMIT 8"
    )))
    .bind(sqlx::types::Json(skip))
    .fetch_all(pool)
    .await?;
    for (turn_id, bot_id) in candidates {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let eligible: Option<bool> = sqlx::query_scalar(
            "SELECT status = 'active' AND NOT cancel_pending FROM bots WHERE id = ?1",
        )
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
        if eligible != Some(true) {
            continue;
        }
        let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE turns SET status = 'preparing', started_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 attempt = attempt + 1
             WHERE id = ?1 AND status = 'queued'
               AND NOT EXISTS (SELECT 1 FROM turns a
                               WHERE a.bot_id = ?2 AND a.status IN {ACTIVE_STATUSES})"
        )))
        .bind(turn_id)
        .bind(bot_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 1 {
            crate::db::events::turn_changed(&mut tx, bot_id, turn_id).await?;
            tx.commit().await?;
            return Ok(Some(turn_id));
        }
    }
    Ok(None)
}

/// Fail a claimed turn that could not be started (for example, its input
/// could not be loaded). Nothing was sent to a provider.
pub async fn fail_unstarted(
    pool: &SqlitePool,
    turn_id: Uuid,
    message: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT b.id FROM bots b JOIN turns t ON t.bot_id = b.id WHERE t.id = ?1",
    )
    .bind(turn_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(bot_id) = bot_id else {
        return Ok(());
    };
    let updated = sqlx::query(
        "UPDATE turns SET status = 'failed', error_code = 'start_failed', error_message = ?2,
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE id = ?1 AND status = 'preparing'",
    )
    .bind(turn_id)
    .bind(message)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if updated == 1 {
        crate::db::events::turn_changed(&mut tx, bot_id, turn_id).await?;
    }
    tx.commit().await
}

/// Build the adapter input for a claimed turn.
pub async fn input(pool: &SqlitePool, turn_id: Uuid) -> Result<TurnInput, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let turn = load_turn(&mut conn, turn_id).await?;
    let (bot_name, bot_role): (String, String) =
        sqlx::query_as("SELECT name, role FROM bots WHERE id = ?1")
            .bind(turn.bot_id)
            .fetch_one(&mut *conn)
            .await?;
    let text: String = sqlx::query_scalar("SELECT text FROM items WHERE id = ?1 AND kind = 'user'")
        .bind(turn.source_item_id)
        .fetch_one(&mut *conn)
        .await?;
    let history = sqlx::query_as::<_, (i64, String, String, String)>(
        "SELECT t.turn_no, i.kind, i.text, t.status FROM items i JOIN turns t ON t.id = i.turn_id
         WHERE i.conversation_id = ?1 AND t.turn_no < ?2 AND i.kind IN ('user', 'assistant')
         ORDER BY t.turn_no, i.seq",
    )
    .bind(turn.conversation_id)
    .bind(turn.turn_no)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(turn_no, kind, text, status)| HistoryEntry {
        turn_no,
        kind,
        text,
        status,
    })
    .collect();
    let session = sqlx::query_as::<_, (String, String, i64, Option<i64>)>(
        "SELECT native_session_id, fingerprint, considered_through_turn, memory_revision
         FROM provider_sessions s
         WHERE conversation_id = ?1 AND provider_id = ?2 AND status = 'active'
           -- A session that may hold an uncertain delivery is never resumed.
           AND NOT EXISTS (SELECT 1 FROM turns u
                           WHERE u.provider_session_id = s.id AND u.status = 'uncertain')",
    )
    .bind(turn.conversation_id)
    .bind(&turn.provider_id)
    .fetch_optional(&mut *conn)
    .await?
    .map(
        |(native_id, fingerprint, considered_through_turn, memory_revision)| SessionState {
            native_id,
            fingerprint,
            considered_through_turn,
            memory_revision,
        },
    );
    let mut input = TurnInput {
        bot_id: turn.bot_id,
        conversation_id: turn.conversation_id,
        turn_id,
        turn_no: turn.turn_no,
        bot_name,
        bot_role,
        text,
        profile: turn.profile.map(|Json(profile)| profile),
        history,
        session,
        tool_grant: None,
        memory: None,
        attempt: turn.attempt,
        delegation: crate::db::delegations::context(&mut conn, turn_id).await?,
        secrets: crate::db::secret_requests::undelivered(&mut conn, turn_id).await?,
        handoff: None,
        jobs: crate::db::handoffs::job_receipts(&mut conn, turn.bot_id).await?,
    };
    // Return this connection before memory loading checks out others.
    drop(conn);
    input.memory = crate::db::memory_tree::session_context(pool, input.bot_id).await?;
    Ok(input)
}

/// Startup reconciliation, run before the scheduler admits work. A turn
/// claimed but never delivered returns to the queue. A turn that may have
/// reached its provider is never replayed: it becomes `uncertain`, keeping
/// any partial text. Returns (requeued, uncertain).
pub async fn recover_after_restart(pool: &SqlitePool) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let requeued: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'queued', started_at = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status = 'preparing'
           AND bot_id IN (SELECT id FROM bots WHERE status = 'active' AND NOT cancel_pending)
         RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    let uncertain: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'uncertain', effect_uncertain = true,
             error_code = 'api_restarted',
             error_message = 'The API restarted while this message was being processed. \
It was not resent; send a follow-up to continue.',
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status IN ('sending', 'running', 'cancelling') RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    // A claim that never reached its provider, on a bot being stopped or
    // deleted, simply ends.
    let mut uncertain = uncertain;
    let halted: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'interrupted', error_code = 'cancelled',
             error_message = 'Stopped before it was sent.', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         WHERE status = 'preparing' RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    uncertain.extend(halted);
    for (turn_id, bot_id) in requeued.iter().chain(&uncertain) {
        crate::db::events::turn_changed(&mut tx, *bot_id, *turn_id).await?;
    }
    crate::db::handoffs::settle_ended(&mut tx, None).await?;
    for (turn_id, _) in &uncertain {
        crate::db::memory::log_turn_end(&mut tx, *turn_id).await?;
        // A root that ended here leaves no child running for it.
        crate::db::delegations::cancel_for_root(&mut tx, *turn_id).await?;
    }
    tx.commit().await?;
    // Stops and deletions waiting on those turns can now complete. Queued
    // turns of deleted bots never run (claim requires an active bot).
    crate::db::stop::settle_all(pool).await?;
    Ok((requeued.len() as u64, uncertain.len() as u64))
}

/// Turns in an active status that no provider task in this process owns.
/// The singleton lock means no other process runs them, so they are settled:
/// one never started fails, one that may have reached its provider becomes
/// `uncertain` (never resent). Pending stops and deletions then complete.
pub async fn settle_orphans(pool: &SqlitePool, live: &[Uuid]) -> Result<u64, sqlx::Error> {
    let orphans: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, bot_id FROM turns WHERE status IN {ACTIVE_STATUSES} AND NOT (lower(hex(id)) IN (SELECT replace(value, '-', '') FROM json_each(?1)))"
    )))
    .bind(sqlx::types::Json(live))
    .fetch_all(pool)
    .await?;
    let mut settled = 0;
    for (turn_id, bot_id) in orphans {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("SELECT 1 FROM bots WHERE id = ?1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        crate::db::delegations::lock_targets_of_root(&mut tx, turn_id).await?;
        let updated = sqlx::query(
            "UPDATE turns SET
                 status = CASE WHEN status = 'preparing' THEN 'failed' ELSE 'uncertain' END,
                 effect_uncertain = effect_uncertain OR status <> 'preparing',
                 error_code = CASE WHEN status = 'preparing' THEN 'start_failed' ELSE 'orphaned' END,
                 error_message = CASE WHEN status = 'preparing'
                     THEN 'The message could not be started. Send it again.'
                     ELSE 'Processing stopped unexpectedly. It was not resent; send a follow-up to continue.'
                 END,
                 finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status IN ('preparing', 'sending', 'running', 'cancelling')",
        )
        .bind(turn_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 1 {
            crate::db::events::turn_changed(&mut tx, bot_id, turn_id).await?;
            crate::db::memory::log_turn_end(&mut tx, turn_id).await?;
            crate::db::delegations::cancel_for_root(&mut tx, turn_id).await?;
            crate::db::handoffs::settle_ended(&mut tx, Some(turn_id)).await?;
            crate::db::stop::settle(&mut tx, bot_id).await?;
            settled += 1;
        }
        tx.commit().await?;
    }
    Ok(settled)
}

/// IDs of turns being stopped.
pub async fn cancelling(pool: &SqlitePool) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM turns WHERE status = 'cancelling'")
        .fetch_all(pool)
        .await
}
