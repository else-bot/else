//! Desktop access, credentials, and guest service startup.

use super::{
    Acquire, ComputerError, ComputerManager, DESKTOP_GUEST_PORT, Desktop, GUEST_INIT, RuntimeError,
    bounded,
};
use std::time::Duration;
use uuid::Uuid;

impl ComputerManager {
    /// For a desktop computer: a free host loopback port for noVNC (saved
    /// with the computer) and a fresh VNC password in a private host file
    /// that is copied into the guest.
    pub(super) async fn desktop_resources(
        &self,
        bot_id: Uuid,
    ) -> Result<(Vec<(u16, u16)>, Vec<(std::path::PathBuf, String)>), RuntimeError> {
        if !self.config.desktop {
            return Ok((Vec::new(), Vec::new()));
        }
        let unavailable = |e: String| RuntimeError::Unavailable(e);
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .map_err(|e| unavailable(e.to_string()))?
            .port();
        sqlx::query("UPDATE bot_computers SET desktop_port = ?2 WHERE bot_id = ?1")
            .bind(bot_id)
            .bind(port as i32)
            .execute(&self.pool)
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        let password = self
            .write_vnc_password(bot_id)
            .map_err(|e| unavailable(e.to_string()))?;
        Ok((
            vec![(port, DESKTOP_GUEST_PORT)],
            vec![(password, "/etc/elsebot/vnc.pass".into())],
        ))
    }

    fn secret_dir(&self, bot_id: Uuid) -> std::path::PathBuf {
        self.config
            .state_dir
            .join("computers")
            .join(bot_id.to_string())
    }

    fn write_vnc_password(&self, bot_id: Uuid) -> std::io::Result<std::path::PathBuf> {
        use base64::Engine;
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let dir = self.secret_dir(bot_id);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut bytes = [0u8; 12];
        getrandom::fill(&mut bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
        // VNC uses at most 8 characters of a password.
        let password: String = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(bytes)
            .chars()
            .take(8)
            .collect();
        let path = dir.join("vnc.pass");
        let temp = dir.join(format!(".vnc.pass.{}", Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        writeln!(file, "{password}")?;
        file.sync_all()?;
        std::fs::rename(&temp, &path)?;
        Ok(path)
    }

    /// The host file holding a desktop computer's VNC password.
    pub fn desktop_password_file(&self, bot_id: Uuid) -> std::path::PathBuf {
        self.secret_dir(bot_id).join("vnc.pass")
    }

    /// Owner access to a desktop: hold the computer with a renewable
    /// `desktop:` lease (an open VNC session counts as use) and return its
    /// loopback address and VNC password. Calling again renews it.
    pub async fn desktop(&self, bot_id: Uuid) -> Result<Result<Desktop, Acquire>, ComputerError> {
        if !self.config.desktop {
            return Ok(Err(Acquire::Unavailable {
                reason: "this installation's computers have no desktop".into(),
            }));
        }
        let acquired = self.acquire(bot_id, &format!("desktop:{bot_id}")).await?;
        let Acquire::Ready { lease_id, .. } = acquired else {
            return Ok(Err(acquired));
        };
        let (port, ready): (Option<i32>, bool) = sqlx::query_as(
            "SELECT desktop_port, services_generation IS NOT DISTINCT FROM generation
             FROM bot_computers WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .fetch_one(&self.pool)
        .await?;
        if !ready {
            self.release(lease_id).await?;
            return Ok(Err(Acquire::Queued {
                reason: "the desktop services are not running yet; try again shortly".into(),
            }));
        }
        let password = std::fs::read_to_string(self.secret_dir(bot_id).join("vnc.pass"))
            .map(|p| p.trim().to_owned())
            .ok();
        match (port, password) {
            (Some(port), Some(password)) => Ok(Ok(Desktop {
                lease_id,
                url: format!("http://127.0.0.1:{port}/vnc.html?autoconnect=1&resize=scale"),
                password,
                lease_seconds: self.config.lease_ttl.as_secs(),
            })),
            _ => {
                self.release(lease_id).await?;
                Ok(Err(Acquire::Unavailable {
                    reason: "this computer was created without a desktop".into(),
                }))
            }
        }
    }

    /// End the owner's desktop session.
    pub async fn close_desktop(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let lease: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM computer_leases WHERE bot_id = ?1 AND holder = ?2 AND released_at IS NULL",
        )
        .bind(bot_id)
        .bind(format!("desktop:{bot_id}"))
        .fetch_optional(&self.pool)
        .await?;
        if let Some(lease) = lease {
            self.release(lease).await?;
        }
        Ok(())
    }

    /// Start the desktop services for the current boot unless already done
    /// for this generation. The caller holds the bot's operation lock. A
    /// failure is recorded and retried on the next use.
    pub(super) async fn ensure_services(
        &self,
        bot_id: Uuid,
        name: &str,
    ) -> Result<(), ComputerError> {
        if !self.config.desktop {
            return Ok(());
        }
        let (generation, done): (i64, Option<i64>) = sqlx::query_as(
            "SELECT generation, services_generation FROM bot_computers WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .fetch_one(&self.pool)
        .await?;
        if done == Some(generation) {
            return Ok(());
        }
        let result = self
            .runtime
            .exec(name, GUEST_INIT, &[], Duration::from_secs(60))
            .await;
        match result {
            Ok(out) if out.status == Some(0) => {
                sqlx::query(
                    "UPDATE bot_computers SET services_generation = ?2 WHERE bot_id = ?1 AND generation = ?2",
                )
                .bind(bot_id)
                .bind(generation)
                .execute(&self.pool)
                .await?;
            }
            other => {
                let error = match other {
                    Ok(out) => format!("guest services did not start: {}", out.stderr.trim()),
                    Err(err) => format!("guest services did not start: {err}"),
                };
                tracing::warn!(bot = %bot_id, %error, "desktop services");
                sqlx::query("UPDATE bot_computers SET last_error = ?2 WHERE bot_id = ?1")
                    .bind(bot_id)
                    .bind(bounded(&error, 2000))
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }
}
