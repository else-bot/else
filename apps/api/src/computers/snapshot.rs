use super::{
    ComputerError, ComputerManager, DiskCapture, GUEST_CHECKPOINT, GUEST_PROJECT, ROOT_PROJECT,
    Reason, Row, RuntimeState,
};
use std::time::Duration;
use uuid::Uuid;

impl ComputerManager {
    /// Export a bot's computer disk for a backup, under its
    /// operation lock. An idle running or paused computer is stopped first
    /// through the normal stop (a desktop checkpoints Chromium); a stopped
    /// one, including an owner-stopped one, is exported without being
    /// started. A held or transitioning computer is `Busy`: the backup
    /// defers rather than capturing live work.
    pub async fn capture_disk(
        &self,
        bot_id: Uuid,
        archive: &std::path::Path,
    ) -> Result<DiskCapture, ComputerError> {
        let _op = self.op(bot_id).await;
        let row = {
            let mut conn = self.pool.acquire().await?;
            Self::row(&mut conn, bot_id).await?
        };
        let Some(row) = row else {
            return Ok(DiskCapture::NoComputer);
        };
        let was_running = matches!(row.observed_state.as_str(), "running" | "paused");
        match row.observed_state.as_str() {
            "absent" => return Ok(DiskCapture::NoComputer),
            "missing" => return Ok(DiskCapture::Missing),
            _ => self.stop_for_disk(bot_id, &row).await?,
        }
        self.runtime.export_disk(&row.runtime_name, archive).await?;
        let project_mount = self
            .runtime
            .mounts(&row.runtime_name)
            .await?
            .into_iter()
            .find(|m| m.guest == GUEST_PROJECT || m.guest == ROOT_PROJECT);
        let (port, browser_checkpoint): (Option<i32>, Option<String>) = sqlx::query_as(
            "SELECT desktop_port, browser_checkpoint FROM bot_computers WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(DiskCapture::Exported {
            runtime_name: row.runtime_name,
            image: row.image,
            cpus: row.cpus as u32,
            memory_mib: row.memory_mib as u32,
            desired_state: row.desired_state,
            desktop_port: port.map(|p| p as u16),
            was_running,
            browser_checkpoint,
            project_mount,
        })
    }

    pub(super) async fn stop_for_disk(&self, bot_id: Uuid, row: &Row) -> Result<(), ComputerError> {
        match row.observed_state.as_str() {
            "absent" | "missing" => {
                return Err(ComputerError::Busy(format!("state {}", row.observed_state)));
            }
            "stopped" => {}
            "running" | "paused" => {
                let leases: Vec<(Uuid, String)> = sqlx::query_as(
                    "SELECT id, holder FROM computer_leases
                     WHERE bot_id = ?1 AND released_at IS NULL",
                )
                .bind(bot_id)
                .fetch_all(&self.pool)
                .await?;
                let mut held = false;
                for (lease, holder) in leases {
                    let ended = matches!(
                        holder.split_once(':'),
                        Some(("turn" | "job", id)) if id.parse::<Uuid>().is_ok()
                    ) && !self.holder_is_live(&holder).await?;
                    if !ended {
                        held = true;
                        continue;
                    }
                    sqlx::query(
                        "UPDATE computer_leases SET released_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), end_reason = 'expired'
                         WHERE id = ?1 AND released_at IS NULL",
                    )
                    .bind(lease)
                    .execute(&self.pool)
                    .await?;
                }
                if held {
                    return Err(ComputerError::Busy("in use".into()));
                }
                if self.config.desktop {
                    if row.observed_state == "paused" {
                        self.runtime.resume(&row.runtime_name).await?;
                        let mut conn = self.pool.acquire().await?;
                        self.set_state(&mut conn, bot_id, "running", None, None)
                            .await?;
                    }
                    let out = self
                        .runtime
                        .exec(
                            &row.runtime_name,
                            GUEST_CHECKPOINT,
                            &[],
                            Duration::from_secs(60),
                        )
                        .await?;
                    if out.status != Some(0) {
                        return Err(ComputerError::Busy(format!(
                            "its browser checkpoint failed: {}",
                            out.stderr.trim().chars().take(200).collect::<String>()
                        )));
                    }
                }
                if !self.stop_held(bot_id, Reason::Backup).await? {
                    return Err(ComputerError::Busy("in use".into()));
                }
            }
            other => return Err(ComputerError::Busy(format!("state {other}"))),
        }
        if self.runtime.state(&row.runtime_name).await? != Some(RuntimeState::Stopped) {
            return Err(ComputerError::Busy(
                "the runtime does not report it stopped".into(),
            ));
        }
        Ok(())
    }
}
