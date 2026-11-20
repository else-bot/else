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
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use rustix::process::{Pid, Signal};

use super::cli::process::{Finish, Scope, Stderr, last_lines};
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

/// How long a run outside a turn may take unless it says otherwise.
pub const RUN_TIMEOUT: Duration = Duration::from_secs(30);
const RUN_LIMIT: usize = 1024 * 1024;

/// One CLI run to completion, supervised like a provider run: its own
/// process group with the installation marker, killed on drop, and bounded
/// in bytes and time.
#[derive(Debug, Clone)]
pub struct Run {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Extra environment, on top of the passed session variables.
    pub env: Vec<(String, String)>,
    pub dir: PathBuf,
    /// Every process the run starts carries it, so the run's leftovers are
    /// swept by it. One run per marker at a time.
    pub marker: Uuid,
    /// Written to stdin; empty means no stdin.
    pub input: Vec<u8>,
    pub limit: usize,
    pub timeout: Duration,
}

impl Run {
    /// `program args` outside any bot, in the temporary directory.
    pub fn new(program: &Path, args: &[&str]) -> Self {
        Self {
            program: program.to_owned(),
            args: args.iter().map(OsString::from).collect(),
            env: Vec::new(),
            dir: std::env::temp_dir(),
            marker: Uuid::new_v4(),
            input: Vec::new(),
            limit: RUN_LIMIT,
            timeout: RUN_TIMEOUT,
        }
    }

    /// A launch run like a provider turn: in the bot's project, under the
    /// launch's marker.
    pub fn launch(spec: &LaunchSpec) -> Self {
        Self {
            program: spec.program.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
            dir: spec.dirs.project.clone(),
            marker: spec.turn_id,
            input: Vec::new(),
            limit: RUN_LIMIT,
            timeout: RUN_TIMEOUT,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("could not start ({0})")]
    Start(String),
    #[error("could not write its input ({0})")]
    Write(std::io::Error),
    #[error("could not read its output ({0})")]
    Read(std::io::Error),
    #[error("its output exceeded {0} bytes")]
    TooLarge(usize),
    #[error("did not finish within {0:?}")]
    TimedOut(Duration),
    #[error("{status}{}", reason(.stderr))]
    Failed {
        status: std::process::ExitStatus,
        stderr: Option<String>,
    },
    #[error("it exited without answering{}", reason(.stderr))]
    NoAnswer { stderr: Option<String> },
    #[error("printed nothing")]
    Empty,
    #[error("{0}")]
    Cleanup(String),
}

impl RunError {
    fn stderr_slot(&mut self) -> Option<&mut Option<String>> {
        match self {
            Self::Failed { stderr, .. } | Self::NoAnswer { stderr } => Some(stderr),
            _ => None,
        }
    }
}

fn reason(stderr: &Option<String>) -> String {
    stderr
        .as_deref()
        .map_or_else(String::new, |line| format!(": {line}"))
}

struct Started {
    scope: Scope,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<Stderr>,
}

impl Started {
    async fn write_input(&mut self, run: &Run) -> Result<(), RunError> {
        use tokio::io::AsyncWriteExt;
        if let Some(stdin) = self.stdin.as_mut() {
            stdin.write_all(&run.input).await.map_err(RunError::Write)?;
        }
        Ok(())
    }

    async fn output(&mut self, run: &Run) -> Result<Vec<u8>, RunError> {
        use tokio::io::AsyncReadExt;
        self.write_input(run).await?;
        self.stdin = None;
        let mut out = Vec::new();
        if let Some(stdout) = self.stdout.take() {
            stdout
                .take(run.limit as u64 + 1)
                .read_to_end(&mut out)
                .await
                .map_err(RunError::Read)?;
        }
        if out.len() > run.limit {
            return Err(RunError::TooLarge(run.limit));
        }
        let status = self.scope.child().wait().await.map_err(RunError::Read)?;
        if !status.success() {
            return Err(RunError::Failed {
                status,
                stderr: None,
            });
        }
        Ok(out)
    }

    async fn answer<T>(
        &mut self,
        run: &Run,
        answer: impl Fn(&str) -> Option<T>,
    ) -> Result<T, RunError> {
        use tokio::io::AsyncReadExt;
        self.write_input(run).await?;
        let Some(stdout) = self.stdout.as_mut() else {
            return Err(RunError::NoAnswer { stderr: None });
        };
        let mut pending: Vec<u8> = Vec::new();
        let mut read = 0;
        let mut chunk = vec![0; 64 * 1024];
        loop {
            let n = stdout.read(&mut chunk).await.map_err(RunError::Read)?;
            if n == 0 {
                return Err(RunError::NoAnswer { stderr: None });
            }
            read += n;
            if read > run.limit {
                return Err(RunError::TooLarge(run.limit));
            }
            pending.extend_from_slice(&chunk[..n]);
            while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = pending.drain(..=end).collect();
                if let Some(found) = answer(String::from_utf8_lossy(&line).trim_end()) {
                    return Ok(found);
                }
            }
        }
    }

    /// Stop the process tree and sweep its marker. The run's own error wins
    /// over a cleanup error. A failure's reason is read after the sweep,
    /// when nothing is left holding stderr open.
    async fn finish<T>(self, result: Result<T, RunError>) -> Result<T, RunError> {
        drop(self.stdin);
        let cleanup = self.scope.finish(Finish::Kill).await;
        let mut error = match result {
            Ok(value) => return cleanup.map_err(RunError::Cleanup).map(|()| value),
            Err(error) => error,
        };
        if let (Some(slot), Some(stderr)) = (error.stderr_slot(), &self.stderr) {
            let tail = stderr.tail().await;
            *slot = last_lines(&tail, 1)
                .first()
                .map(|line| line.chars().take(200).collect());
        }
        Err(error)
    }
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

    /// Run `run` to completion and return its stdout. Everything it started
    /// is stopped and swept before this returns, or in the background if the
    /// call is dropped.
    pub async fn run(&self, run: &Run) -> Result<Vec<u8>, RunError> {
        let mut started = self.start(run).await?;
        let read = tokio::time::timeout(run.timeout, started.output(run));
        let result = read.await.unwrap_or(Err(RunError::TimedOut(run.timeout)));
        started.finish(result).await
    }

    /// Start `run`, write its input, and return the first stdout line
    /// `answer` accepts. Errors past `run.limit` bytes, at EOF, or after
    /// `run.timeout`; the process tree is stopped either way.
    pub async fn ask<T>(
        &self,
        run: &Run,
        answer: impl Fn(&str) -> Option<T>,
    ) -> Result<T, RunError> {
        let mut started = self.start(run).await?;
        let read = tokio::time::timeout(run.timeout, started.answer(run, answer));
        let result = read.await.unwrap_or(Err(RunError::TimedOut(run.timeout)));
        started.finish(result).await
    }

    /// Run a short check such as `--version` and return its trimmed stdout,
    /// or why it did not run: it could not start, it failed (with the last
    /// line of its stderr), it timed out, or it printed nothing.
    pub async fn probe(
        &self,
        program: &Path,
        args: &[&str],
        env: &[(String, String)],
    ) -> Result<String, RunError> {
        let run = Run {
            env: env.to_vec(),
            ..Run::new(program, args)
        };
        let stdout = self.run(&run).await?;
        let stdout = String::from_utf8_lossy(&stdout).trim().to_owned();
        if stdout.is_empty() {
            return Err(RunError::Empty);
        }
        Ok(stdout)
    }

    async fn start(&self, run: &Run) -> Result<Started, RunError> {
        let stdin = || {
            if run.input.is_empty() {
                Stdio::null()
            } else {
                Stdio::piped()
            }
        };
        let mut retries = 5;
        let mut scope = loop {
            let mut command =
                self.command_in(&run.program, &run.args, &run.env, &run.dir, run.marker);
            command
                .stdin(stdin())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            match Scope::supervise(self, run.marker, command) {
                // A concurrent fork can briefly inherit a freshly written executable's writer.
                Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) && retries > 0 => {
                    retries -= 1;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                result => break result.map_err(|e| RunError::Start(e.to_string()))?,
            }
        };
        let stdin = scope.child().stdin.take();
        let stdout = scope.child().stdout.take();
        let stderr = scope
            .child()
            .stderr
            .take()
            .map(|stderr| Stderr::collect(stderr, &run.env));
        Ok(Started {
            scope,
            stdin,
            stdout,
            stderr,
        })
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
        self.command_in(
            &spec.program,
            &spec.args,
            &spec.env,
            &spec.dirs.project,
            spec.turn_id,
        )
    }

    fn command_in(
        &self,
        program: &Path,
        args: &[OsString],
        env: &[(String, String)],
        dir: &Path,
        marker: Uuid,
    ) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(program);
        command
            .args(args)
            .current_dir(dir)
            .env_clear()
            .envs(passed_env())
            .envs(env.iter().map(|(k, v)| (k, v)))
            .env(MARKER_VAR, self.marker(marker))
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

fn passed_env() -> impl Iterator<Item = (OsString, OsString)> {
    std::env::vars_os().filter(|(key, _)| {
        key.to_str()
            .is_some_and(|k| PASSED_ENV.contains(&k) || k.starts_with("LC_"))
    })
}

#[cfg(target_os = "linux")]
mod procs {
    use std::fs;
    use std::os::fd::OwnedFd;

    use rustix::event::{PollFd, PollFlags, Timespec};
    use rustix::process::{Pid, PidfdFlags, Signal};
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
        let uid = rustix::process::getuid().as_raw();
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
        let pinned =
            Pinned(rustix::process::pidfd_open(Pid::from_raw(pid)?, PidfdFlags::empty()).ok()?);
        environ_has(pid, wanted).then_some(pinned)
    }

    impl Pinned {
        pub fn kill(&self) -> bool {
            rustix::process::pidfd_send_signal(&self.0, Signal::KILL).is_ok()
        }

        /// A pidfd becomes readable when its process exits.
        pub fn wait(&self, timeout: Duration) -> bool {
            let Ok(timeout) = Timespec::try_from(timeout) else {
                return false;
            };
            let mut fds = [PollFd::new(&self.0, PollFlags::IN)];
            rustix::event::poll(&mut fds, Some(&timeout)).is_ok_and(|ready| ready > 0)
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
        let uid = rustix::process::getuid().as_raw();
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
            self.alive()
                && rustix::process::Pid::from_raw(self.pid).is_some_and(|pid| {
                    rustix::process::kill_process(pid, rustix::process::Signal::KILL).is_ok()
                })
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
    pub fn signal(&self, signal: Signal) {
        if let Some(pgid) = Pid::from_raw(self.pgid)
            && self.child.id().is_some()
        {
            let _ = rustix::process::kill_process_group(pgid, signal);
        }
    }

    /// SIGKILL the process group and reap its leader.
    pub async fn kill(&mut self) {
        self.signal(Signal::KILL);
        let _ = self.child.wait().await;
    }

    /// SIGTERM the process group, then SIGKILL after `grace`, and wait for
    /// the process to exit.
    pub async fn terminate(&mut self, grace: Duration) {
        self.signal(Signal::TERM);
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
        self.signal(Signal::KILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(dir: &Path) -> Host {
        Host::new(&dir.join("data"), &dir.join("bots"))
    }

    fn sh(script: &str) -> Run {
        Run {
            limit: 1024,
            ..Run::new(Path::new("/bin/sh"), &["-c", script])
        }
    }

    #[tokio::test]
    async fn runs_stop_reading_at_their_limit_even_without_a_newline() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let flood = sh("head -c 4000000 /dev/zero; sleep 60");
        let asked = host.ask(&flood, |_| Some(())).await;
        assert_eq!(
            asked.unwrap_err().to_string(),
            "its output exceeded 1024 bytes"
        );
        let output = host.run(&flood).await;
        assert_eq!(
            output.unwrap_err().to_string(),
            "its output exceeded 1024 bytes"
        );
        let question = Run {
            input: b"hi\n".to_vec(),
            ..sh("read line; echo skip; echo \"got $line\"")
        };
        let answered = host
            .ask(&question, |l| l.strip_prefix("got ").map(str::to_owned))
            .await;
        assert_eq!(answered.unwrap(), "hi");
        assert_eq!(host.run(&sh("echo listed")).await.unwrap(), b"listed\n");
        assert_eq!(
            host.run(&sh("echo why >&2; exit 3"))
                .await
                .unwrap_err()
                .to_string(),
            "exit status: 3: why"
        );
        let silent = host
            .ask(&sh("echo not logged in >&2"), |_| None::<()>)
            .await;
        assert_eq!(
            silent.unwrap_err().to_string(),
            "it exited without answering: not logged in"
        );
    }

    fn gone(pid: i32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_or(true, |stat| stat.split_whitespace().nth(2) == Some("Z"))
    }

    async fn wait_gone(pid: i32) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !gone(pid) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        gone(pid)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_hanging_run_times_out_and_everything_it_started_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let pids = dir.path().join("pids");
        let script = format!(
            "setsid sleep 60 </dev/null >/dev/null 2>&1 & echo $! > {0}.new; \
             sleep 60 & echo $! >> {0}.new; echo $$ >> {0}.new; mv {0}.new {0}; wait",
            pids.display()
        );
        let run = Run {
            timeout: Duration::from_secs(3),
            ..sh(&script)
        };
        let timed_out = host.run(&run).await.unwrap_err();
        assert_eq!(timed_out.to_string(), "did not finish within 3s");
        let pids: Vec<i32> = fs::read_to_string(&pids)
            .unwrap()
            .lines()
            .map(|l| l.parse().unwrap())
            .collect();
        assert_eq!(
            pids.len(),
            3,
            "the shell, its child, and one that left the group"
        );
        for pid in pids {
            assert!(
                wait_gone(pid).await,
                "{pid} was killed when the run timed out"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_dropped_run_kills_its_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let pids = dir.path().join("pids");
        let script = format!("sleep 60 & echo $! > {}; wait", pids.display());
        let args = ["-c", script.as_str()];
        let run = Run::new(Path::new("/bin/sh"), &args);
        tokio::select! {
            _ = host.run(&run) => panic!("the run hangs until dropped"),
            () = async {
                while !fs::read_to_string(&pids).is_ok_and(|p| p.ends_with('\n')) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            } => {}
        }
        let child: i32 = fs::read_to_string(&pids).unwrap().trim().parse().unwrap();
        assert!(
            wait_gone(child).await,
            "the background child {child} was killed with its group"
        );
    }
}
