use super::{
    Acquire, ComputerError, ComputerManager, DESKTOP_GUEST_PORT, Desktop, GUEST_INIT, RuntimeError,
    bounded,
};
use std::time::Duration;
use uuid::Uuid;

impl ComputerManager {
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

/// Where the API's own viewer reaches a desktop. It stays inside the API:
/// the owner's client gets frames, never this address or password.
pub struct ViewerTarget {
    pub lease_id: Uuid,
    pub port: u16,
    pub password: String,
}

impl std::fmt::Debug for ViewerTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerTarget")
            .field("lease_id", &self.lease_id)
            .finish_non_exhaustive()
    }
}

/// Why a viewer cannot attach now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewerRefusal {
    /// Not running, and the viewer did not ask to wake it.
    NotRunning,
    OwnerStopped,
    BotInactive,
    /// This installation's computers have no desktop, or this one was
    /// created without one.
    NoDesktop,
    /// Running, but its desktop services have not started yet.
    Starting,
    Unavailable(String),
}

/// A viewer lease as checked while it streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerLease {
    Live,
    OwnerStopped,
    BotInactive,
    /// Stopped, paused, or rebooted since the lease was granted.
    NotRunning,
    Ended,
}

impl ComputerManager {
    /// Hold a desktop for the API's viewer under `holder`. With `wake`, this
    /// is an ordinary use that may create, start, or resume the computer
    /// (never after an owner stop). Without it, only a running computer is
    /// attached, and the bot's operation lock is not taken: the immediate
    /// transaction alone orders the new lease against stop decisions, which
    /// revoke leases or leave `running` in their own immediate transaction.
    pub async fn viewer(
        &self,
        bot_id: Uuid,
        holder: &str,
        wake: bool,
    ) -> Result<Result<ViewerTarget, ViewerRefusal>, ComputerError> {
        if !self.config.desktop {
            return Ok(Err(ViewerRefusal::NoDesktop));
        }
        let lease_id = if wake {
            match self.acquire(bot_id, holder).await? {
                Acquire::Ready { lease_id, .. } => lease_id,
                Acquire::OwnerStopped => return Ok(Err(ViewerRefusal::OwnerStopped)),
                Acquire::BotInactive => return Ok(Err(ViewerRefusal::BotInactive)),
                Acquire::Queued { reason } | Acquire::Unavailable { reason } => {
                    return Ok(Err(ViewerRefusal::Unavailable(reason)));
                }
            }
        } else {
            match self.attach(bot_id, holder).await? {
                Ok(lease_id) => lease_id,
                Err(refusal) => return Ok(Err(refusal)),
            }
        };
        let ready_query = "SELECT desktop_port, services_generation IS NOT DISTINCT FROM generation
             FROM bot_computers WHERE bot_id = ?1";
        let (mut port, mut ready): (Option<i32>, bool) = sqlx::query_as(ready_query)
            .bind(bot_id)
            .fetch_one(&self.pool)
            .await?;
        if !ready && port.is_some() {
            self.ensure_viewer_services(bot_id, lease_id).await?;
            (port, ready) = sqlx::query_as(ready_query)
                .bind(bot_id)
                .fetch_one(&self.pool)
                .await?;
        }
        let password = std::fs::read_to_string(self.desktop_password_file(bot_id))
            .map(|p| p.trim().to_owned())
            .ok();
        let refusal = match (port.and_then(|p| u16::try_from(p).ok()), password) {
            (Some(_), Some(_)) if !ready => ViewerRefusal::Starting,
            (Some(port), Some(password)) => {
                return Ok(Ok(ViewerTarget {
                    lease_id,
                    port,
                    password,
                }));
            }
            _ => ViewerRefusal::NoDesktop,
        };
        self.release(lease_id).await?;
        Ok(Err(refusal))
    }

    async fn ensure_viewer_services(
        &self,
        bot_id: Uuid,
        lease_id: Uuid,
    ) -> Result<(), ComputerError> {
        let _op = self.op(bot_id).await;
        let same_boot: Option<String> = sqlx::query_scalar(
            "SELECT c.runtime_name FROM computer_leases l JOIN bot_computers c ON c.bot_id = l.bot_id
             WHERE l.id = ?1 AND l.released_at IS NULL AND c.observed_state = 'running'
               AND c.desired_state = 'auto' AND c.generation = l.generation",
        )
        .bind(lease_id)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(name) = same_boot {
            self.ensure_services(bot_id, &name).await?;
        }
        Ok(())
    }

    async fn attach(
        &self,
        bot_id: Uuid,
        holder: &str,
    ) -> Result<Result<Uuid, ViewerRefusal>, ComputerError> {
        if !self.inspected.load(super::Ordering::SeqCst) {
            return Ok(Err(ViewerRefusal::Unavailable(
                "the computer runtime has not been inspected yet".into(),
            )));
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let active: Option<bool> =
            sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = ?1")
                .bind(bot_id)
                .fetch_optional(&mut *tx)
                .await?;
        if active != Some(true) {
            return Ok(Err(ViewerRefusal::BotInactive));
        }
        let Some(row) = Self::row(&mut tx, bot_id).await? else {
            return Ok(Err(ViewerRefusal::NotRunning));
        };
        if row.desired_state == "stopped" {
            return Ok(Err(ViewerRefusal::OwnerStopped));
        }
        if row.observed_state != "running" {
            return Ok(Err(ViewerRefusal::NotRunning));
        }
        let lease = self.lease(&mut tx, bot_id, holder, row.generation).await?;
        tx.commit().await?;
        Ok(Ok(lease))
    }

    /// Whether a viewer's lease still holds a running computer of an
    /// active bot; renews it when `renew` is set.
    pub async fn viewer_lease(
        &self,
        lease_id: Uuid,
        renew: bool,
    ) -> Result<ViewerLease, ComputerError> {
        let row: Option<(bool, Option<String>, bool, String, bool)> = sqlx::query_as(
            "SELECT l.released_at IS NULL, l.end_reason, b.status = 'active', c.desired_state,
                    c.observed_state = 'running' AND c.generation = l.generation
             FROM computer_leases l JOIN bots b ON b.id = l.bot_id
             JOIN bot_computers c ON c.bot_id = l.bot_id WHERE l.id = ?1",
        )
        .bind(lease_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some((live, reason, active, desired, running)) = row else {
            return Ok(ViewerLease::Ended);
        };
        let state = if !active {
            ViewerLease::BotInactive
        } else if desired == "stopped" || reason.as_deref() == Some("owner_stop") {
            ViewerLease::OwnerStopped
        } else if !running {
            ViewerLease::NotRunning
        } else if !live {
            ViewerLease::Ended
        } else {
            ViewerLease::Live
        };
        if state == ViewerLease::Live && renew && !self.renew(lease_id).await? {
            return Ok(ViewerLease::Ended);
        }
        Ok(state)
    }

    /// The lease a controlling stream holds while it controls (no wake).
    pub async fn control_lease(
        &self,
        bot_id: Uuid,
        holder: &str,
    ) -> Result<Result<Uuid, ViewerRefusal>, ComputerError> {
        self.attach(bot_id, holder).await
    }

    pub(crate) async fn owner_controls(
        &self,
        bot_id: Uuid,
        novnc: bool,
    ) -> Result<bool, ComputerError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM computer_leases WHERE bot_id = ?1
                            AND (holder LIKE ?2 OR (?3 AND holder = ?4))
                            AND released_at IS NULL
                            AND expires_at > strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))",
        )
        .bind(bot_id)
        .bind(format!("desktop-control:{bot_id}:%"))
        .bind(novnc)
        .bind(format!("desktop:{bot_id}"))
        .fetch_one(&self.pool)
        .await?)
    }

    /// Make `stream` the one controller of a bot's desktop; false if another
    /// stream controls it.
    pub async fn claim_control(&self, bot_id: Uuid, stream: Uuid) -> bool {
        self.controllers
            .call(move |controllers| *controllers.entry(bot_id).or_insert(stream) == stream)
            .await
    }

    pub async fn end_control(&self, bot_id: Uuid, stream: Uuid) {
        self.controllers
            .call(move |controllers| {
                if controllers.get(&bot_id) == Some(&stream) {
                    controllers.remove(&bot_id);
                }
            })
            .await
    }
}
