//! Provider CLIs on the host.
//!
//! Codex and Claude Code run directly on the host as the API's user, so a
//! bot can work with the owner's files and machine when the owner asks (see
//! docs/security.md, "Host provider processes"). Every run starts in the
//! bot's project directory (`<ELSE_BOTS_DIR>/<bot id>`), which is also
//! mounted into the bot's computer: one workspace on the host and in its VM.
//! Provider state (native sessions, settings, the shared login link) lives
//! in a private per-bot home under the API's data directory, outside the
//! project, so it never reaches the computer.
//!
//! The environment is rebuilt from an allowlist of the API's own (the user's
//! session variables), so the API's database URL, master key, and other
//! settings are not handed to the CLI. That is hygiene, not isolation: the
//! process can read and change anything the API's user can.
//!
//! Supervision: each run is its own process group, terminated with it, and
//! every process carries an installation marker in its environment so a
//! restarted API can find and terminate leftovers before dispatching new
//! work (through `/proc`, so on Linux only).

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Environment variable naming the installation and turn of a provider run.
pub const MARKER_VAR: &str = "ELSE_BOT_PROCESS";

/// Variables passed from the API's environment to provider CLIs (plus every
/// `LC_*`): the user's session, so host work sees the owner's home, tools,
/// and desktop session. Everything else, the API's own settings included,
/// is dropped.
const PASSED_ENV: &[&str] = &[
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "PATH",
    "LANG",
    "LANGUAGE",
    "TZ",
    "TMPDIR",
    "TERM",
    "XDG_RUNTIME_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "DBUS_SESSION_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "SSH_AUTH_SOCK",
];

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
}

/// A bot's directories on the host. Kept when the bot is deleted.
#[derive(Debug, Clone)]
pub struct BotDirs {
    /// Private to the API: `<data dir>/bots/<bot id>`.
    pub root: PathBuf,
    /// The bot's project directory, where every provider run starts; also
    /// its computer's `~/work`.
    pub project: PathBuf,
    /// The provider's own home (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`) under
    /// `root`.
    pub provider_home: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LaunchSpec {
    pub turn_id: Uuid,
    pub dirs: BotDirs,
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Extra environment, on top of the passed session variables.
    pub env: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct Host {
    data_dir: PathBuf,
    bots_dir: PathBuf,
    marker_prefix: String,
}

impl Host {
    pub fn new(data_dir: &Path, bots_dir: &Path) -> Self {
        let digest = Sha256::digest(data_dir.as_os_str().as_encoded_bytes());
        Self {
            data_dir: data_dir.to_owned(),
            bots_dir: bots_dir.to_owned(),
            marker_prefix: digest[..8].iter().map(|b| format!("{b:02x}")).collect(),
        }
    }

    /// Run `program args` with the passed environment plus `env` and return
    /// its stdout, or `None` if it fails.
    pub fn probe(&self, program: &Path, args: &[&str], env: &[(String, String)]) -> Option<String> {
        let output = std::process::Command::new(program)
            .args(args)
            .env_clear()
            .envs(passed_env())
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .filter(|s| !s.is_empty())
    }

    pub fn marker_prefix(&self) -> &str {
        &self.marker_prefix
    }

    /// The bot's project directory (not created).
    pub fn project_path(&self, bot_id: Uuid) -> PathBuf {
        self.bots_dir.join(bot_id.to_string())
    }

    /// Where a bot's provider runs start, part of session fingerprints: a
    /// native session never resumes in another directory.
    pub fn runtime(&self, bot_id: Uuid) -> String {
        format!("runtime:host:{}", self.project_path(bot_id).display())
    }

    /// Create (if missing) the bot's project directory and its private
    /// provider home, mode 0700.
    pub fn bot_dirs(&self, bot_id: Uuid, provider: &str) -> Result<BotDirs, HostError> {
        let root = self.data_dir.join("bots").join(bot_id.to_string());
        let project = project_dir(&self.bots_dir, bot_id)
            .map_err(|e| HostError::Io(self.project_path(bot_id).display().to_string(), e))?;
        let dirs = BotDirs {
            provider_home: root.join(format!("{provider}-home")),
            project,
            root,
        };
        for dir in [&dirs.root, &dirs.provider_home] {
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

    fn marker(&self, turn_id: Uuid) -> String {
        format!("{}:{turn_id}", self.marker_prefix)
    }

    /// A command ready to spawn: the program in the bot's project
    /// directory, the passed environment plus the launch's own, its own
    /// process group, and the marker for discovery.
    pub fn command(&self, spec: &LaunchSpec) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .current_dir(&spec.dirs.project)
            .env_clear()
            .envs(passed_env())
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .env(MARKER_VAR, self.marker(spec.turn_id))
            .process_group(0)
            .kill_on_drop(true);
        command
    }

    /// Terminate every provider process of this installation left from an
    /// earlier API process and wait until they have exited. Each process is
    /// pinned with a pidfd before its marker is re-checked and it is
    /// signalled, so a reused PID can never be hit. Returns how many were
    /// terminated, or an error if any survived.
    pub fn sweep_orphans(&self) -> Result<usize, HostError> {
        let wanted = format!("{MARKER_VAR}={}:", self.marker_prefix);
        let mut killed = Vec::new();
        for pid in find_marked(&self.marker_prefix, None) {
            let Some(fd) = pidfd_open(pid) else { continue };
            // The pidfd now refers to one process; confirm it is ours.
            if !has_marker(pid, &wanted) {
                continue;
            }
            if pidfd_kill(&fd) {
                killed.push(fd);
            }
        }
        let count = killed.len();
        for fd in killed {
            if !pidfd_wait(&fd, Duration::from_secs(5)) {
                return Err(HostError::Io(
                    "orphaned provider process".into(),
                    std::io::Error::other("did not exit after SIGKILL"),
                ));
            }
        }
        Ok(count)
    }

    /// Processes still alive for one turn's run.
    pub fn live_processes(&self, turn_id: Uuid) -> Vec<i32> {
        find_marked(&self.marker_prefix, Some(turn_id))
    }
}

/// `<root>/<bot id>`, created with mode 0700 if missing. Shared by provider
/// runs and the bot's computer, which mounts it.
pub fn project_dir(root: &Path, bot_id: Uuid) -> std::io::Result<PathBuf> {
    let dir = root.join(bot_id.to_string());
    DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    if !fs::symlink_metadata(&dir)?.is_dir() {
        return Err(std::io::Error::other("not a directory"));
    }
    Ok(dir)
}

/// Point `dir/name` at the host login `source`, replacing whatever is there
/// (an earlier link, or a placeholder file). The login is never copied, so
/// token refreshes write through to the one file.
pub fn link_login(dir: &Path, name: &str, source: &Path) -> std::io::Result<()> {
    let temp = dir.join(format!(".{name}.{}.link", Uuid::new_v4()));
    std::os::unix::fs::symlink(source, &temp)?;
    fs::rename(&temp, dir.join(name)).inspect_err(|_| {
        let _ = fs::remove_file(&temp);
    })
}

fn passed_env() -> impl Iterator<Item = (OsString, OsString)> {
    std::env::vars_os().filter(|(key, _)| {
        key.to_str()
            .is_some_and(|k| PASSED_ENV.contains(&k) || k.starts_with("LC_"))
    })
}

fn has_marker(pid: i32, wanted: &str) -> bool {
    fs::read(format!("/proc/{pid}/environ")).is_ok_and(|env| {
        env.split(|b| *b == 0)
            .any(|var| var.starts_with(wanted.as_bytes()))
    })
}

#[cfg(target_os = "linux")]
fn pidfd_open(pid: i32) -> Option<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    // SAFETY: a non-negative return is a fresh descriptor we now own.
    (fd >= 0).then(|| unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) })
}

#[cfg(target_os = "linux")]
fn pidfd_kill(fd: &std::os::fd::OwnedFd) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: signals exactly the process the pidfd refers to.
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        ) == 0
    }
}

/// A pidfd becomes readable when its process exits.
fn pidfd_wait(fd: &std::os::fd::OwnedFd, timeout: Duration) -> bool {
    use std::os::fd::AsRawFd;
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd for the duration of the call.
    unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as i32) > 0 }
}

// Without pidfds (or /proc) there is nothing to find or pin.
#[cfg(not(target_os = "linux"))]
fn pidfd_open(_pid: i32) -> Option<std::os::fd::OwnedFd> {
    None
}

#[cfg(not(target_os = "linux"))]
fn pidfd_kill(_fd: &std::os::fd::OwnedFd) -> bool {
    false
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

/// A running provider process: its process group is terminated on request
/// and reaped before capacity is released.
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
    /// the process to exit.
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
        // A dropped handle must not leave a provider process running.
        if self.pgid > 0 && self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: signalling our own child's process group.
            unsafe {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
        }
    }
}
