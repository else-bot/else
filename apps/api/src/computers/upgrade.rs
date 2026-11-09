//! Explicit image upgrades of existing computers.
//!
//! A computer keeps the image it was created with: ordinary startup and use
//! never replace a disk or reset a profile, so a new image reaches only new
//! bots until the owner upgrades an existing computer here. An upgrade runs
//! under the bot's operation lock while nothing holds the computer and no
//! guest job is active. A running desktop first closes Chromium with SIGINT
//! ([`CLOSE_BROWSER`]); the computer then stops through the normal stop (a
//! desktop checkpoints Chromium), and the upgrade boots the stopped disk
//! without its desktop services to archive the user's data
//! (`/home/elsebot`, including projects,
//! package environments, caches, and the browser profile, and the job
//! records in `/var/lib/elsebot`) with numeric owners, verifies the copy on
//! the host by size and SHA-256, creates a new computer from the new image
//! with that archive copied in, verifies and unpacks it there as the same
//! user ID, and stops it. Only then does the bot switch to the new computer,
//! inside an immediate transaction. The old computer is kept, stopped, so
//! [`ComputerManager::rollback_upgrade`] can switch back; it is never
//! removed here. A failure before the switch removes only the replacement
//! this upgrade created and leaves the bot on its old computer.
//!
//! Bots have root in their computer, so the archive also carries
//! `/usr/local` and `/opt`, where the new image's own files win, and the
//! old computer reports the APT packages marked manual beyond the image's
//! own list (`manual` in `/etc/elsebot/image.json`; an image without it
//! reports none). The new computer installs them again by name, so it gets
//! the versions its archive has, and reports each one it could not install
//! instead of failing. Other system changes, such as edits in `/etc`, stay
//! behind.
//!
//! The owner starts one from the desktop ([`ComputerManager::start_upgrade`])
//! or offline with `else-api upgrade-computer`. One started from the
//! desktop holds a maintenance admission until it ends, so a backup or an
//! update waits for it; it ends the owner's desktop sessions first, and
//! while it runs, new leases and viewers are refused. A started upgrade's
//! computers count against the budget though the bot's row says stopped.
//! Maintenance removes the replacement of an upgrade that a restart
//! interrupted or whose cleanup failed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use super::runtime::valid_image;
use super::{ComputerError, ComputerManager, DESKTOP_GUEST_PORT, RuntimeState, Spec};

const COPY_TIMEOUT: Duration = Duration::from_secs(1800);
const GUEST_STAGING: &str = "/var/tmp/else-upgrade.tar";
const GUEST_ARCHIVE: &str = "/var/lib/elsebot-upgrade/data.tar";

const PACK: &str = r#"set -e
PATH=/usr/sbin:/usr/bin:/sbin:/bin
cd /
archive=/var/tmp/else-upgrade.tar
trap 'rm -f "$archive"' EXIT
paths=""
for p in home/elsebot var/lib/elsebot; do [ -e "$p" ] && paths="$paths $p"; done
[ -n "$paths" ] || { echo "no user data to copy" >&2; exit 3; }
for p in usr/local opt; do [ -d "$p" ] && paths="$paths $p"; done
rm -f "$archive"
need=$(du -skx $paths | awk '{s += $1} END {print s + 65536}')
free=$(df -Pk /var/tmp | awk 'NR == 2 {print $4}')
[ "$free" -ge "$need" ] || { echo "not enough free disk for the archive: ${need} KiB needed, ${free} KiB free" >&2; exit 5; }
tar --numeric-owner --sparse --one-file-system -cpf "$archive" $paths
stat -c %s "$archive"
sha256sum "$archive" | cut -d' ' -f1
id -u elsebot 2>/dev/null || echo none
if command -v jq >/dev/null && jq -e '.manual' /etc/elsebot/image.json >/dev/null 2>&1; then
    jq -r '.manual[]' /etc/elsebot/image.json | sort -u > /var/tmp/else-image-packages
    apt-mark showmanual | sort -u | comm -23 - /var/tmp/else-image-packages
    rm -f /var/tmp/else-image-packages
fi
trap - EXIT"#;

/// Close a running Chromium gracefully before the normal stop. SIGINT makes
/// it commit its cookie store; images before `else-guest:3` stop it with
/// SIGTERM, which drops cookies set in the last 30 seconds. Supervisor may
/// restart Chromium on the closed profile; the checkpoint that follows stops
/// that instance, and the cookies already on disk stay. It fails if Chromium
/// is still running 10 seconds after the signal.
const CLOSE_BROWSER: &str = r#"set -e
PATH=/usr/sbin:/usr/bin:/sbin:/bin
[ -S /run/elsebot/supervisor.sock ] || exit 0
pid=$(supervisorctl -c /etc/elsebot/supervisord.conf pid chromium 2>/dev/null || true)
case "$pid" in ''|0|*[!0-9]*) exit 0 ;; esac
kill -INT "$pid" 2>/dev/null || exit 0
i=0
while kill -0 "$pid" 2>/dev/null; do
    i=$((i + 1))
    [ "$i" -le 100 ] || { echo "Chromium did not exit within 10 seconds of SIGINT" >&2; exit 1; }
    sleep 0.1
done"#;

const UNPACK: &str = r#"set -e
PATH=/usr/sbin:/usr/bin:/sbin:/bin
cd /
trap 'rm -f "$2"' EXIT
[ "$(id -u elsebot 2>/dev/null || echo none)" = "$3" ] || { echo "the new image's elsebot user ID is not $3" >&2; exit 4; }
need=$(( $(stat -c %s "$2") / 1024 + 65536 ))
free=$(df -Pk / | awk 'NR == 2 {print $4}')
[ "$free" -ge "$need" ] || { echo "not enough free disk to unpack: ${need} KiB needed, ${free} KiB free" >&2; exit 5; }
echo "$1  $2" | sha256sum -c --quiet
tar --numeric-owner -xpf "$2" --anchored --exclude=usr/local --exclude=opt
tar --numeric-owner --skip-old-files -xpf "$2" --anchored --exclude=home/elsebot --exclude=var/lib/elsebot
rm -f "$2"
rmdir "$(dirname "$2")" 2>/dev/null || true
trap - EXIT
shift 3
present=""
missing=""
for p; do
    if [ "$(dpkg-query -W -f='${db:Status-Status}' "$p" 2>/dev/null)" = installed ]; then
        present="$present $p"
    else
        missing="$missing $p"
    fi
done
[ -z "$present" ] || apt-mark manual $present >/dev/null 2>&1 || true
if [ -n "$missing" ]; then
    mkdir -p /var/log/elsebot
    log=/var/log/elsebot/upgrade-packages.log
    export DEBIAN_FRONTEND=noninteractive
    if ! { apt-get update && apt-get install -y $missing; } >>"$log" 2>&1 </dev/null; then
        for p in $missing; do
            apt-get install -y "$p" >>"$log" 2>&1 </dev/null || echo "unrestored $p"
        done
    fi
fi
sync
cat /etc/elsebot/image.json 2>/dev/null || echo '{}'"#;

/// An upgrade the owner started from the desktop. Kept in memory: a restart
/// forgets it, and an interrupted upgrade is cleaned up by the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradeStatus {
    Running,
    Failed(String),
}

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
    /// APT packages the bot installed that the new computer could not
    /// install again.
    pub unrestored_packages: Vec<String>,
}

struct Copied {
    bytes: u64,
    sha256: String,
    description: String,
    unrestored: Vec<String>,
}

/// A Debian package name as `apt-mark showmanual` prints it, optionally
/// with an `:arch` qualifier. The first character keeps `apt-get` from
/// reading it as an option.
fn package_name(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.:".contains(c))
}

type UpgradeRow = (
    Uuid,
    String,
    String,
    Option<i64>,
    Option<String>,
    Option<String>,
);

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
    pub async fn upgrade_status(&self, bot_id: Uuid) -> Option<UpgradeStatus> {
        self.upgrades
            .call(move |upgrades| upgrades.get(&bot_id).cloned())
            .await
    }

    pub(super) async fn upgrading(&self, bot_id: Uuid) -> bool {
        self.upgrade_status(bot_id).await == Some(UpgradeStatus::Running)
    }

    /// Start moving a bot's computer to the configured image in the
    /// background; [`Self::upgrade_status`] reports its progress. Refused
    /// when the computer already uses that image or an upgrade is running.
    pub async fn start_upgrade(
        self: &Arc<Self>,
        bot_id: Uuid,
        admission: crate::maintenance::Admission,
    ) -> Result<(), ComputerError> {
        if !self.inspected.load(super::Ordering::SeqCst) {
            return Err(ComputerError::Busy(
                "the computer runtime has not been inspected yet".into(),
            ));
        }
        if self.upgrade_stop.is_cancelled() {
            return Err(ComputerError::Busy("the API is stopping".into()));
        }
        let current: Option<String> =
            sqlx::query_scalar("SELECT image FROM bot_computers WHERE bot_id = ?1")
                .bind(bot_id)
                .fetch_optional(&self.pool)
                .await?;
        let image = self.config.image.clone();
        match current {
            None => return Err(refused("the bot has no computer yet")),
            Some(current) if current == image => {
                return Err(refused("the computer already uses the latest image"));
            }
            Some(_) => {}
        }
        let claimed = self
            .upgrades
            .call(move |upgrades| {
                if upgrades.get(&bot_id) == Some(&UpgradeStatus::Running) {
                    return false;
                }
                upgrades.insert(bot_id, UpgradeStatus::Running);
                true
            })
            .await;
        if !claimed {
            return Err(refused("an upgrade is already running"));
        }
        let this = self.clone();
        self.upgrade_tasks.spawn(async move {
            let _admission = admission;
            let upgrade = async {
                this.end_desktop_sessions(bot_id).await?;
                this.upgrade_image(bot_id, &image, None).await
            };
            let upgraded = tokio::select! {
                upgraded = upgrade => upgraded,
                () = this.upgrade_stop.cancelled() => Err(refused("the API stopped during the upgrade")),
            };
            let failed = match upgraded {
                Ok(_) => None,
                Err(ComputerError::Busy(reason)) if reason == "in use" => {
                    Some("The bot is using its computer. Try again once it is idle.".to_owned())
                }
                Err(err) => {
                    tracing::warn!(bot = %bot_id, error = %err, "computer upgrade failed");
                    Some(err.to_string())
                }
            };
            this.upgrades
                .call(move |upgrades| match failed {
                    None => {
                        upgrades.remove(&bot_id);
                    }
                    Some(error) => {
                        upgrades.insert(bot_id, UpgradeStatus::Failed(error));
                    }
                })
                .await;
        });
        Ok(())
    }

    /// Interrupt running upgrades and wait until they have ended; shutdown
    /// calls it before releasing the API's singleton lock. An interrupted
    /// upgrade has not switched the bot (the switch is one transaction), and
    /// maintenance removes its replacement.
    pub async fn stop_upgrades(&self) {
        self.upgrade_stop.cancel();
        self.upgrade_tasks.close();
        self.upgrade_tasks.wait().await;
    }

    /// Remove the replacements of upgrades a restart interrupted, or whose
    /// removal failed when they did, so their rows stop reserving budget.
    /// Holding the bot's operation lock means no upgrade of it is running.
    pub(super) async fn clear_stranded_upgrades(&self) -> Result<(), ComputerError> {
        let bots: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT bot_id FROM computer_upgrades WHERE state = 'started'",
        )
        .fetch_all(&self.pool)
        .await?;
        for bot_id in bots {
            let Some(_op) = self.try_op(bot_id).await else {
                continue;
            };
            if let Err(err) = self.clear_interrupted_upgrades(bot_id).await {
                tracing::warn!(bot = %bot_id, error = %err, "removing a failed upgrade's replacement failed");
            }
        }
        Ok(())
    }

    /// End the owner's desktop sessions so the upgrade can stop the
    /// computer. A viewer that passed the upgrade check just before still
    /// holds its lease and makes the upgrade fail as in use.
    async fn end_desktop_sessions(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        sqlx::query(
            "UPDATE computer_leases SET released_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), end_reason = 'upgrade'
             WHERE bot_id = ?1 AND released_at IS NULL AND holder LIKE 'desktop%'",
        )
        .bind(bot_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn active_jobs(&self, bot_id: Uuid) -> Result<i64, ComputerError> {
        Ok(sqlx::query_scalar(
            "SELECT count(*) FROM guest_jobs
             WHERE bot_id = ?1 AND status IN ('starting', 'running', 'cancelling')",
        )
        .bind(bot_id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// For the offline upgrade command (the caller holds the API's
    /// singleton lock, so no turn, desktop, or tool call can be live): end
    /// this bot's leases other than background jobs', and ask the guest
    /// about each running job so a job that finished while the API was
    /// down no longer blocks the upgrade. A job still running keeps its
    /// lease, and the upgrade is refused.
    pub async fn settle_offline(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        sqlx::query(
            "UPDATE computer_leases SET released_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), end_reason = 'expired'
             WHERE bot_id = ?1 AND released_at IS NULL AND holder NOT LIKE 'job:%'",
        )
        .bind(bot_id)
        .execute(&self.pool)
        .await?;
        let running: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM guest_jobs WHERE bot_id = ?1 AND status = 'running'",
        )
        .bind(bot_id)
        .fetch_all(&self.pool)
        .await?;
        for job in running {
            self.job(bot_id, job).await?;
        }
        Ok(())
    }

    async fn clear_interrupted_upgrades(&self, bot_id: Uuid) -> Result<(), ComputerError> {
        let started: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT id, to_runtime_name FROM computer_upgrades
             WHERE bot_id = ?1 AND state = 'started'",
        )
        .bind(bot_id)
        .fetch_all(&self.pool)
        .await?;
        for (id, name) in started {
            self.discard_replacement(&name).await?;
            sqlx::query(
                "UPDATE computer_upgrades SET state = 'failed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                     error = coalesce(error, 'interrupted')
                 WHERE id = ?1",
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

    async fn admit(&self, bot_id: Uuid, row: &super::Row) -> Result<(), ComputerError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (cpus, memory) = row.reserved();
        let admitted = self.admits(&mut tx, bot_id, cpus, memory).await?;
        tx.commit().await?;
        if admitted {
            Ok(())
        } else {
            Err(ComputerError::Busy(
                "the computer budget is in use; stop another computer first".into(),
            ))
        }
    }

    async fn remove_guest_archive(&self, name: &str) {
        if matches!(
            self.runtime.state(name).await,
            Ok(Some(RuntimeState::Running))
        ) {
            let _ = self
                .runtime
                .exec(
                    name,
                    "/bin/rm",
                    &["-f", GUEST_STAGING],
                    Duration::from_secs(60),
                )
                .await;
        }
    }

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

    async fn close_browser(
        &self,
        bot_id: Uuid,
        mut row: super::Row,
    ) -> Result<super::Row, ComputerError> {
        if !self.config.desktop || !matches!(row.observed_state.as_str(), "running" | "paused") {
            return Ok(row);
        }
        let holders: Vec<String> = sqlx::query_scalar(
            "SELECT holder FROM computer_leases WHERE bot_id = ?1 AND released_at IS NULL",
        )
        .bind(bot_id)
        .fetch_all(&self.pool)
        .await?;
        for holder in holders {
            let finished = matches!(
                holder.split_once(':'),
                Some(("turn" | "job", id)) if id.parse::<Uuid>().is_ok()
            ) && !self.holder_is_live(&holder).await?;
            if !finished {
                return Err(ComputerError::Busy("in use".into()));
            }
        }
        if row.observed_state == "paused" {
            self.runtime.resume(&row.runtime_name).await?;
            let mut conn = self.pool.acquire().await?;
            self.set_state(&mut conn, bot_id, "running", None, None)
                .await?;
            row.observed_state = "running".into();
        }
        let out = self
            .runtime
            .exec(
                &row.runtime_name,
                "/bin/sh",
                &["-c", CLOSE_BROWSER],
                Duration::from_secs(30),
            )
            .await?;
        if out.status != Some(0) {
            return Err(refused(format!(
                "closing Chromium before the upgrade failed: {}",
                super::bounded(out.stderr.trim(), 500)
            )));
        }
        Ok(row)
    }

    /// Move a bot's computer to `image`, keeping its data. See the module
    /// documentation for the steps and guarantees.
    ///
    /// The computer keeps the network profile it was created with; `network`
    /// is the owner's explicit choice of another, and is required for a
    /// computer created before profiles were recorded.
    pub async fn upgrade_image(
        &self,
        bot_id: Uuid,
        image: &str,
        network: Option<&str>,
    ) -> Result<UpgradeReport, ComputerError> {
        if !valid_image(image) {
            return Err(refused("the image reference is not valid"));
        }
        if network.is_some_and(|n| n.is_empty() || !valid_image(n)) {
            return Err(refused("the network profile is not valid"));
        }
        let _op = self.op(bot_id).await;
        self.clear_interrupted_upgrades(bot_id).await?;
        let active: Option<bool> =
            sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = ?1")
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
        let Some(network) = network.map(str::to_owned).or_else(|| row.network.clone()) else {
            return Err(refused(
                "this computer's network profile was not recorded; choose it explicitly \
                 (--network public or --network none)",
            ));
        };
        if self.active_jobs(bot_id).await? > 0 {
            return Err(refused(
                "a guest job is active; wait for it or cancel it first",
            ));
        }
        let row = self.close_browser(bot_id, row).await?;
        self.stop_for_disk(bot_id, &row).await?;

        let id = Uuid::new_v4();
        let to_runtime_name = format!("elsebot-{bot_id}-{}", &id.simple().to_string()[..12]);
        sqlx::query(
            "INSERT INTO computer_upgrades
                 (id, bot_id, from_runtime_name, from_image, to_runtime_name, to_image, state,
                  from_network, to_network)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'started', ?7, ?8)",
        )
        .bind(id)
        .bind(bot_id)
        .bind(&row.runtime_name)
        .bind(&row.image)
        .bind(&to_runtime_name)
        .bind(image)
        .bind(&row.network)
        .bind(&network)
        .execute(&self.pool)
        .await?;

        let staging = self
            .config
            .state_dir
            .join("upgrades")
            .join(bot_id.to_string());
        let copied = self
            .copy_to_replacement(bot_id, &row, &to_runtime_name, image, &network, &staging)
            .await;
        let _ = std::fs::remove_dir_all(&staging);
        let copied = match copied {
            Ok(done) => done,
            Err(err) => {
                self.remove_guest_archive(&row.runtime_name).await;
                let _ = self.stop_confirmed(&row.runtime_name).await;
                let discarded = self.discard_replacement(&to_runtime_name).await;
                sqlx::query(
                    "UPDATE computer_upgrades SET state = ?2, finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'), error = ?3
                     WHERE id = ?1",
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

        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = Self::row(&mut tx, bot_id).await?.expect("exists");
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM computer_leases WHERE bot_id = ?1 AND released_at IS NULL)",
        )
        .bind(bot_id)
        .fetch_one(&mut *tx)
        .await?;
        if current.runtime_name != row.runtime_name || current.observed_state != "stopped" || held {
            drop(tx);
            let _ = self.discard_replacement(&to_runtime_name).await;
            sqlx::query(
                "UPDATE computer_upgrades SET state = 'failed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                     error = 'the computer changed during the upgrade'
                 WHERE id = ?1",
            )
            .bind(id)
            .execute(&self.pool)
            .await?;
            return Err(ComputerError::Busy(
                "the computer changed during the upgrade".into(),
            ));
        }
        sqlx::query(
            "UPDATE bot_computers SET runtime_name = ?2, image = ?3, network = ?4,
                 observed_state = 'stopped', generation = generation + 1,
                 services_generation = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .bind(&to_runtime_name)
        .bind(image)
        .bind(&network)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE computer_upgrades SET state = 'completed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'),
                 data_bytes = ?2, data_sha256 = ?3, unrestored_packages = ?4
             WHERE id = ?1",
        )
        .bind(id)
        .bind(copied.bytes as i64)
        .bind(&copied.sha256)
        .bind(
            (!copied.unrestored.is_empty())
                .then(|| serde_json::to_string(&copied.unrestored).expect("strings serialize")),
        )
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
            data_bytes: copied.bytes,
            data_sha256: copied.sha256,
            image_description: copied.description,
            unrestored_packages: copied.unrestored,
        })
    }

    async fn copy_to_replacement(
        &self,
        bot_id: Uuid,
        row: &super::Row,
        to_runtime_name: &str,
        image: &str,
        network: &str,
        staging: &Path,
    ) -> Result<Copied, ComputerError> {
        use std::os::unix::fs::DirBuilderExt;
        let io = |e: std::io::Error| refused(format!("host staging: {e}"));
        let _ = std::fs::remove_dir_all(staging);
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(staging)
            .map_err(io)?;

        let row = &self.configure_boot(bot_id, row).await?;
        self.admit(bot_id, row).await?;
        self.runtime.start(&row.runtime_name).await?;
        let out = self
            .runtime
            .exec(&row.runtime_name, "/bin/sh", &["-c", PACK], COPY_TIMEOUT)
            .await?;
        if out.status != Some(0) {
            return Err(refused(format!(
                "archiving the old computer's data failed: {}",
                super::bounded(out.stderr.trim(), 500)
            )));
        }
        let lines: Vec<&str> = out.stdout.lines().map(str::trim).collect();
        let packages = lines.get(3..).unwrap_or_default();
        let (Some(size), Some(sha256), Some(uid), true) = (
            lines.first().and_then(|s| s.parse::<u64>().ok()),
            lines
                .get(1)
                .filter(|s| s.len() == 64)
                .map(|s| (*s).to_owned()),
            lines.get(2).map(|s| (*s).to_owned()),
            packages.iter().all(|p| package_name(p)),
        ) else {
            return Err(refused("the old computer's archive report was unreadable"));
        };
        let archive: PathBuf = staging.join("data.tar");
        let mut copied = Err(super::RuntimeError::Unavailable("not attempted".into()));
        for attempt in 1..=3 {
            let _ = std::fs::remove_file(&archive);
            copied = self
                .runtime
                .copy_out(&row.runtime_name, GUEST_STAGING, &archive)
                .await;
            match &copied {
                Ok(()) => break,
                Err(err) => {
                    tracing::warn!(attempt, error = %err, "copying the upgrade archive failed")
                }
            }
        }
        self.remove_guest_archive(&row.runtime_name).await;
        copied?;
        self.stop_confirmed(&row.runtime_name).await?;
        let (host_size, host_sha) = sha256_file(&archive).map_err(io)?;
        if (host_size, host_sha.as_str()) != (size, sha256.as_str()) {
            return Err(refused(
                "the copied archive does not match the old computer's (size or SHA-256)",
            ));
        }

        let mut ports = Vec::new();
        let mut files = vec![(archive.clone(), GUEST_ARCHIVE.to_owned())];
        if self.config.desktop {
            let port: Option<i32> =
                sqlx::query_scalar("SELECT desktop_port FROM bot_computers WHERE bot_id = ?1")
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
        self.admit(bot_id, row).await?;
        self.runtime
            .create(&Spec {
                name: to_runtime_name.to_owned(),
                image: image.to_owned(),
                cpus: row.cpus as u32,
                memory_mib: row.memory_mib as u32,
                max_cpus: row.max().0,
                max_memory_mib: row.max().1,
                network: network.to_owned(),
                ports,
                files,
                mounts: self.project_mounts(bot_id)?,
            })
            .await?;
        let mut args = vec!["-c", UNPACK, "sh", &sha256, GUEST_ARCHIVE, &uid];
        args.extend(packages);
        let out = self
            .runtime
            .exec(to_runtime_name, "/bin/sh", &args, COPY_TIMEOUT)
            .await?;
        if out.status != Some(0) {
            return Err(refused(format!(
                "unpacking into the new computer failed: {}",
                super::bounded(out.stderr.trim(), 500)
            )));
        }
        let mut unrestored = Vec::new();
        let mut description = out.stdout.as_str();
        while let Some((line, rest)) = description.split_once('\n') {
            let Some(package) = line.strip_prefix("unrestored ") else {
                break;
            };
            unrestored.push(package.to_owned());
            description = rest;
        }
        self.stop_confirmed(to_runtime_name).await?;
        Ok(Copied {
            bytes: size,
            sha256,
            description: super::bounded(description.trim(), 4000),
            unrestored,
        })
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
        let last: Option<UpgradeRow> = sqlx::query_as(
            "SELECT id, from_runtime_name, from_image, data_bytes, data_sha256, from_network
             FROM computer_upgrades
             WHERE bot_id = ?1 AND state = 'completed' AND to_runtime_name = ?2
             ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )
        .bind(bot_id)
        .bind(&row.runtime_name)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, from_name, from_image, bytes, sha256, from_network)) = last else {
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = Self::row(&mut tx, bot_id).await?.expect("exists");
        if current.runtime_name != row.runtime_name || current.observed_state != "stopped" {
            return Err(ComputerError::Busy(
                "the computer changed during the rollback".into(),
            ));
        }
        sqlx::query(
            "UPDATE bot_computers SET runtime_name = ?2, image = ?3, network = ?4,
                 observed_state = 'stopped', generation = generation + 1,
                 services_generation = NULL, updated_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE bot_id = ?1",
        )
        .bind(bot_id)
        .bind(&from_name)
        .bind(&from_image)
        .bind(&from_network)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE computer_upgrades SET state = 'rolled_back', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1",
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
            unrestored_packages: Vec::new(),
        })
    }
}
