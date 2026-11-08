//! Provider CLIs on the host.
//!
//! Provider CLIs run directly on the host as the API's user, so a
//! bot can work with the owner's files and machine when the owner asks.
//! Every run starts in the
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
//! work (on Linux through `/proc` and pidfds, on macOS through libproc and
//! `KERN_PROCARGS2`).

use std::ffi::OsString;
use std::fs::{self, DirBuilder};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use sha2::{Digest, Sha256};
use uuid::Uuid;

/// Environment variable naming the installation and turn of a provider run.
pub const MARKER_VAR: &str = "ELSE_BOT_PROCESS";

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
    /// The provider's own private files under `root`.
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
    /// its trimmed stdout, or why it did not run: it could not start, it
    /// failed (with the last line of its stderr), or it printed nothing.
    pub fn probe(
        &self,
        program: &Path,
        args: &[&str],
        env: &[(String, String)],
    ) -> Result<String, String> {
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .env_clear()
            .envs(passed_env())
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null());
        // A file, not a pipe: a background child that keeps stderr open must
        // not hold the probe until it exits.
        let mut stderr = probe_stderr().map_err(|e| format!("could not start ({e})"))?;
        command.stderr(
            stderr
                .try_clone()
                .map_err(|e| format!("could not start ({e})"))?,
        );
        let mut retries = 5;
        let output = loop {
            match command.output() {
                // A concurrent fork can briefly inherit a freshly written executable's writer.
                Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) && retries > 0 => {
                    retries -= 1;
                    std::thread::sleep(Duration::from_millis(10));
                }
                result => break result.map_err(|e| format!("could not start ({e})"))?,
            }
        };
        if !output.status.success() {
            let mut text = String::new();
            let _ = stderr
                .seek(SeekFrom::Start(0))
                .and_then(|_| stderr.read_to_string(&mut text));
            return Err(match text.lines().map(str::trim).rfind(|l| !l.is_empty()) {
                Some(line) => {
                    let line: String = line.chars().take(200).collect();
                    format!("{}: {line}", output.status)
                }
                None => output.status.to_string(),
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if stdout.is_empty() {
            return Err("printed nothing".into());
        }
        Ok(stdout)
    }

    /// Run a launch to completion like a provider run and return its stdout:
    /// an error if it fails, writes more than `limit` bytes, or outlives
    /// `timeout`. Everything it started is stopped and swept before it
    /// returns (or in the background if the call is dropped).
    pub async fn bounded_output(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        limit: usize,
    ) -> Result<Vec<u8>, String> {
        use tokio::io::AsyncReadExt;
        let mut scope = super::cli::process::Scope::spawn(self, spec, Stdio::null(), Stdio::null())
            .map_err(|e| e.to_string())?;
        let stdout = scope.child().stdout.take();
        let run = async {
            let mut out = Vec::new();
            if let Some(stdout) = stdout {
                stdout
                    .take(limit as u64 + 1)
                    .read_to_end(&mut out)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            if out.len() > limit {
                return Err(format!("output exceeded {limit} bytes"));
            }
            let status = scope.child().wait().await.map_err(|e| e.to_string())?;
            if status.success() {
                Ok(out)
            } else {
                Err(format!("exited with {status}"))
            }
        };
        let result = tokio::time::timeout(timeout, run)
            .await
            .unwrap_or_else(|_| Err(format!("did not finish within {}s", timeout.as_secs())));
        let cleanup = scope.finish(super::cli::process::Finish::Kill).await;
        let out = result?;
        cleanup.map(|()| out)
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
        self.sweep(None)
    }

    pub fn sweep_turn(&self, turn_id: Uuid) -> Result<usize, HostError> {
        self.sweep(Some(turn_id))
    }

    fn sweep(&self, turn_id: Option<Uuid>) -> Result<usize, HostError> {
        let wanted = match turn_id {
            Some(id) => format!("{MARKER_VAR}={}", self.marker(id)),
            None => format!("{MARKER_VAR}={}:", self.marker_prefix),
        };
        let mut killed = Vec::new();
        for pid in find_marked(&self.marker_prefix, turn_id) {
            let Some(process) = procs::pin(pid, &wanted) else {
                continue;
            };
            if process.kill() {
                killed.push(process);
            }
        }
        let count = killed.len();
        for process in killed {
            if !process.wait(Duration::from_secs(5)) {
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

/// An anonymous file for a probe's stderr: created private, then unlinked.
fn probe_stderr() -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "else-probe-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    fs::remove_file(&path)?;
    Ok(file)
}

fn passed_env() -> impl Iterator<Item = (OsString, OsString)> {
    std::env::vars_os().filter(|(key, _)| {
        key.to_str()
            .is_some_and(|k| PASSED_ENV.contains(&k) || k.starts_with("LC_"))
    })
}

#[cfg(target_os = "linux")]
mod procs {
    use std::fs;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::fs::MetadataExt;
    use std::time::Duration;

    fn environ_has(pid: i32, wanted: &str) -> bool {
        fs::read(format!("/proc/{pid}/environ")).is_ok_and(|env| {
            env.split(|b| *b == 0)
                .any(|var| var.starts_with(wanted.as_bytes()))
        })
    }

    /// Scan `/proc`.
    pub fn marked(wanted: &str) -> Vec<i32> {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let Ok(entries) = fs::read_dir("/proc") else {
            return Vec::new();
        };
        let own = std::process::id() as i32;
        entries
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
            .filter(|pid| *pid != own)
            .filter(|pid| {
                fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| m.uid() == uid)
                    && environ_has(*pid, wanted)
            })
            .collect()
    }

    /// A pidfd: it refers to one process for its whole life.
    pub struct Pinned(OwnedFd);

    /// Pin `pid`, then confirm (through the pinned process) that it carries
    /// the marker.
    pub fn pin(pid: i32, wanted: &str) -> Option<Pinned> {
        // SAFETY: pidfd_open takes a pid and flags and returns a new fd or -1.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            return None;
        }
        // SAFETY: a non-negative return is a fresh descriptor we now own.
        let pinned = Pinned(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        environ_has(pid, wanted).then_some(pinned)
    }

    impl Pinned {
        pub fn kill(&self) -> bool {
            // SAFETY: signals exactly the process the pidfd refers to.
            unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.0.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                ) == 0
            }
        }

        /// A pidfd becomes readable when its process exits.
        pub fn wait(&self, timeout: Duration) -> bool {
            let mut poll = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as i32) > 0 }
        }
    }
}

/// macOS has neither `/proc` nor pidfds: processes are listed with
/// `proc_listallpids`, their environment read with `KERN_PROCARGS2`, and a
/// process is pinned by its PID and start time, checked again right before
/// it is signalled and while waiting, so a reused PID is never hit outside
/// that instant.
#[cfg(target_os = "macos")]
mod procs {
    use std::time::{Duration, Instant};

    fn info(pid: i32) -> Option<(libc::uid_t, (u64, u64), bool)> {
        // SAFETY: proc_bsdinfo is plain data; zeroed is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: the buffer is exactly one proc_bsdinfo.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size,
            )
        };
        // SZOMB: exited, not yet reaped.
        (read == size).then_some((
            info.pbi_uid,
            (info.pbi_start_tvsec, info.pbi_start_tvusec),
            info.pbi_status == 5,
        ))
    }

    /// The environment of `pid` (`KERN_PROCARGS2`: argc, the executable
    /// path, padding, the arguments, then the environment), readable only
    /// for this user's processes.
    fn environ(pid: i32) -> Option<Vec<Vec<u8>>> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size: libc::size_t = 0;
        // SAFETY: a size query with a null buffer.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` holds `size` bytes, which sysctl updates.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buf.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        buf.truncate(size);
        let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?).max(0) as usize;
        let mut strings = buf[4..].split(|b| *b == 0);
        strings.next()?;
        let mut strings = strings.skip_while(|s| s.is_empty()).skip(argc);
        Some(
            strings
                .by_ref()
                .take_while(|s| !s.is_empty())
                .map(<[u8]>::to_vec)
                .collect(),
        )
    }

    fn environ_has(pid: i32, wanted: &str) -> bool {
        environ(pid).is_some_and(|env| env.iter().any(|v| v.starts_with(wanted.as_bytes())))
    }

    pub fn marked(wanted: &str) -> Vec<i32> {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        // SAFETY: a count query with a null buffer.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Vec::new();
        }
        let mut pids = vec![0 as libc::pid_t; count as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer holds `bytes` bytes of pids.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if n <= 0 {
            return Vec::new();
        }
        pids.truncate(n as usize);
        let own = std::process::id() as i32;
        pids.into_iter()
            .filter(|pid| *pid > 0 && *pid != own)
            .filter(|pid| {
                info(*pid).is_some_and(|(owner, _, zombie)| owner == uid && !zombie)
                    && environ_has(*pid, wanted)
            })
            .collect()
    }

    pub struct Pinned {
        pid: i32,
        start: (u64, u64),
    }

    pub fn pin(pid: i32, wanted: &str) -> Option<Pinned> {
        let (_, start, zombie) = info(pid)?;
        (!zombie && environ_has(pid, wanted)).then_some(Pinned { pid, start })
    }

    impl Pinned {
        fn alive(&self) -> bool {
            info(self.pid).is_some_and(|(_, start, zombie)| start == self.start && !zombie)
        }

        pub fn kill(&self) -> bool {
            // SAFETY: signals `pid`, checked to still be the pinned process.
            self.alive() && unsafe { libc::kill(self.pid, libc::SIGKILL) } == 0
        }

        pub fn wait(&self, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            while self.alive() {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            true
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod procs {
    use std::time::Duration;

    pub fn marked(_wanted: &str) -> Vec<i32> {
        Vec::new()
    }

    pub struct Pinned;

    pub fn pin(_pid: i32, _wanted: &str) -> Option<Pinned> {
        None
    }

    impl Pinned {
        pub fn kill(&self) -> bool {
            false
        }

        pub fn wait(&self, _timeout: Duration) -> bool {
            true
        }
    }
}

fn find_marked(prefix: &str, turn: Option<Uuid>) -> Vec<i32> {
    match turn {
        Some(turn) => procs::marked(&format!("{MARKER_VAR}={prefix}:{turn}")),
        None => procs::marked(&format!("{MARKER_VAR}={prefix}:")),
    }
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

    /// Signal the process group, only while its leader is unreaped: until
    /// then the group ID cannot belong to anything else. After the leader is
    /// reaped this does nothing; survivors are reached by their marker.
    pub fn signal(&self, signal: i32) {
        if self.pgid > 0 && self.child.id().is_some() {
            // SAFETY: signalling our own child's process group.
            unsafe {
                libc::kill(-self.pgid, signal);
            }
        }
    }

    /// SIGKILL the process group and reap its leader.
    pub async fn kill(&mut self) {
        self.signal(libc::SIGKILL);
        let _ = self.child.wait().await;
    }

    /// SIGTERM the process group, then SIGKILL after `grace`, and wait for
    /// the process to exit.
    pub async fn terminate(&mut self, grace: Duration) {
        self.signal(libc::SIGTERM);
        if tokio::time::timeout(grace, self.child.wait())
            .await
            .is_err()
        {
            self.kill().await;
        }
    }
}

impl Drop for Supervised {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}
