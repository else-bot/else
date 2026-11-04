//! Actor-owned persistence of one provider attempt. Queue acceptance and
//! restart recovery live in `turns`; only this sink buffers provider text.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::turns::ACTIVE_STATUSES;
use crate::actor::Actor;
use crate::app::providers::{Fenced, SessionState, TurnInput, TurnOutcome, TurnSink};

/// Flush buffered provider text at least this often or at this size.
pub const FLUSH_INTERVAL: Duration = Duration::from_millis(100);
pub const FLUSH_BYTES: usize = 2048;

struct OwnerTranscript {
    conversation_id: Uuid,
    last_seq: i64,
}

/// Persists one turn's progress. Writes succeed only while the turn is in
/// an active status and its bot is active.
struct SinkState {
    pool: SqlitePool,
    turn_id: Uuid,
    bot_id: Uuid,
    execution_conversation_id: Uuid,
    provider_id: String,
    /// Only the actor touches text and serializes writes with `finish`.
    assistant_item: Option<Uuid>,
    /// Text not yet committed. Bounded by the flush threshold and
    /// drained only after the transaction that wrote it commits.
    pending: String,
    flush_wanted: Arc<tokio::sync::Notify>,
    fenced: bool,
    attempt: i32,
    /// Child outcomes this attempt's input carried.
    delivered: Vec<Uuid>,
    /// Secret request answers this attempt's input carried.
    delivered_secrets: Vec<Uuid>,
}

pub struct SqliteSink {
    state: Actor<SinkState>,
    flush_wanted: Arc<tokio::sync::Notify>,
}

impl SqliteSink {
    pub fn new(pool: SqlitePool, input: &TurnInput, provider_id: &str) -> Self {
        let flush_wanted = Arc::new(tokio::sync::Notify::new());
        Self {
            state: Actor::new(SinkState {
                pool,
                turn_id: input.turn_id,
                bot_id: input.bot_id,
                execution_conversation_id: input.conversation_id,
                provider_id: provider_id.to_owned(),
                assistant_item: None,
                pending: String::new(),
                flush_wanted: flush_wanted.clone(),
                fenced: false,
                attempt: input.attempt,
                delivered: input.delegation.outcomes.iter().map(|o| o.id).collect(),
                delivered_secrets: input.secrets.iter().map(|o| o.id).collect(),
            }),
            flush_wanted,
        }
    }

    pub async fn flush_wanted(&self) {
        self.flush_wanted.notified().await;
    }
    pub async fn flush(&self) -> Result<(), Fenced> {
        self.state.call_async(|state| Box::pin(state.flush())).await
    }
    pub async fn finish(&self, outcome: &TurnOutcome) -> Result<(), sqlx::Error> {
        let outcome = outcome.clone();
        self.state
            .call_async(move |state| Box::pin(async move { state.finish(&outcome).await }))
            .await
    }
}

impl SinkState {
    fn fence(&mut self) -> Fenced {
        self.fenced = true;
        Fenced
    }

    /// Lock the bot row and check the turn may still write.
    async fn guard(
        &self,
        conn: &mut SqliteConnection,
        allowed: &str,
    ) -> Result<Option<OwnerTranscript>, sqlx::Error> {
        let row: Option<(String, Uuid, i64)> = sqlx::query_as(
            "SELECT b.status, c.id, c.last_item_seq
             FROM bots b JOIN conversations c ON c.bot_id = b.id AND c.kind = 'owner'
             WHERE b.id = ?1",
        )
        .bind(self.bot_id)
        .fetch_optional(&mut *conn)
        .await?;
        let Some((_, conversation_id, last_seq)) = row.filter(|(status, ..)| status == "active")
        else {
            return Ok(None);
        };
        let turn_ok: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE id = ?1 AND status IN {allowed})"
        )))
        .bind(self.turn_id)
        .fetch_one(&mut *conn)
        .await?;
        Ok(turn_ok.then_some(OwnerTranscript {
            conversation_id,
            last_seq,
        }))
    }

    async fn transition(&mut self, from: &str, to: &str) -> Result<(), Fenced> {
        if self.fenced {
            return Err(Fenced);
        }
        let result: Result<bool, sqlx::Error> = async {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            if self.guard(&mut tx, &format!("('{from}')")).await?.is_none() {
                return Ok(false);
            }
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE turns SET status = '{to}', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1"
            )))
            .bind(self.turn_id)
            .execute(&mut *tx)
            .await?;
            // A handoff this attempt carries follows the delivery state.
            match to {
                "sending" => {
                    crate::db::handoffs::transition(
                        &mut tx,
                        self.turn_id,
                        self.attempt,
                        "prepared",
                        "sending",
                    )
                    .await?
                }
                "running" => {
                    crate::db::handoffs::transition(
                        &mut tx,
                        self.turn_id,
                        self.attempt,
                        "sending",
                        "accepted",
                    )
                    .await?
                }
                _ => {}
            }
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
        tx: &mut SqliteConnection,
        assistant_item: Option<Uuid>,
        transcript: &OwnerTranscript,
        chunk: &str,
    ) -> Result<Uuid, sqlx::Error> {
        if let Some(item_id) = assistant_item {
            let offset: i64 = sqlx::query_scalar(
                "UPDATE items SET text = text || ?2 WHERE id = ?1
                 RETURNING (length(CAST(text AS BLOB)) - length(CAST(?2 AS BLOB)))",
            )
            .bind(item_id)
            .bind(chunk)
            .fetch_one(&mut *tx)
            .await?;
            crate::db::events::item_delta(
                tx,
                self.bot_id,
                transcript.conversation_id,
                item_id,
                offset,
                chunk,
            )
            .await?;
            return Ok(item_id);
        }
        let item_id = Uuid::new_v4();
        let seq = transcript.last_seq + 1;
        sqlx::query(
            "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text, provider_id)
             VALUES (?1, ?2, ?3, ?4, 'assistant', ?5, ?6)",
        )
        .bind(item_id)
        .bind(transcript.conversation_id)
        .bind(seq)
        .bind(self.turn_id)
        .bind(chunk)
        .bind(&self.provider_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE conversations SET last_item_seq = ?2 WHERE id = ?1")
            .bind(transcript.conversation_id)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        crate::db::events::item_upsert(tx, self.bot_id, item_id).await?;
        Ok(item_id)
    }

    /// Persist buffered text. Called by the adapter path, a periodic timer,
    /// and before the terminal outcome. Buffered text is removed only after
    /// its transaction commits, so a rolled-back write keeps it for the next
    /// attempt. The actor completes accepted writes even when the caller is cancelled.
    pub async fn flush(&mut self) -> Result<(), Fenced> {
        if self.fenced {
            return Err(Fenced);
        }
        let chunk = self.pending.clone();
        if chunk.is_empty() {
            return Ok(());
        }
        let result: Result<Option<Uuid>, sqlx::Error> = async {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            // Text may still arrive while a stop is being confirmed; it is
            // real output and is preserved.
            let Some(transcript) = self
                .guard(&mut tx, "('sending', 'running', 'cancelling')")
                .await?
            else {
                return Ok(None);
            };
            let item = self
                .write_text(&mut tx, self.assistant_item, &transcript, &chunk)
                .await?;
            tx.commit().await?;
            Ok(Some(item))
        }
        .await;
        match result {
            Ok(Some(item_id)) => {
                self.assistant_item = Some(item_id);
                self.pending.drain(..chunk.len());
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
    pub async fn finish(&mut self, outcome: &TurnOutcome) -> Result<(), sqlx::Error> {
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
        let chunk = self.pending.clone();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut wrote_item = None;
        if !chunk.is_empty() && !self.fenced {
            if let Some(transcript) = self.guard(&mut tx, ACTIVE_STATUSES).await? {
                wrote_item = Some(
                    self.write_text(&mut tx, self.assistant_item, &transcript, &chunk)
                        .await?,
                );
            }
        } else {
            sqlx::query("SELECT 1 FROM bots WHERE id = ?1")
                .bind(self.bot_id)
                .execute(&mut *tx)
                .await?;
        }
        // Children's bots, then the root turn: a child settling meanwhile
        // takes the same root lock, so parking sees its outcome.
        crate::db::delegations::lock_targets_of_root(&mut tx, self.turn_id).await?;
        sqlx::query("SELECT 1 FROM turns WHERE id = ?1")
            .bind(self.turn_id)
            .execute(&mut *tx)
            .await?;
        // A root that delegated or asked the owner for a secret parks
        // instead of finishing: `waiting` while a child works or the owner
        // has not answered, or `queued` again (a continuation) when every
        // one has an outcome this attempt did not see. A Stop (`cancelling`)
        // ends it normally, and ending withdraws pending secret requests.
        let (current, pending, unseen): (String, i64, i64) = sqlx::query_as(
            "SELECT t.status,
                    (SELECT count(*) FROM delegations d
                     WHERE d.root_turn_id = t.id AND d.status IN ('queued', 'running'))
                  + (SELECT count(*) FROM secret_requests r
                     WHERE r.turn_id = t.id AND r.status = 'pending'),
                    (SELECT count(*) FROM delegations d
                     WHERE d.root_turn_id = t.id AND d.delivered_attempt IS NULL
                       AND d.status IN ('completed', 'failed', 'cancelled'))
                  + (SELECT count(*) FROM secret_requests r
                     WHERE r.turn_id = t.id AND r.delivered_attempt IS NULL
                       AND r.status <> 'pending')
             FROM turns t WHERE t.id = ?1",
        )
        .bind(self.turn_id)
        .fetch_one(&mut *tx)
        .await?;
        let parks = current != "cancelling" && matches!(status, "completed" | "interrupted");
        let status = if parks && pending > 0 {
            "waiting"
        } else if parks && unseen > 0 {
            "queued"
        } else {
            if pending > 0 {
                crate::db::delegations::cancel_for_root(&mut tx, self.turn_id).await?;
            }
            status
        };
        let finished = !matches!(status, "waiting" | "queued");
        let updated = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE turns SET status = ?2, error_code = ?3, error_message = ?4,
                 effect_uncertain = effect_uncertain OR ?5,
                 finished_at = CASE WHEN ?6 THEN strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') END, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1 AND status IN {ACTIVE_STATUSES}"
        )))
        .bind(self.turn_id)
        .bind(status)
        .bind(code)
        .bind(message)
        .bind(uncertain)
        .bind(finished)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        // A handoff sent but never acknowledged is uncertain, unless the
        // provider explicitly refused it before consuming it.
        let refused =
            matches!(outcome, TurnOutcome::Failed { code, .. } if code == "provider_refused");
        crate::db::handoffs::transition(
            &mut tx,
            self.turn_id,
            self.attempt,
            "sending",
            if refused { "prepared" } else { "uncertain" },
        )
        .await?;
        if updated == 1 {
            crate::db::events::turn_changed(&mut tx, self.bot_id, self.turn_id).await?;
            // The reply's final text enters memory with the terminal status.
            crate::db::memory::log_turn_end(&mut tx, self.turn_id).await?;
        }
        // Confirm a pending Stop work or deletion now that this turn ended.
        crate::db::stop::settle(&mut tx, self.bot_id).await?;
        tx.commit().await?;
        if let Some(item_id) = wrote_item {
            self.assistant_item = Some(item_id);
        }
        self.pending.drain(..chunk.len());
        Ok(())
    }
}

impl SinkState {
    async fn packet(
        &mut self,
        selection: &crate::app::providers::Selection,
        payload: &str,
    ) -> Result<(), Fenced> {
        if self.fenced {
            return Err(Fenced);
        }
        if let Err(err) = crate::db::handoffs::record_packet(
            &self.pool,
            self.turn_id,
            self.attempt,
            selection,
            payload,
        )
        .await
        {
            tracing::warn!(turn = %self.turn_id, error = %err, "recording the packet failed");
        }
        Ok(())
    }

    async fn retire_session(&mut self) {
        if let Err(err) = sqlx::query(
            "UPDATE provider_sessions SET status = 'retired', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = (SELECT provider_session_id FROM turns WHERE id = ?1)
               AND status = 'active'",
        )
        .bind(self.turn_id)
        .execute(&self.pool)
        .await
        {
            tracing::warn!(turn = %self.turn_id, error = %err, "retiring the provider session failed");
        }
    }

    async fn sending(&mut self) -> Result<(), Fenced> {
        self.transition("preparing", "sending").await
    }

    async fn running(&mut self) -> Result<(), Fenced> {
        // A stop may already have moved the turn to `cancelling`; the turn
        // was still accepted and must be drained, so that is not a fence.
        match self.transition("sending", "running").await {
            Ok(()) => {
                // Accepted by the provider: the outcomes this attempt carried
                // were delivered.
                if !self.delivered.is_empty() || !self.delivered_secrets.is_empty() {
                    let marked = async {
                        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
                        crate::db::delegations::mark_delivered(
                            &mut tx,
                            self.turn_id,
                            self.attempt,
                            &self.delivered,
                        )
                        .await?;
                        crate::db::secret_requests::mark_delivered(
                            &mut tx,
                            self.turn_id,
                            self.attempt,
                            &self.delivered_secrets,
                        )
                        .await?;
                        tx.commit().await
                    }
                    .await;
                    if let Err(err) = marked {
                        tracing::warn!(turn = %self.turn_id, error = %err, "recording delivered outcomes failed");
                    }
                }
                Ok(())
            }
            Err(Fenced) => {
                let cancelling: Result<bool, sqlx::Error> = sqlx::query_scalar(
                    "SELECT t.status = 'cancelling' AND b.status = 'active'
                     FROM turns t JOIN bots b ON b.id = t.bot_id WHERE t.id = ?1",
                )
                .bind(self.turn_id)
                .fetch_one(&self.pool)
                .await;
                if matches!(cancelling, Ok(true)) {
                    self.fenced = false;
                    // Accepted all the same.
                    let accepted = async {
                        let mut conn = self.pool.acquire().await?;
                        crate::db::handoffs::transition(
                            &mut conn,
                            self.turn_id,
                            self.attempt,
                            "sending",
                            "accepted",
                        )
                        .await
                    }
                    .await;
                    if let Err(err) = accepted {
                        tracing::warn!(turn = %self.turn_id, error = %err, "recording handoff acceptance failed");
                    }
                    Ok(())
                } else {
                    Err(Fenced)
                }
            }
        }
    }

    async fn session(&mut self, state: &SessionState) -> Result<(), Fenced> {
        if self.fenced {
            return Err(Fenced);
        }
        let result: Result<bool, sqlx::Error> = async {
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            if self.guard(&mut tx, ACTIVE_STATUSES).await?.is_none() {
                return Ok(false);
            }
            let existing: Option<Uuid> = sqlx::query_scalar(
                "SELECT id FROM provider_sessions
                 WHERE conversation_id = ?1 AND provider_id = ?2 AND status = 'active'
                   AND native_session_id = ?3 AND fingerprint = ?4",
            )
            .bind(self.execution_conversation_id)
            .bind(&self.provider_id)
            .bind(&state.native_id)
            .bind(&state.fingerprint)
            .fetch_optional(&mut *tx)
            .await?;
            let session_id = match existing {
                Some(id) => {
                    sqlx::query(
                        "UPDATE provider_sessions SET considered_through_turn = ?2,
                             memory_revision = coalesce(?3, memory_revision), updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                         WHERE id = ?1",
                    )
                    .bind(id)
                    .bind(state.considered_through_turn)
                    .bind(state.memory_revision)
                    .execute(&mut *tx)
                    .await?;
                    id
                }
                None => {
                    // A replacement retires the previous native session.
                    sqlx::query(
                        "UPDATE provider_sessions SET status = 'retired', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                         WHERE conversation_id = ?1 AND provider_id = ?2 AND status = 'active'",
                    )
                    .bind(self.execution_conversation_id)
                    .bind(&self.provider_id)
                    .execute(&mut *tx)
                    .await?;
                    let id = Uuid::new_v4();
                    sqlx::query(
                        "INSERT INTO provider_sessions (id, bot_id, conversation_id, provider_id,
                             native_session_id, fingerprint, considered_through_turn, memory_revision)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    )
                    .bind(id)
                    .bind(self.bot_id)
                    .bind(self.execution_conversation_id)
                    .bind(&self.provider_id)
                    .bind(&state.native_id)
                    .bind(&state.fingerprint)
                    .bind(state.considered_through_turn)
                    .bind(state.memory_revision)
                    .execute(&mut *tx)
                    .await?;
                    id
                }
            };
            sqlx::query("UPDATE turns SET provider_session_id = ?2 WHERE id = ?1")
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
        &mut self,
        settings: &serde_json::Value,
        native_turn_id: &str,
    ) -> Result<(), Fenced> {
        if self.fenced {
            return Err(Fenced);
        }
        // Bookkeeping: a database error is logged, never fences the output.
        if let Err(err) =
            sqlx::query("UPDATE turns SET reported_profile = ?2, native_turn_id = ?3 WHERE id = ?1")
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

    async fn text(&mut self, chunk: &str) -> Result<(), Fenced> {
        if self.fenced {
            return Err(Fenced);
        }
        // The actor completes commits even if the adapter future is dropped.
        // Flush at the threshold to apply backpressure to a fast provider.
        // A failed previous write must drain before any more text is admitted.
        if self.pending.len() >= FLUSH_BYTES {
            self.flush().await?;
            if self.pending.len() >= FLUSH_BYTES {
                return Err(Fenced);
            }
        }
        self.pending.push_str(chunk);
        if self.pending.len() >= FLUSH_BYTES {
            self.flush_wanted.notify_one();
            self.flush().await?;
            if self.pending.len() >= FLUSH_BYTES {
                // Stop accepting output after a failed flush. Keep the
                // buffer unfenced so finish can retry its final transaction.
                return Err(Fenced);
            }
        }
        Ok(())
    }

    async fn end_message(&mut self) -> Result<(), Fenced> {
        self.flush().await?;
        if self.pending.is_empty() {
            self.assistant_item = None;
        } else {
            // A failed write keeps the text buffered, so the next message
            // joins it rather than splitting text whose item is unknown.
            self.pending.push_str("\n\n");
        }
        Ok(())
    }
}

#[async_trait]
impl TurnSink for SqliteSink {
    async fn packet(
        &self,
        selection: &crate::app::providers::Selection,
        payload: &str,
    ) -> Result<(), Fenced> {
        let selection = selection.clone();
        let payload = payload.to_owned();
        self.state
            .call_async(move |state| {
                Box::pin(async move { state.packet(&selection, &payload).await })
            })
            .await
    }
    async fn retire_session(&self) {
        self.state
            .call_async(|state| Box::pin(state.retire_session()))
            .await;
    }
    async fn sending(&self) -> Result<(), Fenced> {
        self.state
            .call_async(|state| Box::pin(state.sending()))
            .await
    }
    async fn running(&self) -> Result<(), Fenced> {
        self.state
            .call_async(|state| Box::pin(state.running()))
            .await
    }
    async fn session(&self, session: &SessionState) -> Result<(), Fenced> {
        let session = session.clone();
        self.state
            .call_async(move |state| Box::pin(async move { state.session(&session).await }))
            .await
    }
    async fn reported(
        &self,
        settings: &serde_json::Value,
        native_turn_id: &str,
    ) -> Result<(), Fenced> {
        let settings = settings.clone();
        let native_turn_id = native_turn_id.to_owned();
        self.state
            .call_async(move |state| {
                Box::pin(async move { state.reported(&settings, &native_turn_id).await })
            })
            .await
    }
    async fn text(&self, chunk: &str) -> Result<(), Fenced> {
        // Bound the bytes in each mailbox entry as well as the number of entries.
        let mut remaining = chunk;
        while !remaining.is_empty() {
            let mut end = remaining.len().min(FLUSH_BYTES);
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            let chunk = remaining[..end].to_owned();
            self.state
                .call_async(move |state| Box::pin(async move { state.text(&chunk).await }))
                .await?;
            remaining = &remaining[end..];
        }
        Ok(())
    }
    async fn end_message(&self) -> Result<(), Fenced> {
        self.state
            .call_async(|state| Box::pin(state.end_message()))
            .await
    }
}
