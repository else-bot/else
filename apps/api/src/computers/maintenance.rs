use super::{
    ComputerError, ComputerManager, GUEST_CHECKPOINT, IdleAction, Ordering, Reason, RuntimeState,
};
use crate::db::turns::ACTIVE_STATUSES;
use std::mem::DropGuard;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

impl ComputerManager {
    /// Owner stop: no implicit wake until resumed. Active leases are
    /// revoked and the computer stops; its disk is kept.
    pub async fn owner_stop(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("SELECT 1 FROM bots WHERE id = ?1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib, desired_state, network)
             VALUES (?1, ?2, ?3, ?4, ?5, 'stopped', ?6)
             ON CONFLICT (bot_id) DO UPDATE SET desired_state = 'stopped', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')",
        )
        .bind(bot_id)
        .bind(Self::runtime_name(bot_id))
        .bind(&self.config.image)
        .bind(self.config.cpus as i32)
        .bind(self.config.memory_mib as i32)
            .bind(&self.config.network)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE computer_leases SET released_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), end_reason = 'owner_stop'
             WHERE bot_id = ?1 AND released_at IS NULL",
        )
        .bind(bot_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.end_bot_commands(bot_id).await;
        let _op = self.op(bot_id).await;
        self.stop_held(bot_id, Reason::MustStop).await.map(|_| ())
    }

    /// Clear an owner stop. The computer starts again only on next use.
    pub async fn owner_resume(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        sqlx::query(
            "UPDATE bot_computers SET desired_state = 'auto', updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(super) async fn stop_held(
        &self,
        bot_id: Uuid,
        reason: Reason,
    ) -> Result<bool, ComputerError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active: Option<bool> =
            sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = ?1")
                .bind(bot_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(row) = Self::row(&mut tx, bot_id).await? else {
            return Ok(false);
        };
        let action = match reason {
            Reason::MustStop => {
                if active == Some(true) && row.desired_state != "stopped" {
                    return Ok(false);
                }
                sqlx::query(
                    "UPDATE computer_leases SET released_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), end_reason = 'revoked'
                     WHERE bot_id = ?1 AND released_at IS NULL",
                )
                .bind(bot_id)
                .execute(&mut *tx)
                .await?;
                IdleAction::Stop
            }
            Reason::Backup => {
                let held: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM computer_leases
                                    WHERE bot_id = ?1 AND released_at IS NULL)",
                )
                .bind(bot_id)
                .fetch_one(&mut *tx)
                .await?;
                if held {
                    return Ok(false);
                }
                IdleAction::Stop
            }
            Reason::ForJobs => {
                if row.observed_state != "paused" {
                    return Ok(false);
                }
                IdleAction::Stop
            }
            Reason::Idle(action) => {
                let idle: bool = sqlx::query_scalar(
                    "SELECT NOT EXISTS (SELECT 1 FROM computer_leases
                                        WHERE bot_id = ?1 AND released_at IS NULL)
                        AND coalesce(last_used_at, updated_at) <= strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', printf('%+f seconds', -?2))
                     FROM bot_computers WHERE bot_id = ?1",
                )
                .bind(bot_id)
                .bind(self.config.idle_after.as_secs_f64())
                .fetch_one(&mut *tx)
                .await?;
                if !idle || active != Some(true) || row.desired_state != "auto" {
                    return Ok(false);
                }
                action
            }
        };
        let applies = match action {
            IdleAction::Stop => {
                matches!(row.observed_state.as_str(), "running" | "paused" | "error")
            }
            IdleAction::Pause => row.observed_state == "running",
        };
        if !applies {
            tx.commit().await?;
            return Ok(false);
        }
        let (transient, done) = match action {
            IdleAction::Stop => ("stopping", "stopped"),
            IdleAction::Pause => ("pausing", "paused"),
        };
        self.set_state(&mut tx, bot_id, transient, Some(row.generation + 1), None)
            .await?;
        tx.commit().await?;
        if crate::faults::hit("computer.after_stop_decision") {
            return Err(ComputerError::Interrupted);
        }
        tracing::info!(bot = %bot_id, ?reason, "computer transition");
        if self.config.desktop && action == IdleAction::Stop && row.observed_state == "running" {
            let checkpointed = self
                .runtime
                .exec(
                    &row.runtime_name,
                    GUEST_CHECKPOINT,
                    &[],
                    Duration::from_secs(30),
                )
                .await
                .is_ok_and(|out| out.status == Some(0));
            sqlx::query("UPDATE bot_computers SET browser_checkpoint = ?2 WHERE bot_id = ?1")
                .bind(bot_id)
                .bind(if checkpointed { "ok" } else { "failed" })
                .execute(&self.pool)
                .await?;
        }
        let result = match action {
            IdleAction::Stop => self.runtime.stop(&row.runtime_name).await,
            IdleAction::Pause => self.runtime.pause(&row.runtime_name).await,
        };
        match result {
            Ok(()) => {
                let mut conn = self.pool.acquire().await?;
                self.set_state(&mut conn, bot_id, done, None, None).await?;
                Ok(true)
            }
            Err(err) => {
                self.observe_after_failure(bot_id, &err.to_string()).await?;
                Ok(false)
            }
        }
    }

    /// One maintenance pass: finish the startup inspection if needed,
    /// reconcile expired leases against live work, re-inspect uncertain
    /// computers, stop computers of deleted or owner-stopped bots (retried
    /// until confirmed), and stop or pause idle ones. Stops early when
    /// cancelled.
    pub async fn tick(&self, cancel: &CancellationToken) -> Result<(), ComputerError> {
        if !self.inspected.load(Ordering::SeqCst) {
            self.reconcile_startup().await?;
            if !self.inspected.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
        let expired: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
            "SELECT id, bot_id, holder FROM computer_leases
             WHERE released_at IS NULL AND expires_at < strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')",
        )
        .fetch_all(&self.pool)
        .await?;
        for (lease, bot, holder) in expired {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if self.holder_is_live(&holder).await? {
                self.renew(lease).await?;
                continue;
            }
            let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
            sqlx::query("SELECT 1 FROM bot_computers WHERE bot_id = ?1")
                .bind(bot)
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE computer_leases SET released_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), end_reason = 'expired'
                 WHERE id = ?1 AND released_at IS NULL AND expires_at < strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')",
            )
            .bind(lease)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        let uncertain: Vec<Uuid> = sqlx::query_scalar(
            "SELECT bot_id FROM bot_computers WHERE observed_state IN
                 ('error', 'creating', 'starting', 'resuming', 'stopping', 'pausing')",
        )
        .fetch_all(&self.pool)
        .await?;
        for bot in uncertain {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let Some(_op) = self.try_op(bot).await else {
                continue;
            };
            self.observe(bot).await?;
        }
        if let Err(err) = self.poll_jobs(cancel).await {
            tracing::warn!(error = %err, "polling guest jobs failed");
        }
        let must_stop: Vec<Uuid> = sqlx::query_scalar(
            "SELECT c.bot_id FROM bot_computers c JOIN bots b ON b.id = c.bot_id
             WHERE c.observed_state IN ('running', 'paused', 'error')
               AND (b.status <> 'active' OR c.desired_state = 'stopped')",
        )
        .fetch_all(&self.pool)
        .await?;
        for bot in must_stop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let Some(_op) = self.try_op(bot).await else {
                continue;
            };
            self.stop_held(bot, Reason::MustStop).await?;
        }
        let idle_states: Vec<&str> = match self.config.idle_action {
            IdleAction::Stop => vec!["running", "paused"],
            IdleAction::Pause => vec!["running"],
        };
        let idle: Vec<Uuid> = sqlx::query_scalar(
            "SELECT c.bot_id FROM bot_computers c
             WHERE c.observed_state IN (SELECT value FROM json_each(?1))
               AND coalesce(c.last_used_at, c.updated_at) <= strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', printf('%+f seconds', -?2))
               AND NOT EXISTS (SELECT 1 FROM computer_leases l
                               WHERE l.bot_id = c.bot_id AND l.released_at IS NULL)",
        )
        .bind(sqlx::types::Json(&idle_states))
        .bind(self.config.idle_after.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        for bot in idle {
            if cancel.is_cancelled() {
                return Ok(());
            }
            let Some(_op) = self.try_op(bot).await else {
                continue;
            };
            self.stop_held(bot, Reason::Idle(self.config.idle_action))
                .await?;
        }
        self.clear_stranded_upgrades().await?;
        crate::db::stop::settle_all(&self.pool).await?;
        Ok(())
    }

    pub(super) async fn holder_is_live(&self, holder: &str) -> Result<bool, sqlx::Error> {
        let Some((kind, id)) = holder.split_once(':') else {
            return Ok(false);
        };
        let Ok(id) = id.parse::<Uuid>() else {
            return Ok(false);
        };
        match kind {
            "turn" => {
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT EXISTS (SELECT 1 FROM turns WHERE id = ?1
                                AND status IN {ACTIVE_STATUSES})"
                )))
                .bind(id)
                .fetch_one(&self.pool)
                .await
            }
            "job" => {
                sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM guest_jobs WHERE id = ?1
                                    AND status IN ('starting', 'running', 'cancelling'))",
                )
                .bind(id)
                .fetch_one(&self.pool)
                .await
            }
            _ => Ok(false),
        }
    }

    /// Startup: record what the runtime reports without changing it. A
    /// recorded computer that is gone becomes `missing`, never recreated;
    /// an interrupted transition takes the runtime's actual state. Until
    /// this succeeds no computer is used, and maintenance retries it.
    pub async fn reconcile_startup(&self) -> Result<(), ComputerError> {
        let listed = match self.runtime.list().await {
            Ok(listed) => listed,
            Err(err) => {
                tracing::warn!(error = %err, "computer runtime inspection failed; retrying");
                return Ok(());
            }
        };
        let rows: Vec<(Uuid, String, String)> =
            sqlx::query_as("SELECT bot_id, runtime_name, observed_state FROM bot_computers")
                .fetch_all(&self.pool)
                .await?;
        for (bot, name, observed) in rows {
            let actual = listed.iter().find(|(n, _)| *n == name).map(|(_, s)| *s);
            let state = match actual {
                Some(RuntimeState::Running) => "running",
                Some(RuntimeState::Paused) => "paused",
                Some(RuntimeState::Stopped) => "stopped",
                Some(RuntimeState::Failed) => "error",
                None if observed == "absent" || observed == "creating" => "absent",
                None => "missing",
            };
            if state != observed {
                tracing::info!(bot = %bot, from = %observed, to = state, "computer state reconciled");
            }
            sqlx::query(
                "UPDATE bot_computers SET observed_state = ?2, generation = generation + 1,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
                 WHERE bot_id = ?1",
            )
            .bind(bot)
            .bind(state)
            .execute(&self.pool)
            .await?;
        }
        self.clear_stranded_upgrades().await?;
        sqlx::query(
            "UPDATE computer_leases AS l SET generation = c.generation
             FROM bot_computers c
             WHERE c.bot_id = l.bot_id AND l.released_at IS NULL
               AND c.observed_state IN ('running', 'paused')",
        )
        .execute(&self.pool)
        .await?;
        self.inspected.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Maintenance loop until cancelled.
    pub async fn run(self: Arc<Self>, cancel: CancellationToken) {
        let orphans = {
            let manager = self.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    if let Err(err) = manager.end_orphaned_commands().await {
                        tracing::warn!(error = %err, "ending stopped foreground commands failed");
                    }
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
            })
        };
        let _orphans = DropGuard::new(orphans, |task| task.abort());
        loop {
            if let Err(err) = self.tick(&cancel).await {
                tracing::warn!(error = %err, "computer maintenance failed");
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }
}
