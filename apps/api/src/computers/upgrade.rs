//! Explicit image upgrades of existing computers (POC-025).
//!
//! A computer keeps the image it was created with: ordinary startup and use
//! never replace a disk or reset a profile, so a new image reaches only new
//! bots until the owner upgrades an existing computer here. An upgrade runs
//! under the bot's operation lock while nothing holds the computer and no
//! guest job is active. It stops the computer through the normal stop (a
//! desktop checkpoints Chromium), boots the stopped disk without its desktop
//! services to archive the user's data (`/home/elsebot`, including projects,
//! package environments, caches, and the browser profile, and the job
//! records in `/var/lib/elsebot`) with numeric owners, verifies the copy on
//! the host by size and SHA-256, creates a new computer from the new image
//! with that archive copied in, verifies and unpacks it there as the same
//! user ID, and stops it. Only then does the bot switch to the new computer,
//! under its row lock. The old computer is kept, stopped, so
//! [`ComputerManager::rollback_upgrade`] can switch back; it is never
//! removed here. A failure before the switch removes only the replacement
//! this upgrade created and leaves the bot on its old computer.

use std::path::{Path, PathBuf};
use std::time::Duration;

use uuid::Uuid;

use super::runtime::valid_image;
use super::{ComputerError, ComputerManager, DESKTOP_GUEST_PORT, RuntimeState, Spec};

/// Archiving or unpacking a large home directory.
const COPY_TIMEOUT: Duration = Duration::from_secs(1800);
/// Where the archive is copied into the new computer before it boots.
const GUEST_ARCHIVE: &str = "/var/lib/elsebot-upgrade/data.tar";

/// Archive the user's data on the old computer: print the archive size,
/// SHA-256, and the user's numeric ID.
const PACK: &str = r#"set -e
cd /
paths=""
for p in home/elsebot var/lib/elsebot; do [ -e "$p" ] && paths="$paths $p"; done
[ -n "$paths" ] || { echo "no user data to copy" >&2; exit 3; }
rm -f /var/tmp/else-upgrade.tar
tar --numeric-owner -cpf /var/tmp/else-upgrade.tar $paths
stat -c %s /var/tmp/else-upgrade.tar
sha256sum /var/tmp/else-upgrade.tar | cut -d' ' -f1
id -u elsebot 2>/dev/null || echo none"#;

/// Verify and unpack the archive on the new computer as the same user ID,
/// then print the new image's description.
const UNPACK: &str = r#"set -e
cd /
[ "$(id -u elsebot 2>/dev/null || echo none)" = "$3" ] || { echo "the new image's elsebot user ID is not $3" >&2; exit 4; }
echo "$1  $2" | sha256sum -c --quiet
tar --numeric-owner -xpf "$2"
rm -f "$2"
rmdir "$(dirname "$2")" 2>/dev/null || true
sync
cat /etc/elsebot/image.json 2>/dev/null || echo '{}'"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpgradeReport {
    pub id: Uuid,
    pub from_runtime_name: String,
    pub from_image: String,
    pub to_runtime_name: String,
    pub to_image: String,
    pub data_bytes: u64,
    pub data_sha256: String,
    /// The new image's `/etc/elsebot/image.json`, when it has one.
    pub image_description: String,
}

fn refused(message: impl Into<String>) -> ComputerError {
    ComputerError::Refused(message.into())
}

fn sha256_file(path: &Path) -> std::io::Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&chunk[..n]);
    }
    Ok((size, hex::encode(hasher.finalize())))
}

impl ComputerManager {
    async fn active_jobs(&self, bot_id: Uuid) -> Result<i64, ComputerError> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM guest_jobs
             WHERE bot_id = $1 AND status IN ('starting', 'running', 'cancelling')",
        )
        .bind(bot_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Remove replacements left by an upgrade whose process died: they were
    /// created by else.bot and never handed to the bot.
    async fn clear_interrupted_upgrades(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let started: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT id, to_runtime_name FROM computer_upgrades
             WHERE bot_id = $1 AND state = 'started'",
        )
        .bind(bot_id)
        .fetch_all(&self.pool)
        .await?;
        for (id, name) in started {
            self.discard_replacement(&name).await?;
            sqlx::query(
                "UPDATE computer_upgrades SET state = 'failed', finished_at = now(),
                     error = coalesce(error, 'interrupted')
                 WHERE id = $1",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    async fn discard_replacement(&self, name: &str) -> Result<(), ComputerError> {
        match self.runtime.state(name).await? {
            None => return Ok(()),
            Some(RuntimeState::Stopped) => {}
            Some(_) => self.runtime.stop(name).await?,
        }
        self.runtime.remove(name).await?;
        Ok(())
    }

    /// Stop a computer the upgrade booted, confirming it is stopped.
    async fn stop_confirmed(&self, name: &str) -> Result<(), ComputerError> {
        if self.runtime.state(name).await? != Some(RuntimeState::Stopped) {
            self.runtime.stop(name).await?;
        }
        match self.runtime.state(name).await? {
            Some(RuntimeState::Stopped) => Ok(()),
            other => Err(ComputerError::Runtime(super::RuntimeError::Failed {
                operation: "stop".into(),
                message: format!("{name} is {other:?}, not stopped"),
            })),
        }
    }

    /// Move a bot's computer to `image`, keeping its data. See the module
    /// documentation for the steps and guarantees.
    pub async fn upgrade_image(
        &self,
        bot_id: Uuid,
        image: &str,
    ) -> Result<UpgradeReport, ComputerError> {
        if !valid_image(image) {
            return Err(refused("the image reference is not valid"));
        }
        let _op = self.op(bot_id).await;
        let active: Option<bool> =
            sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = $1")
                .bind(bot_id)
                .fetch_optional(&self.pool)
                .await?;
        if active != Some(true) {
            return Err(refused("the bot is not active"));
        }
        let row = {
            let mut conn = self.pool.acquire().await?;
            Self::row(&mut conn, bot_id).await?
        };
        let Some(row) = row else {
            return Err(refused(
                "the bot has no computer yet; it is created from the configured image on first use",
            ));
        };
        if row.image == image {
            return Err(refused(format!("the computer already uses {image}")));
        }
        if self.active_jobs(bot_id).await? > 0 {
            return Err(refused(
                "a guest job is active; wait for it or cancel it first",
            ));
        }
        self.clear_interrupted_upgrades(bot_id).await?;
        self.stop_for_disk(bot_id, &row).await?;

        let id = Uuid::new_v4();
        let to_runtime_name = format!("elsebot-{bot_id}-{}", &id.simple().to_string()[..12]);
        sqlx::query(
            "INSERT INTO computer_upgrades
                 (id, bot_id, from_runtime_name, from_image, to_runtime_name, to_image, state)
             VALUES ($1, $2, $3, $4, $5, $6, 'started')",
        )
        .bind(id)
        .bind(bot_id)
        .bind(&row.runtime_name)
        .bind(&row.image)
        .bind(&to_runtime_name)
        .bind(image)
        .execute(&self.pool)
        .await?;

        let staging = self
            .config
            .state_dir
            .join("upgrades")
            .join(bot_id.to_string());
        let copied = self
            .copy_to_replacement(bot_id, &row, &to_runtime_name, image, &staging)
            .await;
        let _ = std::fs::remove_dir_all(&staging);
        let (bytes, sha256, description) = match copied {
            Ok(done) => done,
            Err(err) => {
                // The old computer stays the bot's; it was booted only to
                // read its disk.
                let _ = self.stop_confirmed(&row.runtime_name).await;
                let discarded = self.discard_replacement(&to_runtime_name).await;
                sqlx::query(
                    "UPDATE computer_upgrades SET state = $2, finished_at = now(), error = $3
                     WHERE id = $1",
                )
                .bind(id)
                .bind(if discarded.is_ok() {
                    "failed"
                } else {
                    "started"
                })
                .bind(super::bounded(&err.to_string(), 2000))
                .execute(&self.pool)
                .await?;
                return Err(err);
            }
        };

        // Switch under the row lock, only if nothing changed meanwhile.
        let mut tx = self.pool.begin().await?;
        let current = Self::row(&mut tx, bot_id).await?.expect("exists");
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM computer_leases WHERE bot_id = $1 AND released_at IS NULL)",
        )
        .bind(bot_id)
        .fetch_one(&mut *tx)
        .await?;
        if current.runtime_name != row.runtime_name || current.observed_state != "stopped" || held {
            drop(tx);
            let _ = self.discard_replacement(&to_runtime_name).await;
            sqlx::query(
                "UPDATE computer_upgrades SET state = 'failed', finished_at = now(),
                     error = 'the computer changed during the upgrade'
                 WHERE id = $1",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
            return Err(ComputerError::Busy(
                "the computer changed during the upgrade".into(),
            ));
        }
        sqlx::query(
            "UPDATE bot_computers SET runtime_name = $2, image = $3, observed_state = 'stopped',
                 generation = generation + 1, services_generation = NULL, updated_at = now()
             WHERE bot_id = $1",
        )
        .bind(bot_id)
        .bind(&to_runtime_name)
        .bind(image)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE computer_upgrades SET state = 'completed', finished_at = now(),
                 data_bytes = $2, data_sha256 = $3
             WHERE id = $1",
        )
        .bind(id)
        .bind(bytes as i64)
        .bind(&sha256)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!(bot = %bot_id, from = %row.image, to = %image, "computer upgraded");
        Ok(UpgradeReport {
            id,
            from_runtime_name: row.runtime_name,
            from_image: row.image,
            to_runtime_name,
            to_image: image.to_owned(),
            data_bytes: bytes,
            data_sha256: sha256,
            image_description: description,
        })
    }

    async fn copy_to_replacement(
        &self,
        bot_id: Uuid,
        row: &super::Row,
        to_runtime_name: &str,
        image: &str,
        staging: &Path,
    ) -> Result<(u64, String, String), ComputerError> {
        use std::os::unix::fs::DirBuilderExt;
        let io = |e: std::io::Error| refused(format!("host staging: {e}"));
        let _ = std::fs::remove_dir_all(staging);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(staging)
            .map_err(io)?;

        // Read the old disk: booted without its desktop services.
        self.runtime.start(&row.runtime_name).await?;
        let out = self
            .runtime
            .exec(&row.runtime_name, "sh", &["-c", PACK], COPY_TIMEOUT)
            .await?;
        if out.status != Some(0) {
            return Err(refused(format!(
                "archiving the old computer's data failed: {}",
                super::bounded(out.stderr.trim(), 500)
            )));
        }
        let lines: Vec<&str> = out.stdout.lines().map(str::trim).collect();
        let (Some(size), Some(sha256), Some(uid)) = (
            lines.first().and_then(|s| s.parse::<u64>().ok()),
            lines
                .get(1)
                .filter(|s| s.len() == 64)
                .map(|s| (*s).to_owned()),
            lines.get(2).map(|s| (*s).to_owned()),
        ) else {
            return Err(refused("the old computer's archive report was unreadable"));
        };
        let archive: PathBuf = staging.join("data.tar");
        self.runtime
            .copy_out(&row.runtime_name, "/var/tmp/else-upgrade.tar", &archive)
            .await?;
        let _ = self
            .runtime
            .exec(
                &row.runtime_name,
                "rm",
                &["-f", "/var/tmp/else-upgrade.tar"],
                Duration::from_secs(60),
            )
            .await;
        self.stop_confirmed(&row.runtime_name).await?;
        let (host_size, host_sha) = sha256_file(&archive).map_err(io)?;
        if (host_size, host_sha.as_str()) != (size, sha256.as_str()) {
            return Err(refused(
                "the copied archive does not match the old computer's (size or SHA-256)",
            ));
        }

        // The replacement: same resources, network, and desktop port and
        // password, booted with the archive in place.
        let mut ports = Vec::new();
        let mut files = vec![(archive.clone(), GUEST_ARCHIVE.to_owned())];
        if self.config.desktop {
            let port: Option<i32> =
                sqlx::query_scalar("SELECT desktop_port FROM bot_computers WHERE bot_id = $1")
                    .bind(bot_id)
                    .fetch_one(&self.pool)
                    .await?;
            if let Some(port) = port {
                ports.push((port as u16, DESKTOP_GUEST_PORT));
            }
            let password = self.desktop_password_file(bot_id);
            if password.is_file() {
                files.push((password, "/etc/elsebot/vnc.pass".into()));
            }
        }
        self.runtime
            .create(&Spec {
                name: to_runtime_name.to_owned(),
                image: image.to_owned(),
                cpus: row.cpus as u32,
                memory_mib: row.memory_mib as u32,
                network: self.config.network.clone(),
                ports,
                files,
            })
            .await?;
        let out = self
            .runtime
            .exec(
                to_runtime_name,
                "sh",
                &["-c", UNPACK, "sh", &sha256, GUEST_ARCHIVE, &uid],
                COPY_TIMEOUT,
            )
            .await?;
        if out.status != Some(0) {
            return Err(refused(format!(
                "unpacking into the new computer failed: {}",
                super::bounded(out.stderr.trim(), 500)
            )));
        }
        let description = out.stdout.trim().to_owned();
        self.stop_confirmed(to_runtime_name).await?;
        Ok((size, sha256, super::bounded(&description, 4000)))
    }

    /// Switch a bot back to the computer its last completed upgrade
    /// replaced. The newer computer is kept, stopped; nothing is removed.
    pub async fn rollback_upgrade(&self, bot_id: Uuid) -> Result<UpgradeReport, ComputerError> {
        let _op = self.op(bot_id).await;
        let row = {
            let mut conn = self.pool.acquire().await?;
            Self::row(&mut conn, bot_id).await?
        };
        let Some(row) = row else {
            return Err(refused("the bot has no computer"));
        };
        let last: Option<(Uuid, String, String, Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT id, from_runtime_name, from_image, data_bytes, data_sha256
             FROM computer_upgrades
             WHERE bot_id = $1 AND state = 'completed' AND to_runtime_name = $2
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(bot_id)
        .bind(&row.runtime_name)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, from_name, from_image, bytes, sha256)) = last else {
            return Err(refused(
                "the current computer is not the result of an upgrade",
            ));
        };
        if self.active_jobs(bot_id).await? > 0 {
            return Err(refused(
                "a guest job is active; wait for it or cancel it first",
            ));
        }
        if self.runtime.state(&from_name).await? != Some(RuntimeState::Stopped) {
            return Err(refused(format!(
                "the previous computer {from_name} is not present and stopped"
            )));
        }
        self.stop_for_disk(bot_id, &row).await?;
        let mut tx = self.pool.begin().await?;
        let current = Self::row(&mut tx, bot_id).await?.expect("exists");
        if current.runtime_name != row.runtime_name || current.observed_state != "stopped" {
            return Err(ComputerError::Busy(
                "the computer changed during the rollback".into(),
            ));
        }
        sqlx::query(
            "UPDATE bot_computers SET runtime_name = $2, image = $3, observed_state = 'stopped',
                 generation = generation + 1, services_generation = NULL, updated_at = now()
             WHERE bot_id = $1",
        )
        .bind(bot_id)
        .bind(&from_name)
        .bind(&from_image)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE computer_upgrades SET state = 'rolled_back', finished_at = now() WHERE id = $1",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(UpgradeReport {
            id,
            from_runtime_name: row.runtime_name,
            from_image: row.image,
            to_runtime_name: from_name,
            to_image: from_image,
            data_bytes: bytes.unwrap_or(0) as u64,
            data_sha256: sha256.unwrap_or_default(),
            image_description: String::new(),
        })
    }
}
