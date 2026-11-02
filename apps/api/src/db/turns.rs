//! Durable owner messages and turn attempts.
//!
//! Acceptance writes the owner item and its queued turn in one transaction
//! under the bot row lock. The scheduler claims turns with short
//! transactions; provider progress is persisted through [`PgSink`], whose
//! writes are fenced on the turn still being active and the bot still active.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::app::providers::{
    Fenced, HistoryEntry, Profile, SessionState, TurnInput, TurnOutcome, TurnSink,
};

/// Turn statuses past the queue that hold the bot's single active slot.
pub const ACTIVE_STATUSES: &str = "('preparing', 'sending', 'running', 'cancelling')";

/// Flush buffered provider text at least this often or at this size.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
pub const FLUSH_BYTES: usize = 2048;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TurnRecord {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub request_id: Uuid,
    pub turn_no: i64,
    pub owner_item_id: Uuid,
    pub provider_id: String,
    pub profile_revision: Option<i64>,
    pub profile: Option<Json<Profile>>,
    pub status: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub effect_uncertain: bool,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
}

pub const TURN_COLUMNS: &str = "t.id, t.bot_id, t.conversation_id, t.request_id, t.turn_no,
    t.owner_item_id, t.provider_id, t.profile_revision, t.profile, t.status, t.error_code,
    t.error_message, t.effect_uncertain, t.created_at, t.finished_at";

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

pub async fn accept(pool: &PgPool, req: SendRequest<'_>) -> Result<Accepted, AcceptError> {
    let hash = request_hash(&req);
    let mut tx = pool.begin().await?;
    let bot: Option<(String, Uuid, i64, i64, bool)> = sqlx::query_as(
        "SELECT b.status, c.id, c.last_turn_no, c.last_item_seq, b.cancel_pending
         FROM bots b JOIN conversations c ON c.bot_id = b.id
         WHERE b.id = $1 FOR UPDATE OF b, c",
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
        "SELECT id, request_hash FROM turns WHERE conversation_id = $1 AND request_id = $2",
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
        });
    }
    if cancel_pending {
        return Err(AcceptError::CancellationPending);
    }
    let queued: i64 =
        sqlx::query_scalar("SELECT count(*) FROM turns WHERE bot_id = $1 AND status = 'queued'")
            .bind(req.bot_id)
            .fetch_one(&mut *tx)
            .await?;
    if queued >= req.max_queued {
        return Err(AcceptError::QueueFull);
    }
    let profile: Option<(i64, Json<Profile>)> = if req.uses_profile {
        let row: Option<(i64, String, String, String)> = sqlx::query_as(
            "SELECT revision, model, reasoning_effort, service_tier
             FROM provider_profiles WHERE provider_id = $1 FOR SHARE",
        )
        .bind(req.provider_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (revision, model, reasoning_effort, service_tier) =
            row.ok_or_else(|| AcceptError::ProfileMissing(req.provider_id.to_owned()))?;
        Some((
            revision,
            Json(Profile {
                model,
                reasoning_effort,
                service_tier,
            }),
        ))
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
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
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
         VALUES ($1, $2, $3, $4, 'user', $5)",
    )
    .bind(item_id)
    .bind(conversation_id)
    .bind(seq)
    .bind(turn_id)
    .bind(req.text)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE conversations SET last_turn_no = $2, last_item_seq = $3 WHERE id = $1")
        .bind(conversation_id)
        .bind(turn_no)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE bots SET selected_provider_id = $2 WHERE id = $1")
        .bind(req.bot_id)
        .bind(req.provider_id)
        .execute(&mut *tx)
        .await?;
    crate::db::events::turn_accepted(&mut tx, req.bot_id, turn_id).await?;
    let (turn, item) = load_turn_and_item(&mut tx, turn_id).await?;
    tx.commit().await?;
    Ok(Accepted {
        created: true,
        turn,
        item,
    })
}

pub async fn load_turn(conn: &mut PgConnection, turn_id: Uuid) -> Result<TurnRecord, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {TURN_COLUMNS} FROM turns t WHERE t.id = $1"
    )))
    .bind(turn_id)
    .fetch_one(conn)
    .await
}

pub async fn load_item(conn: &mut PgConnection, item_id: Uuid) -> Result<ItemRecord, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ITEM_COLUMNS} FROM items i JOIN turns t ON t.id = i.turn_id WHERE i.id = $1"
    )))
    .bind(item_id)
    .fetch_one(conn)
    .await
}

async fn load_turn_and_item(
    conn: &mut PgConnection,
    turn_id: Uuid,
) -> Result<(TurnRecord, ItemRecord), sqlx::Error> {
    let turn = load_turn(conn, turn_id).await?;
    let item = load_item(conn, turn.owner_item_id).await?;
    Ok((turn, item))
}

/// A previously accepted send with this request ID, if any. `Err` when the
/// ID was used with a different payload. Lets a matching retry succeed even
/// if its provider has since become unavailable.
pub async fn existing_send(
    pool: &PgPool,
    req: &SendRequest<'_>,
) -> Result<Option<Accepted>, AcceptError> {
    let hash = request_hash(req);
    let mut conn = pool.acquire().await?;
    let row: Option<(Uuid, Vec<u8>, String)> = sqlx::query_as(
        "SELECT t.id, t.request_hash, b.status FROM turns t JOIN bots b ON b.id = t.bot_id
         WHERE t.bot_id = $1 AND t.request_id = $2",
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
            }))
        }
    }
}

/// Resolve a lost send response by its request ID.
pub async fn find_by_request(
    pool: &PgPool,
    bot_id: Uuid,
    request_id: Uuid,
) -> Result<Option<(TurnRecord, ItemRecord)>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let turn_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM turns WHERE bot_id = $1 AND request_id = $2")
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
pub async fn claim_next(pool: &PgPool, skip: &[Uuid]) -> Result<Option<Uuid>, sqlx::Error> {
    let candidates: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, bot_id FROM (
            SELECT DISTINCT ON (t.bot_id) t.id, t.bot_id, t.created_at
            FROM turns t JOIN bots b ON b.id = t.bot_id
            WHERE t.status = 'queued' AND b.status = 'active' AND NOT b.cancel_pending
              AND NOT (t.bot_id = ANY($1))
              AND NOT EXISTS (SELECT 1 FROM turns a
                              WHERE a.bot_id = t.bot_id AND a.status IN {ACTIVE_STATUSES})
            ORDER BY t.bot_id, t.turn_no
         ) oldest ORDER BY created_at, id LIMIT 8"
    )))
    .bind(skip)
    .fetch_all(pool)
    .await?;
    for (turn_id, bot_id) in candidates {
        let mut tx = pool.begin().await?;
        let eligible: Option<bool> = sqlx::query_scalar(
            "SELECT status = 'active' AND NOT cancel_pending FROM bots WHERE id = $1 FOR UPDATE",
        )
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
        if eligible != Some(true) {
            continue;
        }
        let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE turns SET status = 'preparing', started_at = now(), updated_at = now()
             WHERE id = $1 AND status = 'queued'
               AND NOT EXISTS (SELECT 1 FROM turns a
                               WHERE a.bot_id = $2 AND a.status IN {ACTIVE_STATUSES})"
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
    pool: &PgPool,
    turn_id: Uuid,
    message: &str,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let bot_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT b.id FROM bots b JOIN turns t ON t.bot_id = b.id WHERE t.id = $1 FOR UPDATE OF b",
    )
    .bind(turn_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(bot_id) = bot_id else {
        return Ok(());
    };
    let updated = sqlx::query(
        "UPDATE turns SET status = 'failed', error_code = 'start_failed', error_message = $2,
             finished_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'preparing'",
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
pub async fn input(pool: &PgPool, turn_id: Uuid) -> Result<TurnInput, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    let turn = load_turn(&mut conn, turn_id).await?;
    let (bot_name, bot_role): (String, String) =
        sqlx::query_as("SELECT name, role FROM bots WHERE id = $1")
            .bind(turn.bot_id)
            .fetch_one(&mut *conn)
            .await?;
    let text: String = sqlx::query_scalar("SELECT text FROM items WHERE id = $1")
        .bind(turn.owner_item_id)
        .fetch_one(&mut *conn)
        .await?;
    let history = sqlx::query_as::<_, (i64, String, String)>(
        "SELECT t.turn_no, i.kind, i.text FROM items i JOIN turns t ON t.id = i.turn_id
         WHERE i.conversation_id = $1 AND t.turn_no < $2 AND i.kind IN ('user', 'assistant')
         ORDER BY t.turn_no, i.seq",
    )
    .bind(turn.conversation_id)
    .bind(turn.turn_no)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|(turn_no, kind, text)| HistoryEntry {
        turn_no,
        kind,
        text,
    })
    .collect();
    let session = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT native_session_id, fingerprint, considered_through_turn FROM provider_sessions s
         WHERE conversation_id = $1 AND provider_id = $2 AND status = 'active'
           -- A session that may hold an uncertain delivery is never resumed.
           AND NOT EXISTS (SELECT 1 FROM turns u
                           WHERE u.provider_session_id = s.id AND u.status = 'uncertain')",
    )
    .bind(turn.conversation_id)
    .bind(&turn.provider_id)
    .fetch_optional(&mut *conn)
    .await?
    .map(
        |(native_id, fingerprint, considered_through_turn)| SessionState {
            native_id,
            fingerprint,
            considered_through_turn,
        },
    );
    Ok(TurnInput {
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
    })
}

/// State owned by whoever holds the flush lock.
struct Buffer {
    assistant_item: Option<Uuid>,
}

/// Persists one turn's progress. Writes succeed only while the turn is in
/// an active status and its bot is active.
pub struct PgSink {
    pool: PgPool,
    turn_id: Uuid,
    bot_id: Uuid,
    conversation_id: Uuid,
    provider_id: String,
    /// Held across each text write, serializing flushes and `finish`.
    buffer: tokio::sync::Mutex<Buffer>,
    /// Text not yet committed. Appended by `text()` without any I/O and
    /// drained only after the transaction that wrote it commits.
    pending: Mutex<String>,
    flush_wanted: tokio::sync::Notify,
    fenced: Mutex<bool>,
}

impl PgSink {
    pub fn new(pool: PgPool, input: &TurnInput, provider_id: &str) -> Self {
        Self {
            pool,
            turn_id: input.turn_id,
            bot_id: input.bot_id,
            conversation_id: input.conversation_id,
            provider_id: provider_id.to_owned(),
            buffer: tokio::sync::Mutex::new(Buffer {
                assistant_item: None,
            }),
            pending: Mutex::new(String::new()),
            flush_wanted: tokio::sync::Notify::new(),
            fenced: Mutex::new(false),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, String> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Resolves when enough text is buffered to flush before the interval.
    pub async fn flush_wanted(&self) {
        self.flush_wanted.notified().await;
    }

    fn is_fenced(&self) -> bool {
        *self.fenced.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn fence(&self) -> Fenced {
        *self.fenced.lock().unwrap_or_else(|p| p.into_inner()) = true;
        Fenced
    }

    /// Lock the bot row and check the turn may still write. Returns the
    /// conversation's last item seq.
    async fn guard(
        &self,
        conn: &mut PgConnection,
        allowed: &str,
    ) -> Result<Option<i64>, sqlx::Error> {
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT b.status, c.last_item_seq FROM bots b JOIN conversations c ON c.bot_id = b.id
             WHERE b.id = $1 FOR UPDATE OF b, c",
        )
        .bind(self.bot_id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((status, last_seq)) = row.filter(|(status, _)| status == "active") else {
            return Ok(None);
        };
        let _ = status;
        let turn_ok: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE id = $1 AND status IN {allowed})"
        )))
        .bind(self.turn_id)
        .fetch_one(&mut *conn)
        .await?;
        Ok(turn_ok.then_some(last_seq))
    }

    async fn transition(&self, from: &str, to: &str) -> Result<(), Fenced> {
        if self.is_fenced() {
            return Err(Fenced);
        }
        let result: Result<bool, sqlx::Error> = async {
            let mut tx = self.pool.begin().await?;
            if self.guard(&mut tx, &format!("('{from}')")).await?.is_none() {
                return Ok(false);
            }
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE turns SET status = '{to}', updated_at = now() WHERE id = $1"
            )))
            .bind(self.turn_id)
            .execute(&mut *tx)
            .await?;
            crate::db::events::turn_changed(&mut tx, self.bot_id, self.turn_id).await?;
            tx.commit().await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => Ok(()),
            Ok(false) => Err(self.fence()),
            Err(err) => {
                tracing::error!(turn = %self.turn_id, error = %err, "persisting turn status failed");
                Err(self.fence())
            }
        }
    }

    /// Write `chunk` to the assistant item inside `tx`, creating the item
    /// on first use. Returns the item ID.
    async fn write_text(
        &self,
        tx: &mut PgConnection,
        assistant_item: Option<Uuid>,
        last_seq: i64,
        chunk: &str,
    ) -> Result<Uuid, sqlx::Error> {
        if let Some(item_id) = assistant_item {
            let offset: i64 = sqlx::query_scalar(
                "UPDATE items SET text = text || $2::text WHERE id = $1
                 RETURNING (octet_length(text) - octet_length($2::text))::bigint",
            )
            .bind(item_id)
            .bind(chunk)
            .fetch_one(&mut *tx)
            .await?;
            crate::db::events::item_delta(
                tx,
                self.bot_id,
                self.conversation_id,
                item_id,
                offset,
                chunk,
            )
            .await?;
            return Ok(item_id);
        }
        let item_id = Uuid::new_v4();
        let seq = last_seq + 1;
        sqlx::query(
            "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text, provider_id)
             VALUES ($1, $2, $3, $4, 'assistant', $5, $6)",
        )
        .bind(item_id)
        .bind(self.conversation_id)
        .bind(seq)
        .bind(self.turn_id)
        .bind(chunk)
        .bind(&self.provider_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE conversations SET last_item_seq = $2 WHERE id = $1")
            .bind(self.conversation_id)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        crate::db::events::item_upsert(tx, self.bot_id, item_id).await?;
        Ok(item_id)
    }

    /// Persist buffered text. Called by the adapter path, a periodic timer,
    /// and before the terminal outcome. Buffered text is removed only after
    /// its transaction commits, so a rolled-back write keeps it for the next
    /// attempt. Not cancellation safe: callers must await it to completion.
    pub async fn flush(&self) -> Result<(), Fenced> {
        let mut buffer = self.buffer.lock().await;
        if self.is_fenced() {
            return Err(Fenced);
        }
        let chunk = self.pending().clone();
        if chunk.is_empty() {
            return Ok(());
        }
        let result: Result<Option<Uuid>, sqlx::Error> = async {
            let mut tx = self.pool.begin().await?;
            // Text may still arrive while a stop is being confirmed; it is
            // real output and is preserved.
            let Some(last_seq) = self
                .guard(&mut tx, "('sending', 'running', 'cancelling')")
                .await?
            else {
                return Ok(None);
            };
            let item = self
                .write_text(&mut tx, buffer.assistant_item, last_seq, &chunk)
                .await?;
            tx.commit().await?;
            Ok(Some(item))
        }
        .await;
        match result {
            Ok(Some(item_id)) => {
                buffer.assistant_item = Some(item_id);
                // Text appended during the write stays for the next flush.
                self.pending().drain(..chunk.len());
                Ok(())
            }
            Ok(None) => Err(self.fence()),
            Err(err) => {
                // Transient: keep the text and let the next flush retry.
                tracing::warn!(turn = %self.turn_id, error = %err, "persisting assistant text failed");
                Ok(())
            }
        }
    }

    /// Record the terminal outcome and any remaining text in one
    /// transaction, so a turn is never reported finished without its text.
    /// The turn status is internal state and is recorded even if the bot was
    /// deleted; visible history is not appended in that case. On error
    /// nothing is lost and the call can be retried.
    pub async fn finish(&self, outcome: &TurnOutcome) -> Result<(), sqlx::Error> {
        let (status, code, message, uncertain) = match outcome {
            TurnOutcome::Completed => ("completed", None, None, false),
            TurnOutcome::Interrupted => ("interrupted", None, None, false),
            TurnOutcome::Failed { code, message } => {
                ("failed", Some(code.as_str()), Some(message.as_str()), false)
            }
            TurnOutcome::Uncertain { message } => (
                "uncertain",
                Some("delivery_uncertain"),
                Some(message.as_str()),
                true,
            ),
        };
        let mut buffer = self.buffer.lock().await;
        let chunk = self.pending().clone();
        let mut tx = self.pool.begin().await?;
        let mut wrote_item = None;
        if !chunk.is_empty() && !self.is_fenced() {
            if let Some(last_seq) = self.guard(&mut tx, ACTIVE_STATUSES).await? {
                wrote_item = Some(
                    self.write_text(&mut tx, buffer.assistant_item, last_seq, &chunk)
                        .await?,
                );
            }
        } else {
            sqlx::query("SELECT 1 FROM bots WHERE id = $1 FOR UPDATE")
                .bind(self.bot_id)
                .execute(&mut *tx)
                .await?;
        }
        let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE turns SET status = $2, error_code = $3, error_message = $4,
                 effect_uncertain = effect_uncertain OR $5, finished_at = now(), updated_at = now()
             WHERE id = $1 AND status IN {ACTIVE_STATUSES}"
        )))
        .bind(self.turn_id)
        .bind(status)
        .bind(code)
        .bind(message)
        .bind(uncertain)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 1 {
            crate::db::events::turn_changed(&mut tx, self.bot_id, self.turn_id).await?;
        }
        // Confirm a pending Stop work or deletion now that this turn ended.
        crate::db::stop::settle(&mut tx, self.bot_id).await?;
        tx.commit().await?;
        if let Some(item_id) = wrote_item {
            buffer.assistant_item = Some(item_id);
        }
        self.pending().drain(..chunk.len());
        Ok(())
    }
}

#[async_trait]
impl TurnSink for PgSink {
    async fn sending(&self) -> Result<(), Fenced> {
        self.transition("preparing", "sending").await
    }

    async fn running(&self) -> Result<(), Fenced> {
        // A stop may already have moved the turn to `cancelling`; the turn
        // was still accepted and must be drained, so that is not a fence.
        match self.transition("sending", "running").await {
            Ok(()) => Ok(()),
            Err(Fenced) => {
                let cancelling: Result<bool, sqlx::Error> = sqlx::query_scalar(
                    "SELECT t.status = 'cancelling' AND b.status = 'active'
                     FROM turns t JOIN bots b ON b.id = t.bot_id WHERE t.id = $1",
                )
                .bind(self.turn_id)
                .fetch_one(&self.pool)
                .await;
                if matches!(cancelling, Ok(true)) {
                    *self.fenced.lock().unwrap_or_else(|p| p.into_inner()) = false;
                    Ok(())
                } else {
                    Err(Fenced)
                }
            }
        }
    }

    async fn session(&self, state: &SessionState) -> Result<(), Fenced> {
        if self.is_fenced() {
            return Err(Fenced);
        }
        let result: Result<bool, sqlx::Error> = async {
            let mut tx = self.pool.begin().await?;
            if self.guard(&mut tx, ACTIVE_STATUSES).await?.is_none() {
                return Ok(false);
            }
            let existing: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM provider_sessions
                 WHERE conversation_id = $1 AND provider_id = $2 AND status = 'active'
                   AND native_session_id = $3 AND fingerprint = $4",
            )
            .bind(self.conversation_id)
            .bind(&self.provider_id)
            .bind(&state.native_id)
            .bind(&state.fingerprint)
            .fetch_optional(&mut *tx)
            .await?;
            let session_id = match existing {
                Some(id) => {
                    sqlx::query(
                        "UPDATE provider_sessions SET considered_through_turn = $2, updated_at = now()
                         WHERE id = $1",
                    )
                    .bind(id)
                    .bind(state.considered_through_turn)
                    .execute(&mut *tx)
                    .await?;
                    id
                }
                None => {
                    // A replacement retires the previous native session.
                    sqlx::query(
                        "UPDATE provider_sessions SET status = 'retired', updated_at = now()
                         WHERE conversation_id = $1 AND provider_id = $2 AND status = 'active'",
                    )
                    .bind(self.conversation_id)
                    .bind(&self.provider_id)
                    .execute(&mut *tx)
                    .await?;
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO provider_sessions (id, bot_id, conversation_id, provider_id,
                             native_session_id, fingerprint, considered_through_turn)
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    )
                    .bind(id)
                    .bind(self.bot_id)
                    .bind(self.conversation_id)
                    .bind(&self.provider_id)
                    .bind(&state.native_id)
                    .bind(&state.fingerprint)
                    .bind(state.considered_through_turn)
                    .execute(&mut *tx)
                    .await?;
                    id
                }
            };
            sqlx::query("UPDATE turns SET provider_session_id = $2 WHERE id = $1")
                .bind(self.turn_id)
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(true)
        }
        .await;
        match result {
            Ok(true) => Ok(()),
            Ok(false) => Err(self.fence()),
            // Bookkeeping failure: without a recorded session the next turn
            // simply starts a new one, so output is never fenced for it.
            Err(err) => {
                tracing::warn!(turn = %self.turn_id, error = %err, "recording provider session failed");
                Ok(())
            }
        }
    }

    async fn reported(
        &self,
        settings: &serde_json::Value,
        native_turn_id: &str,
    ) -> Result<(), Fenced> {
        if self.is_fenced() {
            return Err(Fenced);
        }
        // Bookkeeping: a database error is logged, never fences the output.
        if let Err(err) =
            sqlx::query("UPDATE turns SET reported_profile = $2, native_turn_id = $3 WHERE id = $1")
                .bind(self.turn_id)
                .bind(Json(settings))
                .bind(native_turn_id)
                .execute(&self.pool)
                .await
        {
            tracing::warn!(turn = %self.turn_id, error = %err, "recording reported settings failed");
        }
        Ok(())
    }

    async fn text(&self, chunk: &str) -> Result<(), Fenced> {
        if self.is_fenced() {
            return Err(Fenced);
        }
        // Buffer only. Database writes belong to the scheduler's flusher, so
        // abandoning an adapter future can never interrupt a commit.
        let mut pending = self.pending();
        pending.push_str(chunk);
        if pending.len() >= FLUSH_BYTES {
            self.flush_wanted.notify_one();
        }
        Ok(())
    }
}

/// Startup reconciliation, run before the scheduler admits work. A turn
/// claimed but never delivered returns to the queue. A turn that may have
/// reached its provider is never replayed: it becomes `uncertain`, keeping
/// any partial text. Returns (requeued, uncertain).
pub async fn recover_after_restart(pool: &PgPool) -> Result<(u64, u64), sqlx::Error> {
    let mut tx = pool.begin().await?;
    let requeued: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'queued', started_at = NULL, updated_at = now()
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
             finished_at = now(), updated_at = now()
         WHERE status IN ('sending', 'running', 'cancelling') RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    // A claim that never reached its provider, on a bot being stopped or
    // deleted, simply ends.
    let mut uncertain = uncertain;
    let halted: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "UPDATE turns SET status = 'interrupted', error_code = 'cancelled',
             error_message = 'Stopped before it was sent.', finished_at = now(), updated_at = now()
         WHERE status = 'preparing' RETURNING id, bot_id",
    )
    .fetch_all(&mut *tx)
    .await?;
    uncertain.extend(halted);
    for (turn_id, bot_id) in requeued.iter().chain(&uncertain) {
        crate::db::events::turn_changed(&mut tx, *bot_id, *turn_id).await?;
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
pub async fn settle_orphans(pool: &PgPool, live: &[Uuid]) -> Result<u64, sqlx::Error> {
    let orphans: Vec<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, bot_id FROM turns WHERE status IN {ACTIVE_STATUSES} AND NOT (id = ANY($1))"
    )))
    .bind(live)
    .fetch_all(pool)
    .await?;
    let mut settled = 0;
    for (turn_id, bot_id) in orphans {
        let mut tx = pool.begin().await?;
        sqlx::query("SELECT 1 FROM bots WHERE id = $1 FOR UPDATE")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        let updated = sqlx::query(
            "UPDATE turns SET
                 status = CASE WHEN status = 'preparing' THEN 'failed' ELSE 'uncertain' END,
                 effect_uncertain = effect_uncertain OR status <> 'preparing',
                 error_code = CASE WHEN status = 'preparing' THEN 'start_failed' ELSE 'orphaned' END,
                 error_message = CASE WHEN status = 'preparing'
                     THEN 'The message could not be started. Send it again.'
                     ELSE 'Processing stopped unexpectedly. It was not resent; send a follow-up to continue.'
                 END,
                 finished_at = now(), updated_at = now()
             WHERE id = $1 AND status IN ('preparing', 'sending', 'running', 'cancelling')",
        )
        .bind(turn_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 1 {
            crate::db::events::turn_changed(&mut tx, bot_id, turn_id).await?;
            crate::db::stop::settle(&mut tx, bot_id).await?;
            settled += 1;
        }
        tx.commit().await?;
    }
    Ok(settled)
}

/// IDs of turns being stopped.
pub async fn cancelling(pool: &PgPool) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT id FROM turns WHERE status = 'cancelling'")
        .fetch_all(pool)
        .await
}
