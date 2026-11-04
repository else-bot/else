//! Lease creation, renewal, release, and checked execution.

use super::{ComputerError, ComputerManager, ExecOutput, OwnedSemaphorePermit, SqliteConnection};
use std::time::Duration;
use uuid::Uuid;

impl ComputerManager {
    pub(super) async fn lease(
        &self,
        tx: &mut SqliteConnection,
        bot_id: Uuid,
        holder: &str,
        generation: i64,
    ) -> Result<Uuid, sqlx::Error> {
        let existing: Option<Uuid> = sqlx::query_scalar(
            "UPDATE computer_leases SET expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', printf('%+f seconds', ?3)),
                 generation = ?4
             WHERE bot_id = ?1 AND holder = ?2 AND released_at IS NULL RETURNING id",
        )
        .bind(bot_id)
        .bind(holder)
        .bind(self.config.lease_ttl.as_secs_f64())
        .bind(generation)
        .fetch_optional(&mut *tx)
        .await?;
        let id = match existing {
            Some(id) => id,
            None => {
                let id = Uuid::new_v4();
                sqlx::query(
                    "INSERT INTO computer_leases (id, bot_id, holder, generation, expires_at)
                     VALUES (?1, ?2, ?3, ?4, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', printf('%+f seconds', ?5)))",
                )
                .bind(id)
                .bind(bot_id)
                .bind(holder)
                .bind(generation)
                .bind(self.config.lease_ttl.as_secs_f64())
                .execute(&mut *tx)
                .await?;
                id
            }
        };
        sqlx::query("UPDATE bot_computers SET last_used_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE bot_id = ?1")
            .bind(bot_id)
            .execute(&mut *tx)
            .await?;
        Ok(id)
    }

    /// Extend a live lease. False if it already ended.
    pub async fn renew(&self, lease_id: Uuid) -> Result<bool, ComputerError> {
        let renewed = sqlx::query(
            "UPDATE computer_leases SET expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', printf('%+f seconds', ?2))
             WHERE id = ?1 AND released_at IS NULL",
        )
        .bind(lease_id)
        .bind(self.config.lease_ttl.as_secs_f64())
        .execute(&self.pool)
        .await?
        .rows_affected();
        if crate::faults::hit("lease.after_renew") {
            return Err(ComputerError::Interrupted);
        }
        Ok(renewed == 1)
    }

    /// End a lease. Repeating it is harmless. Locks the computer, then the
    /// lease, like every other path.
    pub async fn release(&self, lease_id: Uuid) -> Result<(), ComputerError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let bot: Option<Uuid> =
            sqlx::query_scalar("SELECT bot_id FROM computer_leases WHERE id = ?1")
                .bind(lease_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some(bot) = bot else { return Ok(()) };
        sqlx::query("SELECT 1 FROM bot_computers WHERE bot_id = ?1")
            .bind(bot)
            .execute(&mut *tx)
            .await?;
        let ended = sqlx::query(
            "UPDATE computer_leases SET released_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), end_reason = 'released'
             WHERE id = ?1 AND released_at IS NULL",
        )
        .bind(lease_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if ended == 1 {
            sqlx::query("UPDATE bot_computers SET last_used_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE bot_id = ?1")
                .bind(bot)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Run a command in the computer a live lease holds. Checked and run
    /// under the bot's operation lock, so a stop cannot slip in between the
    /// check and the command (which would otherwise wake a stopped VM).
    pub async fn exec(
        &self,
        lease_id: Uuid,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, ComputerError> {
        let (bot, name, _op) = self.checked(lease_id).await?;
        let out = self.runtime.exec(&name, program, args, timeout).await?;
        sqlx::query("UPDATE bot_computers SET last_used_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE bot_id = ?1")
            .bind(bot)
            .execute(&self.pool)
            .await?;
        Ok(out)
    }

    /// Validate a lease under its bot's operation lock: live, its bot
    /// active, and its computer running at the lease's generation with no
    /// owner stop. The returned guard keeps a stop from slipping in before
    /// the caller's runtime call (which could otherwise wake a stopped VM).
    pub(crate) async fn checked(
        &self,
        lease_id: Uuid,
    ) -> Result<(Uuid, String, OwnedSemaphorePermit), ComputerError> {
        let bot: Option<Uuid> =
            sqlx::query_scalar("SELECT bot_id FROM computer_leases WHERE id = ?1")
                .bind(lease_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(bot) = bot else {
            return Err(ComputerError::LeaseEnded);
        };
        let op = self.op(bot).await;
        let ok: Option<(String, bool, bool)> = sqlx::query_as(
            "SELECT c.runtime_name,
                    c.observed_state = 'running' AND c.desired_state = 'auto'
                        AND l.generation = c.generation,
                    l.released_at IS NULL AND b.status = 'active'
             FROM computer_leases l JOIN bot_computers c ON c.bot_id = l.bot_id
             JOIN bots b ON b.id = l.bot_id WHERE l.id = ?1",
        )
        .bind(lease_id)
        .fetch_optional(&self.pool)
        .await?;
        match ok {
            Some((name, true, true)) => Ok((bot, name, op)),
            Some((_, false, true)) => Err(ComputerError::NotRunning),
            _ => Err(ComputerError::LeaseEnded),
        }
    }
}
