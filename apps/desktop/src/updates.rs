//! Updating this installation from the app.
//!
//! One actor owns the state and publishes it as a [`Snapshot`]; the window
//! sends it bounded [`Commands`]. It checks the release tags at launch and
//! every few hours, asks the local server whether bots are working, runs
//! the installation's own `bin/elsebot update`, and relaunches the app
//! through the installation once that succeeded.
//!
//! The installer is the authority on whether an update may stop the server:
//! it closes the API's maintenance gate and refuses while bots work. The app
//! reads the same counts beforehand (`GET /v1/maintenance/update`) and
//! refuses to start when work is busy or unknown. The installer closes
//! the race between that read and stopping the server.
//!
//! The installer runs in its own session with its output in
//! `logs/desktop-update.log`, which the actor follows. Quitting the app
//! never interrupts it halfway: it finishes on its own, and its result is
//! in that log and in `elsebot status`.

mod output;
mod release;

use std::ffi::OsString;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use reqwest::Method;
use tokio::io::AsyncReadExt as _;
use tokio::sync::{mpsc, watch};

use crate::api::{Client, ClientError};
use crate::config::OwnerToken;
use crate::launch::{Installation, Launch, LocalServer, service_env};

use release::Version;

const OUTPUT_CAP: usize = 64 * 1024;
const UPDATE_LOG: &str = "logs/desktop-update.log";
const DESKTOP_LOG: &str = "logs/desktop.log";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    Checking,
    Current { latest: String },
    Available { latest: String },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Work {
    Checking,
    Idle,
    Busy { turns: u64, commands: u64 },
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    Idle,
    Preparing,
    Installing,
    Ready { version: String },
    Failed(String),
    Restarting,
    Relaunched,
    RestartFailed(String),
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub check: Check,
    pub work: Work,
    pub operation: Operation,
    /// The installer's output, without terminal escapes, capped to its end.
    pub output: String,
    /// Why this app cannot update its installation, if it cannot.
    pub unavailable: Option<String>,
    pub checked_at: Option<SystemTime>,
}

impl Snapshot {
    pub fn initial() -> Self {
        Self {
            check: Check::Checking,
            work: Work::Checking,
            operation: Operation::Idle,
            output: String::new(),
            unavailable: None,
            checked_at: None,
        }
    }

    fn unavailable(why: String) -> Self {
        Self {
            check: Check::Failed(why.clone()),
            work: Work::Unavailable(why.clone()),
            unavailable: Some(why),
            ..Self::initial()
        }
    }

    fn running(&self) -> bool {
        matches!(
            self.operation,
            Operation::Preparing
                | Operation::Installing
                | Operation::Restarting
                | Operation::Relaunched
        )
    }

    fn install_refusal(&self) -> Option<String> {
        if let Some(why) = &self.unavailable {
            return Some(why.clone());
        }
        match &self.operation {
            Operation::Preparing | Operation::Installing => {
                return Some("An update is already running.".into());
            }
            Operation::Ready { version } => {
                return Some(format!(
                    "{version} is installed; restart else.bot to use it."
                ));
            }
            Operation::Restarting | Operation::Relaunched => {
                return Some("else.bot is restarting.".into());
            }
            Operation::Idle | Operation::Failed(_) | Operation::RestartFailed(_) => {}
        }
        match &self.check {
            Check::Available { .. } => {}
            Check::Checking => return Some("Still checking for updates.".into()),
            Check::Current { latest } => return Some(format!("{latest} is already installed.")),
            Check::Failed(why) => return Some(format!("Check for updates again first. {why}")),
        }
        match &self.work {
            Work::Idle => None,
            Work::Checking => Some("Still checking whether bots are working.".into()),
            Work::Busy { turns, commands } => Some(busy(*turns, *commands)),
            Work::Unavailable(why) => Some(why.clone()),
        }
    }

    fn restart_refusal(&self) -> Option<String> {
        if let Some(why) = &self.unavailable {
            return Some(why.clone());
        }
        match &self.operation {
            Operation::Ready { .. } | Operation::RestartFailed(_) => None,
            Operation::Restarting | Operation::Relaunched => Some("else.bot is restarting.".into()),
            _ => Some("Install the update before restarting.".into()),
        }
    }
}

fn busy(turns: u64, commands: u64) -> String {
    let count = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    format!(
        "Bots are working ({}, {}). Update once they have finished.",
        count(turns, "active turn", "active turns"),
        count(commands, "running command", "running commands")
    )
}

#[derive(Debug, Clone, Copy)]
enum Command {
    Check,
    Install,
    RefreshWork,
    Restart,
}

/// What the window may ask for. Each call returns at once: `Err` says why
/// the request was not taken; its outcome appears in the snapshot.
#[derive(Clone)]
pub struct Commands {
    tx: mpsc::Sender<Command>,
    state: watch::Receiver<Snapshot>,
    pending: Arc<AtomicBool>,
}

impl Commands {
    pub fn check(&self) -> Result<(), String> {
        let refusal = {
            let state = self.state.borrow();
            state.unavailable.clone().or_else(|| {
                matches!(
                    state.operation,
                    Operation::Preparing | Operation::Installing
                )
                .then(|| "An update is already running.".into())
            })
        };
        self.send(refusal, Command::Check)
    }

    pub fn install(&self) -> Result<(), String> {
        let refusal = self.state.borrow().install_refusal();
        self.send_once(refusal, Command::Install)
    }

    pub fn refresh_work(&self) -> Result<(), String> {
        let refusal = self.state.borrow().unavailable.clone();
        self.send(refusal, Command::RefreshWork)
    }

    pub fn restart(&self) -> Result<(), String> {
        let refusal = self.state.borrow().restart_refusal();
        self.send_once(refusal, Command::Restart)
    }

    fn send_once(&self, refusal: Option<String>, command: Command) -> Result<(), String> {
        if refusal.is_none() && self.pending.swap(true, Ordering::AcqRel) {
            return Err("The last request is still starting.".into());
        }
        let sent = self.send(refusal.clone(), command);
        if sent.is_err() && refusal.is_none() {
            self.pending.store(false, Ordering::Release);
        }
        sent
    }

    fn send(&self, refusal: Option<String>, command: Command) -> Result<(), String> {
        if let Some(why) = refusal {
            return Err(why);
        }
        self.tx.try_send(command).map_err(|err| match err {
            mpsc::error::TrySendError::Full(_) => {
                "Updates are still handling the last request; try again in a moment.".into()
            }
            mpsc::error::TrySendError::Closed(_) => {
                "Updates have stopped; restart else.bot to use them again.".into()
            }
        })
    }
}

pub fn start(
    launch: &Launch,
    runtime: &tokio::runtime::Handle,
) -> (Commands, watch::Receiver<Snapshot>) {
    start_with(launch.updates.clone(), Settings::default(), runtime)
}

#[derive(Debug, Clone)]
struct Settings {
    git: PathBuf,
    repo: String,
    check_every: Duration,
    work_every: Duration,
    check_limit: Duration,
    work_limit: Duration,
    relaunch_grace: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            git: "git".into(),
            repo: release::REPO.into(),
            check_every: Duration::from_secs(4 * 60 * 60),
            work_every: Duration::from_secs(10),
            check_limit: Duration::from_secs(30),
            work_limit: Duration::from_secs(10),
            relaunch_grace: Duration::from_secs(2),
        }
    }
}

fn start_with(
    installation: Result<Installation, String>,
    settings: Settings,
    runtime: &tokio::runtime::Handle,
) -> (Commands, watch::Receiver<Snapshot>) {
    let (tx, commands) = mpsc::channel(4);
    let installation = match installation {
        Ok(installation) => installation,
        Err(why) => {
            let (_, state) = watch::channel(Snapshot::unavailable(why));
            let pending = Arc::default();
            return (
                Commands {
                    tx,
                    state: state.clone(),
                    pending,
                },
                state,
            );
        }
    };
    let (state_tx, state) = watch::channel(Snapshot::initial());
    let (events_tx, events) = mpsc::channel(16);
    let pending = Arc::new(AtomicBool::new(false));
    let actor = Actor {
        pending: pending.clone(),
        installation,
        settings,
        state: state_tx,
        events: events_tx,
        log: output::Log::new(OUTPUT_CAP),
        client: None,
        ready: None,
        checking: false,
        counting: false,
    };
    runtime.spawn(actor.run(commands, events));
    (
        Commands {
            tx,
            state: state.clone(),
            pending,
        },
        state,
    )
}

#[derive(Debug)]
enum Event {
    Checked(Result<(Version, Version), String>),
    Work(Work),
    Operation(Operation),
    Output(Vec<u8>),
    Installed(Result<Version, String>),
    Relaunched(Result<(), String>),
}

struct Actor {
    pending: Arc<AtomicBool>,
    installation: Installation,
    settings: Settings,
    state: watch::Sender<Snapshot>,
    events: mpsc::Sender<Event>,
    log: output::Log,
    client: Option<(OwnerToken, Arc<Client>)>,
    ready: Option<Version>,
    checking: bool,
    counting: bool,
}

impl Actor {
    async fn run(
        mut self,
        mut commands: mpsc::Receiver<Command>,
        mut events: mpsc::Receiver<Event>,
    ) {
        let mut check_timer = tokio::time::interval(self.settings.check_every);
        let mut work_timer = tokio::time::interval(self.settings.work_every);
        for timer in [&mut check_timer, &mut work_timer] {
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        }
        let mut open = true;
        loop {
            tokio::select! {
                command = commands.recv(), if open => match command {
                    Some(command) => self.command(command),
                    None => open = false,
                },
                Some(event) = events.recv() => self.event(event),
                _ = check_timer.tick() => {
                    if !self.state.borrow().running() {
                        self.check();
                    }
                }
                _ = work_timer.tick() => {
                    if !self.state.borrow().running() {
                        self.count_work();
                    }
                }
                () = self.state.closed() => return,
            }
        }
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Check => {
                if !matches!(
                    self.state.borrow().operation,
                    Operation::Preparing | Operation::Installing
                ) {
                    self.check();
                }
            }
            Command::RefreshWork => self.count_work(),
            Command::Install => self.install(),
            Command::Restart => self.restart(),
        }
        if matches!(command, Command::Install | Command::Restart) {
            self.pending.store(false, Ordering::Release);
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Checked(result) => {
                self.checking = false;
                let running = env!("CARGO_PKG_VERSION").parse::<Version>().ok();
                let awaiting_restart = result
                    .as_ref()
                    .ok()
                    .map(|(_, installed)| *installed)
                    .filter(|installed| running.is_some_and(|running| *installed > running))
                    .filter(|_| matches!(self.state.borrow().operation, Operation::Idle));
                if let Some(installed) = awaiting_restart {
                    self.ready = Some(installed);
                }
                let check = match result {
                    Ok((latest, installed)) if latest > installed => Check::Available {
                        latest: latest.to_string(),
                    },
                    Ok((latest, _)) => Check::Current {
                        latest: latest.to_string(),
                    },
                    Err(why) => Check::Failed(why),
                };
                let available = matches!(check, Check::Available { .. });
                self.state.send_modify(|state| {
                    if !matches!(check, Check::Failed(_)) {
                        state.checked_at = Some(SystemTime::now());
                    }
                    state.check = check;
                    if let Some(installed) = awaiting_restart {
                        state.operation = Operation::Ready {
                            version: installed.to_string(),
                        };
                    }
                });
                if available {
                    self.count_work();
                }
            }
            Event::Work(work) => {
                self.counting = false;
                self.state.send_modify(|state| state.work = work);
            }
            Event::Operation(operation) => {
                self.state.send_modify(|state| state.operation = operation);
            }
            Event::Output(bytes) => {
                self.log.push(&bytes);
                let text = self.log.text().to_owned();
                self.state.send_modify(|state| state.output = text);
            }
            Event::Relaunched(result) => {
                self.state.send_modify(|state| {
                    state.operation = match result {
                        Ok(()) => Operation::Relaunched,
                        Err(why) => Operation::RestartFailed(why),
                    }
                });
            }
            Event::Installed(result) => {
                self.ready = result.as_ref().ok().copied();
                self.log.finish();
                let text = self.log.text().to_owned();
                self.state.send_modify(|state| {
                    state.output = text;
                    state.operation = match result {
                        Ok(version) => Operation::Ready {
                            version: version.to_string(),
                        },
                        Err(why) => Operation::Failed(why),
                    };
                });
                self.check();
            }
        }
    }

    fn check(&mut self) {
        if std::mem::replace(&mut self.checking, true) {
            return;
        }
        self.state
            .send_modify(|state| state.check = Check::Checking);
        let (home, settings, events) = (
            self.installation.home().to_owned(),
            self.settings.clone(),
            self.events.clone(),
        );
        tokio::spawn(async move {
            let result = async {
                let installed = release::installed(&home)?;
                let env = service_env(std::env::vars_os(), &home);
                let latest =
                    release::latest(&settings.git, &settings.repo, env, settings.check_limit)
                        .await?;
                Ok((latest, installed))
            }
            .await;
            let _ = events.send(Event::Checked(result)).await;
        });
    }

    fn count_work(&mut self) {
        if std::mem::replace(&mut self.counting, true) {
            return;
        }
        let (counter, events) = (self.counter(), self.events.clone());
        tokio::spawn(async move {
            let _ = events.send(Event::Work(counter.count().await)).await;
        });
    }

    fn counter(&mut self) -> Counter {
        let Some(server) = self.installation.server() else {
            return Counter::AppOnly;
        };
        let token = match server.key() {
            Ok(token) => token,
            Err(err) => return Counter::Unavailable(err.to_string()),
        };
        if let Some((known, client)) = &self.client
            && *known == token
        {
            return Counter::Server(server.clone(), client.clone(), self.settings.work_limit);
        }
        match Client::new(
            server.origin().clone(),
            &token,
            tokio::runtime::Handle::current(),
        ) {
            Ok(client) => {
                let client = Arc::new(client);
                self.client = Some((token, client.clone()));
                Counter::Server(server.clone(), client, self.settings.work_limit)
            }
            Err(why) => Counter::Unavailable(why),
        }
    }

    fn install(&mut self) {
        let Some(target) = ({
            let state = self.state.borrow();
            match (&state.check, state.install_refusal()) {
                (Check::Available { latest }, None) => Version::from_tag(latest),
                _ => None,
            }
        }) else {
            return;
        };
        self.log = output::Log::new(OUTPUT_CAP);
        self.state.send_modify(|state| {
            state.operation = Operation::Preparing;
            state.output.clear();
        });
        let (counter, events) = (self.counter(), self.events.clone());
        let home = self.installation.home().to_owned();
        tokio::spawn(async move {
            let work = counter.count().await;
            let idle = work == Work::Idle;
            if events.send(Event::Work(work)).await.is_err() {
                return;
            }
            if !idle {
                let _ = events.send(Event::Operation(Operation::Idle)).await;
                return;
            }
            let result = run_installer(&home, target, &events).await;
            let _ = events.send(Event::Installed(result)).await;
        });
    }

    fn restart(&mut self) {
        let ready = self.state.borrow().restart_refusal().is_none();
        let Some(version) = self.ready.filter(|_| ready) else {
            return;
        };
        self.state
            .send_modify(|state| state.operation = Operation::Restarting);
        let (home, grace, events) = (
            self.installation.home().to_owned(),
            self.settings.relaunch_grace,
            self.events.clone(),
        );
        tokio::spawn(async move {
            let result = async {
                let installed = release::installed(&home)?;
                if installed != version {
                    return Err(format!(
                        "{installed} is installed now, not {version}; check for updates again."
                    ));
                }
                relaunch(&home, grace).await
            }
            .await;
            let _ = events.send(Event::Relaunched(result)).await;
        });
    }
}

enum Counter {
    AppOnly,
    Unavailable(String),
    Server(LocalServer, Arc<Client>, Duration),
}

#[derive(serde::Deserialize)]
struct Report {
    active_turns: u64,
    running_commands: u64,
}

impl Counter {
    async fn count(self) -> Work {
        let (server, client, limit) = match self {
            Self::AppOnly => return Work::Idle,
            Self::Unavailable(why) => return Work::Unavailable(why),
            Self::Server(server, client, limit) => (server, client, limit),
        };
        let origin = server.origin().as_str();
        let request = client.request(Method::GET, "/v1/maintenance/update");
        match tokio::time::timeout(limit, client.send_json::<Report>(request)).await {
            Ok(Ok((_, report))) if report.active_turns == 0 && report.running_commands == 0 => {
                Work::Idle
            }
            Ok(Ok((_, report))) => Work::Busy {
                turns: report.active_turns,
                commands: report.running_commands,
            },
            Ok(Err(ClientError::Unreachable { .. })) | Err(_) => Work::Unavailable(format!(
                "The server at {origin} is not answering, so the app cannot tell whether bots \
                 are working. Start it (elsebot start), or update in a terminal: elsebot update"
            )),
            Ok(Err(ClientError::Unauthorized)) => Work::Unavailable(format!(
                "The server at {origin} rejected this installation's owner key, so the app \
                 cannot tell whether bots are working. Check: elsebot status"
            )),
            Ok(Err(ClientError::Protocol(why)))
                if why.starts_with("HTTP 404") || why.starts_with("HTTP 405") =>
            {
                Work::Unavailable(
                    "The running server is too old to say whether bots are working. Update \
                     once in a terminal: elsebot update"
                        .into(),
                )
            }
            Ok(Err(err)) => Work::Unavailable(format!(
                "The server at {origin} could not say whether bots are working: {err}"
            )),
        }
    }
}

fn child_env(home: &Path) -> Vec<(OsString, OsString)> {
    let mut env = service_env(std::env::vars_os(), home);
    env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    env
}

fn private_log(
    home: &Path,
    relative: &str,
    truncate: bool,
) -> Result<(PathBuf, std::fs::File), String> {
    let path = home.join(relative);
    let opened =
        std::fs::create_dir_all(path.parent().expect("a log has a directory")).and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .append(!truncate)
                .truncate(truncate)
                .mode(0o600)
                .open(&path)
        });
    opened
        .map(|file| (path.clone(), file))
        .map_err(|err| format!("Cannot write {}: {err}.", path.display()))
}

/// A new session: no controlling terminal for sudo or git to prompt on, and
/// no signal from this app's terminal or process group.
fn detached(
    program: &Path,
    home: &Path,
    log: &std::fs::File,
) -> std::io::Result<tokio::process::Command> {
    let mut command = tokio::process::Command::new(program);
    command
        .current_dir("/")
        .env_clear()
        .envs(child_env(home))
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

async fn run_installer(
    home: &Path,
    target: Version,
    events: &mpsc::Sender<Event>,
) -> Result<Version, String> {
    let (path, log) = private_log(home, UPDATE_LOG, true)?;
    let program = home.join("bin/elsebot");
    let mut child = detached(&program, home, &log)
        .and_then(|mut command| {
            command
                .args(["update", "--yes", "--version", &target.plain()])
                .spawn()
        })
        .map_err(|err| format!("Cannot run {}: {err}.", program.display()))?;
    drop(log);
    let _ = events.send(Event::Operation(Operation::Installing)).await;
    let mut reader = tokio::fs::File::open(&path)
        .await
        .map_err(|err| format!("Cannot read {}: {err}.", path.display()))?;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut poll = tokio::time::interval(Duration::from_millis(200));
    let status = loop {
        let ended = tokio::select! {
            status = child.wait() => Some(status),
            _ = poll.tick() => None,
        };
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if events
                        .send(Event::Output(chunk[..n].to_vec()))
                        .await
                        .is_err()
                    {
                        return Err("The app stopped following the update.".into());
                    }
                }
            }
        }
        if let Some(status) = ended {
            break status.map_err(|err| format!("The update could not be followed: {err}."))?;
        }
    };
    if !status.success() {
        return Err(failure(&path, &status.to_string()));
    }
    match release::installed(home) {
        Ok(installed) if installed == target => Ok(target),
        Ok(installed) => Err(format!(
            "The installer finished, but {installed} is installed instead of {target}. The log is {}.",
            path.display()
        )),
        Err(why) => Err(why),
    }
}

fn failure(log: &Path, status: &str) -> String {
    let mut tail = Vec::new();
    if let Ok(mut file) = std::fs::File::open(log) {
        let length = file.metadata().map_or(0, |metadata| metadata.len());
        if file
            .seek(SeekFrom::Start(length.saturating_sub(OUTPUT_CAP as u64)))
            .is_ok()
        {
            let _ = file.take(OUTPUT_CAP as u64).read_to_end(&mut tail);
        }
    }
    let mut rendered = output::Log::new(OUTPUT_CAP);
    rendered.push(&tail);
    rendered.finish();
    let lines: Vec<&str> = rendered
        .text()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let reason = lines
        .iter()
        .rev()
        .find_map(|line| {
            line.strip_prefix("error:")
                .or_else(|| line.strip_prefix("elsebot:"))
        })
        .or(lines.last().copied())
        .map(str::trim)
        .unwrap_or("it printed nothing");
    let advice = if reason.contains("sudo") {
        "It needs your password for system packages: run elsebot update in a terminal."
    } else {
        "Nothing was switched if it stopped before \"Switch\"; see the log, or run elsebot update in a terminal."
    };
    format!(
        "The update failed ({status}): {reason}. {advice} The log is {}.",
        log.display()
    )
}

async fn relaunch(home: &Path, grace: Duration) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        return open_bundle(home).await;
    }
    let (path, log) = private_log(home, DESKTOP_LOG, false)?;
    let program = home.join("bin/elsebot");
    let mut child = detached(&program, home, &log)
        .and_then(|mut command| command.arg("desktop").spawn())
        .map_err(|err| format!("Cannot run {}: {err}.", program.display()))?;
    match tokio::time::timeout(grace, child.wait()).await {
        Err(_) => {
            tokio::spawn(async move { child.wait().await });
            Ok(())
        }
        Ok(status) => Err(format!(
            "The updated app stopped right away ({}). The log is {}.",
            status.map_or_else(|err| err.to_string(), |status| status.to_string()),
            path.display()
        )),
    }
}

/// macOS: a new instance of the bundle the installer published, through
/// LaunchServices. `open -n`, because this instance still runs.
async fn open_bundle(home: &Path) -> Result<(), String> {
    let marker = home.join("config/elsebot.env");
    let app = tokio::process::Command::new("/bin/bash")
        .args(["-c", ". \"$1\" && printf %s \"${ELSEBOT_APP:-}\"", "-"])
        .arg(&marker)
        .current_dir("/")
        .env_clear()
        .envs(child_env(home))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let app = match tokio::time::timeout(Duration::from_secs(5), app).await {
        Ok(Ok(out)) if out.status.success() => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    };
    if app.is_empty() || !Path::new(&app).join("Contents/Info.plist").is_file() {
        return Err(format!(
            "This installation names no app to open ({}). Publish it again with: elsebot update",
            marker.display()
        ));
    }
    let opened = tokio::process::Command::new("/usr/bin/open")
        .arg("-n")
        .arg(&app)
        .current_dir("/")
        .env_clear()
        .envs(child_env(home))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(Duration::from_secs(15), opened).await {
        Ok(Ok(out)) if out.status.success() => Ok(()),
        Ok(Ok(out)) => Err(format!(
            "macOS could not open {app}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Ok(Err(err)) => Err(format!("Cannot run open: {err}.")),
        Err(_) => Err(format!("macOS did not open {app} within 15 s.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::AtomicU16;
    use tokio::io::AsyncWriteExt as _;

    use crate::config::Origin;

    const TOKEN: &str = "KKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKK";

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn releases(dir: &Path) -> String {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "x"]);
        for tag in [
            "v0.0.9",
            "v0.0.19",
            "v0.0.20",
            "v0.0.21-rc.1",
            "v1.0.0-beta",
        ] {
            git(&repo, &["tag", tag]);
        }
        git(&repo, &["tag", "-a", "v0.0.10", "-m", "annotated"]);
        repo.to_str().unwrap().to_owned()
    }

    struct Home {
        dir: tempfile::TempDir,
        repo: String,
    }

    impl Home {
        fn new(script: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = releases(dir.path());
            let home = dir.path().join("home");
            std::fs::create_dir_all(home.join("versions/0.0.9/bin")).unwrap();
            std::os::unix::fs::symlink("versions/0.0.9", home.join("current")).unwrap();
            let program = home.join("bin/elsebot");
            std::fs::create_dir_all(program.parent().unwrap()).unwrap();
            std::fs::write(
                &program,
                format!(
                    "#!/bin/sh\n\
                     sid=$(awk '{{print $6}}' /proc/$$/stat 2>/dev/null || echo $$)\n\
                     read line; stdin=$?\n\
                     echo \"$* cwd=$(pwd) home=$ELSE_HOME stdin=$stdin leader=$([ \"$sid\" = $$ ] && echo yes)\" >>\"$ELSE_HOME/calls\"\n\
                     {script}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self { dir, repo }
        }

        fn home(&self) -> PathBuf {
            self.dir.path().join("home")
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.home().join("calls"))
                .map(|text| text.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        }

        fn settings(&self) -> Settings {
            Settings {
                repo: self.repo.clone(),
                work_every: Duration::from_millis(100),
                relaunch_grace: Duration::from_millis(300),
                ..Settings::default()
            }
        }

        fn start(&self, server: Option<&Origin>) -> (Commands, watch::Receiver<Snapshot>) {
            let token = OwnerToken::parse(TOKEN).unwrap();
            let installation =
                Installation::for_tests(self.home(), server.map(|origin| (origin.clone(), token)));
            start_with(
                Ok(installation),
                self.settings(),
                &tokio::runtime::Handle::current(),
            )
        }
    }

    async fn until(
        state: &mut watch::Receiver<Snapshot>,
        what: &str,
        ok: impl FnMut(&Snapshot) -> bool,
    ) -> Snapshot {
        let found = tokio::time::timeout(Duration::from_secs(15), async {
            state.wait_for(ok).await.map(|snapshot| snapshot.clone())
        })
        .await;
        match found {
            Ok(Ok(snapshot)) => snapshot,
            _ => panic!("{what}: {:#?}", *state.borrow()),
        }
    }

    fn available(snapshot: &Snapshot) -> bool {
        snapshot.check
            == Check::Available {
                latest: "v0.0.20".into(),
            }
            && snapshot.work == Work::Idle
    }

    const SWITCH: &str = "ln -sfn versions/0.0.20 \"$ELSE_HOME/current\"";

    #[tokio::test]
    async fn an_update_installed_elsewhere_still_offers_restart() {
        let home = Home::new("exit 1");
        let major: u64 = env!("CARGO_PKG_VERSION")
            .split('.')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let version = format!("{}.0.0", major + 1);
        let tag = format!("v{version}");
        let target = PathBuf::from("versions").join(version);
        git(Path::new(&home.repo), &["tag", &tag]);
        std::fs::create_dir_all(home.home().join(&target).join("bin")).unwrap();
        std::fs::remove_file(home.home().join("current")).unwrap();
        std::os::unix::fs::symlink(target, home.home().join("current")).unwrap();
        let (commands, mut state) = home.start(None);
        let ready = until(&mut state, "external update detected", |snapshot| {
            matches!(snapshot.operation, Operation::Ready { .. })
        })
        .await;
        assert_eq!(ready.operation, Operation::Ready { version: tag });
        assert!(commands.install().is_err());
        assert!(
            home.calls().is_empty(),
            "checking must never launch the installer"
        );
    }

    #[tokio::test]
    async fn an_app_only_update_streams_its_output_then_restarts() {
        let home = Home::new(&format!(
            "case \"$1\" in\n\
             update)\n\
               printf '\\033[1m==> Build\\033[0m\\n10%%\\r100%%\\ncaf\\303'\n\
               sleep 0.5\n\
               printf '\\251 ok\\n'\n\
               echo 'a warning' >&2\n\
               {SWITCH} ;;\n\
             desktop) exec sleep 5 ;;\n\
             esac"
        ));
        let (commands, mut state) = home.start(None);
        assert!(commands.restart().is_err(), "nothing to restart yet");
        until(&mut state, "a stable release is offered", available).await;
        commands.install().unwrap();
        assert!(commands.install().is_err(), "one update at a time");
        let ready = until(&mut state, "installed", |s| {
            matches!(s.operation, Operation::Ready { .. } | Operation::Failed(_))
        })
        .await;
        assert_eq!(
            ready.operation,
            Operation::Ready {
                version: "v0.0.20".into()
            }
        );
        assert_eq!(ready.output, "==> Build\n100%\ncafé ok\na warning\n");
        until(&mut state, "current after the update", |s| {
            s.check
                == Check::Current {
                    latest: "v0.0.20".into(),
                }
        })
        .await;
        assert!(commands.install().is_err());

        commands.restart().unwrap();
        let restarted = until(&mut state, "relaunched", |s| {
            matches!(
                s.operation,
                Operation::Relaunched | Operation::RestartFailed(_)
            )
        })
        .await;
        assert_eq!(restarted.operation, Operation::Relaunched);
        let calls = home.calls();
        let home_dir = home.home();
        assert_eq!(
            calls,
            [
                format!(
                    "update --yes --version 0.0.20 cwd=/ home={} stdin=1 leader=yes",
                    home_dir.display()
                ),
                format!(
                    "desktop cwd=/ home={} stdin=1 leader=yes",
                    home_dir.display()
                ),
            ]
        );
        let mode = std::fs::metadata(home_dir.join(UPDATE_LOG))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "the update log is private");
    }

    #[tokio::test]
    async fn a_failed_update_is_not_restartable_and_can_be_retried() {
        let home = Home::new(&format!(
            "if [ -e \"$ELSE_HOME/fixed\" ]; then {SWITCH}; exit 0; fi\n\
             printf '\\n\\033[31merror:\\033[0m sudo was refused\\n' >&2\n\
             exit 1"
        ));
        let (commands, mut state) = home.start(None);
        until(&mut state, "offered", available).await;
        commands.install().unwrap();
        let failed = until(&mut state, "failed", |s| {
            matches!(s.operation, Operation::Failed(_))
        })
        .await;
        let Operation::Failed(why) = &failed.operation else {
            unreachable!()
        };
        assert!(
            why.contains("sudo was refused") && why.contains("elsebot update in a terminal"),
            "{why}"
        );
        assert_eq!(
            commands.restart(),
            Err("Install the update before restarting.".into())
        );

        std::fs::write(home.home().join("fixed"), "").unwrap();
        until(&mut state, "offered again", available).await;
        commands.install().unwrap();
        until(&mut state, "retried", |s| {
            matches!(s.operation, Operation::Ready { .. })
        })
        .await;
    }

    #[tokio::test]
    async fn a_clean_exit_that_did_not_switch_is_a_failure() {
        let home = Home::new("echo nothing happened");
        let (commands, mut state) = home.start(None);
        until(&mut state, "offered", available).await;
        commands.install().unwrap();
        let failed = until(&mut state, "failed", |s| {
            matches!(s.operation, Operation::Failed(_))
        })
        .await;
        assert!(
            format!("{:?}", failed.operation).contains("v0.0.9 is installed instead of v0.0.20")
        );
        assert!(commands.restart().is_err());
    }

    #[tokio::test]
    async fn a_relaunch_that_exits_at_once_is_reported() {
        let home = Home::new(&format!(
            "case \"$1\" in update) {SWITCH} ;; desktop) echo 'no display' >&2; exit 4 ;; esac"
        ));
        let (commands, mut state) = home.start(None);
        until(&mut state, "offered", available).await;
        commands.install().unwrap();
        until(&mut state, "ready", |s| {
            matches!(s.operation, Operation::Ready { .. })
        })
        .await;
        commands.restart().unwrap();
        let failed = until(&mut state, "restart failed", |s| {
            matches!(s.operation, Operation::RestartFailed(_))
        })
        .await;
        assert!(format!("{:?}", failed.operation).contains("stopped right away"));
        let log = std::fs::read_to_string(home.home().join(DESKTOP_LOG)).unwrap();
        assert!(log.contains("no display"), "{log}");
        assert!(
            commands.restart().is_ok(),
            "a failed restart can be retried"
        );
    }

    #[tokio::test]
    async fn sessions_without_an_installation_never_update() {
        let (commands, state) = start_with(
            Err("a development session".into()),
            Settings::default(),
            &tokio::runtime::Handle::current(),
        );
        assert_eq!(
            state.borrow().unavailable.as_deref(),
            Some("a development session")
        );
        for result in [
            commands.check(),
            commands.install(),
            commands.restart(),
            commands.refresh_work(),
        ] {
            assert_eq!(result, Err("a development session".into()));
        }
    }

    async fn server(answer: Arc<AtomicU16>) -> Origin {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = vec![0u8; 4096];
                let n = socket.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..n]);
                assert!(
                    request.starts_with("GET /v1/maintenance/update "),
                    "{request}"
                );
                assert!(
                    request.contains(&format!("authorization: Bearer {TOKEN}")),
                    "{request}"
                );
                let response = match answer.load(Ordering::SeqCst) {
                    405 => {
                        "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n".to_owned()
                    }
                    turns => {
                        let body = format!(
                            "{{\"active_turns\":{turns},\"running_commands\":0,\"queued_turns\":0,\"background_jobs\":0}}"
                        );
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                            body.len()
                        )
                    }
                };
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        origin
    }

    #[tokio::test]
    async fn working_bots_hold_the_update_until_they_finish() {
        let home = Home::new(SWITCH);
        let answer = Arc::new(AtomicU16::new(2));
        let origin = server(answer.clone()).await;
        let (commands, mut state) = home.start(Some(&origin));
        until(&mut state, "busy", |s| {
            s.work
                == Work::Busy {
                    turns: 2,
                    commands: 0,
                }
                && matches!(s.check, Check::Available { .. })
        })
        .await;
        assert_eq!(
            commands.install(),
            Err("Bots are working (2 active turns, 0 running commands). Update once they have finished.".into())
        );
        answer.store(0, Ordering::SeqCst);
        until(&mut state, "idle once work ends", available).await;
        commands.install().unwrap();
        until(&mut state, "installed", |s| {
            matches!(s.operation, Operation::Ready { .. })
        })
        .await;
        assert_eq!(home.calls().len(), 1);
    }

    #[tokio::test]
    async fn work_that_starts_before_the_install_cancels_it() {
        let home = Home::new(SWITCH);
        let answer = Arc::new(AtomicU16::new(0));
        let origin = server(answer.clone()).await;
        let (commands, mut state) = home.start(Some(&origin));
        until(&mut state, "offered", available).await;
        answer.store(1, Ordering::SeqCst);
        commands.install().unwrap();
        let after = until(&mut state, "refused", |s| {
            s.operation == Operation::Idle && matches!(s.work, Work::Busy { .. })
        })
        .await;
        assert!(after.install_refusal().unwrap().contains("1 active turn,"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(home.calls().is_empty(), "the installer never ran");
    }

    #[tokio::test]
    async fn unknown_work_prevents_a_server_update() {
        let home = Home::new(SWITCH);
        let answer = Arc::new(AtomicU16::new(405));
        let old = server(answer).await;
        let (commands, mut state) = home.start(Some(&old));
        let snapshot = until(&mut state, "old server", |s| {
            matches!(s.work, Work::Unavailable(_)) && matches!(s.check, Check::Available { .. })
        })
        .await;
        assert!(format!("{:?}", snapshot.work).contains("too old"));
        assert!(commands.install().unwrap_err().contains("too old"));

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let down = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        drop(listener);
        let (commands, mut state) = home.start(Some(&down));
        until(&mut state, "down", |s| {
            matches!(&s.work, Work::Unavailable(why) if why.contains("not answering"))
                && matches!(s.check, Check::Available { .. })
        })
        .await;
        assert!(commands.install().unwrap_err().contains("not answering"));
        assert!(home.calls().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_release_list_fails_the_check() {
        let home = Home::new(SWITCH);
        let settings = Settings {
            repo: home.dir.path().join("missing").to_str().unwrap().into(),
            ..home.settings()
        };
        let installation = Installation::for_tests(home.home(), None);
        let (commands, mut state) = start_with(
            Ok(installation),
            settings,
            &tokio::runtime::Handle::current(),
        );
        let failed = until(&mut state, "failed", |s| {
            matches!(s.check, Check::Failed(_))
        })
        .await;
        assert!(failed.checked_at.is_none());
        assert!(
            commands
                .install()
                .unwrap_err()
                .starts_with("Check for updates again first.")
        );
    }
}
