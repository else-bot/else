//! Restricted host execution for provider CLIs.
//!
//! Every provider process runs inside bubblewrap with its own PID, IPC, and
//! UTS namespaces, a read-only view of system directories, a private tmpfs
//! home, and only the bot's own directories bound read-write. The API's data
//! directory (owner token, master key, other bots), the owner's home, the
//! Docker socket, and the user session bus are not visible. The environment
//! is cleared and rebuilt from an allowlist. Network access stays available
//! because the provider talks to its service; database credentials and the
//! owner token are never present to use with it.
//!
//! Supervision: the sandbox is its own process group and session, bubblewrap
//! kills the sandbox if the API dies (`--die-with-parent`), and every process
//! carries an installation marker in its environment so a restarted API can
//! find and terminate leftovers before dispatching new work.

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Environment variable naming the installation and turn of a sandbox.
pub const MARKER_VAR: &str = "ELSE_BOT_SANDBOX";
/// Mount points inside the sandbox.
pub const SANDBOX_HOME: &str = "/home/bot";
pub const SANDBOX_WORK: &str = "/work";
pub const SANDBOX_PROGRAM: &str = "/opt/else/provider";
pub const SANDBOX_PROVIDER_HOME: &str = "/var/lib/provider-home";

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("bubblewrap (bwrap) is required to run provider CLIs: {0}")]
    Unavailable(String),
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
}

/// Per-bot host directories. Kept when a bot is deleted.
#[derive(Debug, Clone)]
pub struct BotDirs {
    pub root: PathBuf,
    pub work: PathBuf,
    pub provider_home: PathBuf,
}

/// A host file made visible inside the sandbox at `target` (absolute).
#[derive(Debug, Clone)]
pub struct Bind {
    pub source: PathBuf,
    pub target: String,
    pub writable: bool,
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub turn_id: Uuid,
    pub dirs: BotDirs,
    /// Host path of the provider executable; bound read-only.
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Extra environment, on top of the fixed allowlist.
    pub env: Vec<(String, String)>,
    pub extra_binds: Vec<Bind>,
}

#[derive(Clone)]
pub struct HostSandbox {
    bwrap: PathBuf,
    data_dir: PathBuf,
    marker_prefix: String,
}

impl HostSandbox {
    /// Find bubblewrap and prove it can start a sandbox here.
    pub fn detect(data_dir: &Path) -> Result<Self, HostError> {
        let bwrap = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file())
            .ok_or_else(|| HostError::Unavailable("bwrap was not found in /usr/bin".into()))?;
        let digest = Sha256::digest(data_dir.as_os_str().as_encoded_bytes());
        let marker_prefix: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        let sandbox = Self {
            bwrap,
            data_dir: data_dir.to_owned(),
            marker_prefix,
        };
        let status = std::process::Command::new(&sandbox.bwrap)
            .args(sandbox.base_args())
            .arg("--")
            .arg("/usr/bin/true")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|err| HostError::Unavailable(err.to_string()))?;
        if !status.status.success() {
            return Err(HostError::Unavailable(format!(
                "a test sandbox failed: {}",
                String::from_utf8_lossy(&status.stderr).trim()
            )));
        }
        Ok(sandbox)
    }

    pub fn marker_prefix(&self) -> &str {
        &self.marker_prefix
    }

    /// Create (if missing) the bot's private directories under the data
    /// directory, mode 0700.
    pub fn bot_dirs(&self, bot_id: Uuid, provider: &str) -> Result<BotDirs, HostError> {
        let root = self.data_dir.join("bots").join(bot_id.to_string());
        let dirs = BotDirs {
            work: root.join("work"),
            provider_home: root.join(format!("{provider}-home")),
            root,
        };
        for dir in [&dirs.root, &dirs.work, &dirs.provider_home] {
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .map_err(|e| HostError::Io(dir.display().to_string(), e))?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| HostError::Io(dir.display().to_string(), e))?;
        }
        Ok(dirs)
    }

    /// Arguments shared by every sandbox: namespaces and read-only system.
    fn base_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = [
            "--die-with-parent",
            "--new-session",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--unshare-cgroup-try",
            "--ro-bind",
            "/usr",
            "/usr",
            "--ro-bind",
            "/etc",
            "/etc",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--tmpfs",
            "/home",
            "--dir",
            SANDBOX_HOME,
            "--tmpfs",
            "/var",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        // Merged-/usr systems link these into /usr; others have directories.
        for top in ["/bin", "/sbin", "/lib", "/lib64"] {
            match fs::read_link(top) {
                Ok(target) => {
                    args.extend(["--symlink".into(), target.into_os_string(), top.into()]);
                }
                Err(_) if Path::new(top).is_dir() => {
                    args.extend(["--ro-bind".into(), top.into(), top.into()]);
                }
                Err(_) => {}
            }
        }
        // DNS: resolv.conf often links into /run (systemd-resolved).
        if let Ok(target) = fs::canonicalize("/etc/resolv.conf") {
            if let Some(dir) = target.parent().filter(|d| !d.starts_with("/etc")) {
                args.extend(["--ro-bind".into(), dir.into(), dir.into()]);
            }
        }
        args
    }

    fn marker(&self, turn_id: Uuid) -> String {
        format!("{}:{turn_id}", self.marker_prefix)
    }

    /// The full bubblewrap argument list for a launch.
    pub fn args(&self, spec: &LaunchSpec) -> Vec<OsString> {
        let mut args = self.base_args();
        let bind = |args: &mut Vec<OsString>, flag: &str, source: &Path, target: &str| {
            args.extend([flag.into(), source.into(), target.into()]);
        };
        bind(&mut args, "--ro-bind", &spec.program, SANDBOX_PROGRAM);
        bind(&mut args, "--bind", &spec.dirs.work, SANDBOX_WORK);
        bind(
            &mut args,
            "--bind",
            &spec.dirs.provider_home,
            SANDBOX_PROVIDER_HOME,
        );
        for extra in &spec.extra_binds {
            let flag = if extra.writable {
                "--bind"
            } else {
                "--ro-bind"
            };
            bind(&mut args, flag, &extra.source, &extra.target);
        }
        args.extend(["--chdir".into(), SANDBOX_WORK.into(), "--clearenv".into()]);
        let marker = self.marker(spec.turn_id);
        let mut env: Vec<(String, String)> = vec![
            ("HOME".into(), SANDBOX_HOME.into()),
            ("PATH".into(), "/usr/local/bin:/usr/bin:/bin".into()),
            ("LANG".into(), "C.UTF-8".into()),
            ("TMPDIR".into(), "/tmp".into()),
            (MARKER_VAR.into(), marker),
        ];
        env.extend(spec.env.iter().cloned());
        for (key, value) in env {
            args.extend(["--setenv".into(), key.into(), value.into()]);
        }
        args.push("--".into());
        args.push(SANDBOX_PROGRAM.into());
        args.extend(spec.args.iter().cloned());
        args
    }

    /// A command ready to spawn: no inherited environment, its own process
    /// group, and the marker on bubblewrap itself for discovery.
    pub fn command(&self, spec: &LaunchSpec) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.bwrap);
        command
            .args(self.args(spec))
            .env_clear()
            .env(MARKER_VAR, self.marker(spec.turn_id))
            .process_group(0)
            .kill_on_drop(true);
        command
    }

    /// Terminate every sandbox process of this installation left from an
    /// earlier API process. Returns how many were signalled.
    pub fn sweep_orphans(&self) -> usize {
        let pids = find_marked(&self.marker_prefix, None);
        for pid in &pids {
            // SAFETY: plain signal delivery to a process we own.
            unsafe {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
        pids.len()
    }

    /// Processes still alive for one turn's sandbox.
    pub fn live_processes(&self, turn_id: Uuid) -> Vec<i32> {
        find_marked(&self.marker_prefix, Some(turn_id))
    }
}

/// Scan `/proc` for processes owned by this user whose environment carries
/// this installation's marker (optionally for one turn).
fn find_marked(prefix: &str, turn: Option<Uuid>) -> Vec<i32> {
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    let wanted = match turn {
        Some(turn) => format!("{MARKER_VAR}={prefix}:{turn}"),
        None => format!("{MARKER_VAR}={prefix}:"),
    };
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    let own = std::process::id() as i32;
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| *pid != own)
        .filter(|pid| {
            fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| m.uid() == uid)
                && fs::read(format!("/proc/{pid}/environ")).is_ok_and(|env| {
                    env.split(|b| *b == 0)
                        .any(|var| var.starts_with(wanted.as_bytes()))
                })
        })
        .collect()
}

/// A running sandbox: its process group is terminated on request and
/// reaped before capacity is released.
pub struct Supervised {
    pub child: tokio::process::Child,
    pgid: i32,
}

impl Supervised {
    pub fn spawn(mut command: tokio::process::Command) -> std::io::Result<Self> {
        let child = command.spawn()?;
        let pgid = child.id().map(|id| id as i32).unwrap_or(-1);
        Ok(Self { child, pgid })
    }

    pub fn pid(&self) -> i32 {
        self.pgid
    }

    /// SIGTERM the process group, then SIGKILL after `grace`, and wait for
    /// the sandbox to exit. Its namespace ends with it.
    pub async fn terminate(&mut self, grace: Duration) {
        if self.pgid > 0 {
            // SAFETY: signalling our own child's process group.
            unsafe {
                libc::kill(-self.pgid, libc::SIGTERM);
            }
        }
        if tokio::time::timeout(grace, self.child.wait())
            .await
            .is_err()
        {
            if self.pgid > 0 {
                // SAFETY: as above.
                unsafe {
                    libc::kill(-self.pgid, libc::SIGKILL);
                }
            }
            let _ = self.child.wait().await;
        }
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        // A dropped handle must not leave a sandbox running.
        if self.pgid > 0 && self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: signalling our own child's process group.
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
        }
    }
}
