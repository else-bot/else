//! What this desktop process may know about the machine it runs on: where
//! the profile lives, which API on this machine the welcome offers, and,
//! for an installed server only, how to start it.
//!
//! An installation is found the same way from Finder, the Dock, and a
//! terminal: `ELSE_HOME`, else the installation recorded inside the macOS app
//! bundle, else `~/.else`. Development and test sessions name their server
//! explicitly and never see an installation.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::api::{Client, ClientError};
use crate::config::{self, Origin, OwnerToken, Profile, ProfileStore};

const EXPLICIT: &[&str] = &[
    "ELSE_DESKTOP_CONFIG_FILE",
    "ELSE_API_URL",
    "ELSE_OWNER_TOKEN",
    "ELSE_OWNER_TOKEN_FILE",
    "OWNER_TOKEN_FILE",
    "ELSE_DATA_DIR",
];

const INSTALLED_BIND: &str = "127.0.0.1:7878";

const EXPLICIT_UPDATES: &str =
    "This development or test session names its own server; update its checkout instead.";
const NOT_INSTALLED: &str =
    "No else.bot installation was found; install one with the installer to update from here.";

pub struct Launch {
    /// Where the profile lives; an error is shown on the welcome screen.
    pub store: Result<ProfileStore, String>,
    /// The server on this machine the welcome offers, or why there is none.
    pub local: Result<LocalServer, NoLocal>,
    /// The remote form's starting values, from an explicit launcher only.
    pub remote_seed: Option<RemoteSeed>,
    /// The installation this app may update and relaunch, or why there is
    /// none. Development and test sessions never have one.
    pub updates: Result<Installation, String>,
}

/// A trusted installation: its own `bin/elsebot` updates it, and its local
/// server, when it has one, answers whether bots are working.
#[derive(Debug, Clone)]
pub struct Installation {
    home: PathBuf,
    server: Option<LocalServer>,
}

impl Installation {
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The installed server; `None` for an app-only installation.
    pub fn server(&self) -> Option<&LocalServer> {
        self.server.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn for_tests(home: PathBuf, server: Option<(Origin, OwnerToken)>) -> Self {
        let server = server.map(|(origin, token)| LocalServer {
            origin,
            key: KeySource::Given(token),
            service: Some(Service { home: home.clone() }),
        });
        Self { home, server }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RemoteSeed {
    pub origin: String,
    pub token: Option<OwnerToken>,
}

/// Why the welcome has no local server to offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoLocal {
    /// A development or test session that names no server on this machine.
    Explicit,
    /// An installation with only the app.
    AppOnly,
    NotInstalled,
    /// An installation that is not safe to use; the text says why.
    Refused(String),
}

/// An API on this machine. Only an installed one may be started, and only
/// through the installation's own `bin/elsebot start`.
#[derive(Debug, Clone)]
pub struct LocalServer {
    origin: Origin,
    key: KeySource,
    service: Option<Service>,
}

#[derive(Debug, Clone)]
enum KeySource {
    File(PathBuf),
    Given(OwnerToken),
}

#[derive(Debug, Clone)]
struct Service {
    home: PathBuf,
}

/// What the local card says about the owner key before any attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    Loaded,
    Missing,
    Unreadable(String),
}

/// Progress of one local attempt, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Connecting,
    Starting,
    Waiting,
}

#[derive(Debug)]
pub enum Step {
    Phase(Phase),
    Done(Result<(), LocalError>),
}

/// How long a local attempt may wait.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// For `elsebot start` itself.
    pub start: Duration,
    /// From the start until the API answers.
    pub ready: Duration,
    /// Between readiness probes.
    pub poll: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            start: Duration::from_secs(30),
            ready: Duration::from_secs(60),
            poll: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalError {
    KeyMissing(PathBuf),
    KeyUnreadable(String),
    Rejected,
    NotRunning(String),
    StartFailed(String),
    NotReady(Duration),
    Unexpected(String),
}

pub fn this_machine() -> &'static str {
    if cfg!(target_os = "macos") {
        "this Mac"
    } else {
        "this machine"
    }
}

impl fmt::Display for LocalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let here = this_machine();
        match self {
            Self::KeyMissing(path) => write!(
                f,
                "No local owner key was found at {}. Finish installing the local server \
                 (elsebot update --both), then try again, or connect to a remote server.",
                path.display()
            ),
            Self::KeyUnreadable(why) => write!(f, "The local owner key cannot be used. {why}"),
            Self::Rejected => write!(
                f,
                "The server on {here} rejected the local owner key. Check which installation \
                 answers with: elsebot status"
            ),
            Self::NotRunning(origin) => write!(
                f,
                "Nothing answers at {origin}. Start that server, then try again."
            ),
            Self::StartFailed(why) => write!(
                f,
                "The local server could not be started: {why}. Check: elsebot status, elsebot logs"
            ),
            Self::NotReady(after) => write!(
                f,
                "The local server did not answer within {} s. Check: elsebot status, elsebot logs",
                after.as_secs()
            ),
            Self::Unexpected(why) => write!(f, "{why}"),
        }
    }
}

impl Launch {
    /// `get` reads the launch settings; `exe` is this executable, which
    /// inside the macOS app names its installation.
    pub fn discover(get: impl Fn(&str) -> Option<String>, exe: Option<&Path>) -> Self {
        if EXPLICIT.iter().any(|name| get(name).is_some()) {
            return Self::explicit(&get);
        }
        let home = get("ELSE_HOME")
            .map(PathBuf::from)
            .or_else(|| exe.and_then(bundle_home))
            .or_else(|| {
                let home = Path::new(&get("HOME")?).join(".else");
                home.join("config/elsebot.env").is_file().then_some(home)
            });
        match home {
            Some(home) if home.is_absolute() => {
                let local = installed(&home);
                Self {
                    store: Ok(ProfileStore::at(home.join("config/desktop.json"))),
                    updates: updatable(&home, &local),
                    local,
                    remote_seed: None,
                }
            }
            Some(home) => {
                let why = format!("ELSE_HOME ({}) must be an absolute path.", home.display());
                Self {
                    store: ProfileStore::from_env(&get),
                    local: Err(NoLocal::Refused(why.clone())),
                    remote_seed: None,
                    updates: Err(why),
                }
            }
            None => Self {
                store: ProfileStore::from_env(&get),
                local: Err(NoLocal::NotInstalled),
                remote_seed: None,
                updates: Err(NOT_INSTALLED.into()),
            },
        }
    }

    fn explicit(get: &impl Fn(&str) -> Option<String>) -> Self {
        let store = ProfileStore::from_env(get);
        let raw = get("ELSE_API_URL").unwrap_or_default();
        let loopback = Origin::parse(&raw).is_ok_and(|origin| origin.is_loopback());
        let key = match get("ELSE_OWNER_TOKEN") {
            Some(value) => OwnerToken::parse(&value).ok().map(KeySource::Given),
            None if loopback => token_file(get).map(KeySource::File),
            None => get("ELSE_OWNER_TOKEN_FILE")
                .or_else(|| get("OWNER_TOKEN_FILE"))
                .map(PathBuf::from)
                .map(KeySource::File),
        };
        if let (Ok(origin), Some(key)) = (Origin::parse(&raw), key.clone())
            && origin.is_loopback()
        {
            return Self {
                store,
                local: Ok(LocalServer {
                    origin,
                    key,
                    service: None,
                }),
                remote_seed: None,
                updates: Err(EXPLICIT_UPDATES.into()),
            };
        }
        let token = key.and_then(|key| match key {
            KeySource::Given(token) => Some(token),
            KeySource::File(path) => config::read_token_file(&path).ok(),
        });
        let seeded = !raw.is_empty() || token.is_some();
        Self {
            store,
            local: Err(NoLocal::Explicit),
            remote_seed: seeded.then_some(RemoteSeed { origin: raw, token }),
            updates: Err(EXPLICIT_UPDATES.into()),
        }
    }
}

fn token_file(get: &impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    if let Some(path) = get("ELSE_OWNER_TOKEN_FILE").or_else(|| get("OWNER_TOKEN_FILE")) {
        return Some(PathBuf::from(path));
    }
    let data = match (get("ELSE_DATA_DIR"), get("XDG_DATA_HOME"), get("HOME")) {
        (Some(dir), _, _) => PathBuf::from(dir),
        (None, Some(xdg), _) => Path::new(&xdg).join("else-bot"),
        (None, None, Some(home)) => Path::new(&home).join(".local/share/else-bot"),
        _ => return None,
    };
    Some(data.join("owner-token"))
}

fn bundle_home(exe: &Path) -> Option<PathBuf> {
    #[derive(serde::Deserialize)]
    struct Marker {
        home: PathBuf,
    }
    let marker = exe.parent()?.parent()?.join("Resources/installation.json");
    let text = std::fs::read(&marker).ok()?;
    if text.len() > 4096 {
        return None;
    }
    let marker: Marker = serde_json::from_slice(&text).ok()?;
    marker.home.is_absolute().then_some(marker.home)
}

pub(crate) fn component_directory(home: &Path, pointer: &str) -> PathBuf {
    let component = home.join(pointer);
    if component.symlink_metadata().is_ok() {
        component
    } else {
        home.join("current")
    }
}

fn installed(home: &Path) -> Result<LocalServer, NoLocal> {
    let server = component_directory(home, "server-current").join("bin/else-api");
    let app = component_directory(home, "app-current").join("bin/else-desktop");
    if !is_executable(&server) {
        return Err(if is_executable(&app) {
            NoLocal::AppOnly
        } else {
            NoLocal::NotInstalled
        });
    }
    let config = home.join("config");
    trusted(&config, Kind::PrivateDir)?;
    trusted(&config.join("elsebot.env"), Kind::File)?;
    trusted(&home.join("bin/elsebot"), Kind::Program)?;
    let settings = settings(&config.join(".env"))?;
    let bind = settings
        .get("ELSE_API_BIND")
        .map_or(INSTALLED_BIND, String::as_str);
    let origin = loopback_origin(bind)?;
    let absolute = |name: &str, default: PathBuf| match settings.get(name) {
        Some(path) if Path::new(path).is_absolute() => Ok(PathBuf::from(path)),
        Some(_) => Err(NoLocal::Refused(format!(
            "{name} in {} must be an absolute path.",
            config.join(".env").display()
        ))),
        None => Ok(default),
    };
    let data = absolute("ELSE_DATA_DIR", home.join("data"))?;
    let key = absolute("OWNER_TOKEN_FILE", data.join("owner-token"))?;
    Ok(LocalServer {
        origin,
        key: KeySource::File(key),
        service: Some(Service {
            home: home.to_owned(),
        }),
    })
}

fn updatable(home: &Path, local: &Result<LocalServer, NoLocal>) -> Result<Installation, String> {
    let server = match local {
        Ok(server) => Some(server.clone()),
        Err(NoLocal::AppOnly) => {
            let config = home.join("config");
            let checked = trusted(&config, Kind::PrivateDir)
                .and_then(|()| trusted(&config.join("elsebot.env"), Kind::File))
                .and_then(|()| trusted(&home.join("bin/elsebot"), Kind::Program));
            if let Err(NoLocal::Refused(why)) = checked {
                return Err(why);
            }
            None
        }
        Err(NoLocal::Refused(why)) => return Err(why.clone()),
        Err(NoLocal::NotInstalled | NoLocal::Explicit) => return Err(NOT_INSTALLED.into()),
    };
    Ok(Installation {
        home: home.to_owned(),
        server,
    })
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

enum Kind {
    PrivateDir,
    File,
    Program,
}

fn trusted(path: &Path, kind: Kind) -> Result<(), NoLocal> {
    let shown = path.display();
    let meta = std::fs::symlink_metadata(path)
        .map_err(|err| NoLocal::Refused(format!("Cannot inspect {shown}: {err}.")))?;
    let mode = meta.permissions().mode();
    let owned = meta.uid() == config::current_uid();
    let ok = match kind {
        Kind::PrivateDir => meta.is_dir() && owned && mode & 0o077 == 0,
        Kind::File => meta.is_file() && owned && mode & 0o022 == 0,
        Kind::Program => meta.is_file() && owned && mode & 0o022 == 0 && mode & 0o100 != 0,
    };
    if ok {
        return Ok(());
    }
    Err(NoLocal::Refused(match kind {
        Kind::PrivateDir => format!("{shown} must be a directory only you can access (mode 0700)."),
        Kind::File => format!("{shown} must be a file you own that others cannot change."),
        Kind::Program => {
            format!("{shown} must be an executable you own that others cannot change.")
        }
    }))
}

fn settings(path: &Path) -> Result<HashMap<String, String>, NoLocal> {
    let Some(text) = config::read_private(path).map_err(NoLocal::Refused)? else {
        return Ok(HashMap::new());
    };
    dotenvy::from_read_iter(text.as_bytes())
        .collect::<Result<_, _>>()
        .map_err(|_| {
            NoLocal::Refused(format!(
                "{} is not valid. Check the .env syntax.",
                path.display()
            ))
        })
}

fn loopback_origin(bind: &str) -> Result<Origin, NoLocal> {
    let mut addr: SocketAddr = bind.trim().parse().map_err(|_| {
        NoLocal::Refused(format!(
            "ELSE_API_BIND ({bind}) is not an address and port."
        ))
    })?;
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => addr.set_ip(Ipv4Addr::LOCALHOST.into()),
        IpAddr::V6(ip) if ip.is_unspecified() => addr.set_ip(Ipv6Addr::LOCALHOST.into()),
        ip if ip.is_loopback() => {}
        _ => {
            return Err(NoLocal::Refused(format!(
                "ELSE_API_BIND ({bind}) is not a loopback address; use Edit configuration to \
                 connect to it."
            )));
        }
    }
    Origin::parse(&format!("http://{addr}")).map_err(NoLocal::Refused)
}

impl LocalServer {
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Whether this is an installed server the desktop may start.
    pub fn manages_service(&self) -> bool {
        self.service.is_some()
    }

    /// Read the owner key now: setup may have created it since launch.
    pub fn key(&self) -> Result<OwnerToken, LocalError> {
        match &self.key {
            KeySource::Given(token) => Ok(token.clone()),
            KeySource::File(path) => match config::read_private(path) {
                Ok(Some(text)) => OwnerToken::parse(&text).map_err(LocalError::KeyUnreadable),
                Ok(None) => Err(LocalError::KeyMissing(path.clone())),
                Err(err) => Err(LocalError::KeyUnreadable(err)),
            },
        }
    }

    pub fn key_state(key: &Result<OwnerToken, LocalError>) -> KeyState {
        match key {
            Ok(_) => KeyState::Loaded,
            Err(LocalError::KeyMissing(_)) => KeyState::Missing,
            Err(err) => KeyState::Unreadable(err.to_string()),
        }
    }

    /// Whether a saved profile is exactly this installed server with its
    /// current key, so reopening may start it again.
    pub fn owns(&self, profile: &Profile) -> bool {
        self.service.is_some()
            && profile.origin == self.origin
            && self.key().is_ok_and(|key| key == profile.token)
    }

    /// Probe, start an installed server that does not answer, and wait for
    /// it. Dropping the receiver abandons the attempt: polling stops at
    /// once, and an `elsebot start` already running finishes within its own
    /// limit rather than being cut off in the middle of a service change.
    pub fn connect(
        &self,
        client: Arc<Client>,
        runtime: &tokio::runtime::Handle,
        limits: Limits,
    ) -> mpsc::Receiver<Step> {
        let (tx, rx) = mpsc::channel(4);
        let (origin, service) = (self.origin.as_str().to_owned(), self.service.clone());
        runtime.spawn(async move {
            if let Some(result) = attempt(&tx, client, origin, service, limits).await {
                let _ = tx.send(Step::Done(result)).await;
            }
        });
        rx
    }
}

enum Probe {
    Up,
    Down,
    Failed(LocalError),
}

async fn probe(client: &Arc<Client>, limit: Duration) -> Probe {
    match tokio::time::timeout(limit, client.clone().capabilities()).await {
        Ok(Ok(_)) => Probe::Up,
        Ok(Err(ClientError::Unauthorized)) => Probe::Failed(LocalError::Rejected),
        Ok(Err(ClientError::Unreachable { .. })) | Err(_) => Probe::Down,
        Ok(Err(err)) => Probe::Failed(LocalError::Unexpected(err.to_string())),
    }
}

async fn attempt(
    tx: &mpsc::Sender<Step>,
    client: Arc<Client>,
    origin: String,
    service: Option<Service>,
    limits: Limits,
) -> Option<Result<(), LocalError>> {
    tx.send(Step::Phase(Phase::Connecting)).await.ok()?;
    match probe(&client, limits.ready).await {
        Probe::Up => return Some(Ok(())),
        Probe::Failed(err) => return Some(Err(err)),
        Probe::Down => {}
    }
    let Some(service) = service else {
        return Some(Err(LocalError::NotRunning(origin)));
    };
    tx.send(Step::Phase(Phase::Starting)).await.ok()?;
    let deadline = Instant::now() + limits.ready;
    if let Err(err) = service.start(limits.start).await {
        return Some(Err(err));
    }
    tx.send(Step::Phase(Phase::Waiting)).await.ok()?;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match probe(&client, left).await {
            Probe::Up => return Some(Ok(())),
            Probe::Failed(err) => return Some(Err(err)),
            Probe::Down if left.is_zero() => return Some(Err(LocalError::NotReady(limits.ready))),
            Probe::Down => {}
        }
        tokio::select! {
            _ = tokio::time::sleep(limits.poll.min(left)) => {}
            _ = tx.closed() => return None,
        }
    }
}

impl Service {
    async fn start(&self, limit: Duration) -> Result<(), LocalError> {
        let program = self.home.join("bin/elsebot");
        let child = tokio::process::Command::new(&program)
            .arg("start")
            .current_dir("/")
            .env_clear()
            .envs(service_env(std::env::vars_os(), &self.home))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()
            .map_err(|err| {
                LocalError::StartFailed(format!("cannot run {}: {err}", program.display()))
            })?;
        let mut child = Starter {
            group: child.id().expect("spawned child has a process id") as libc::pid_t,
            child,
        };
        match tokio::time::timeout(limit, child.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(LocalError::StartFailed(format!("elsebot start {status}"))),
            Ok(Err(err)) => Err(LocalError::StartFailed(err.to_string())),
            Err(_) => {
                child.stop();
                let _ = child.child.wait().await;
                Err(LocalError::StartFailed(format!(
                    "elsebot start did not finish within {} s",
                    limit.as_secs()
                )))
            }
        }
    }
}

struct Starter {
    child: tokio::process::Child,
    group: libc::pid_t,
}

impl Starter {
    fn stop(&mut self) {
        if self.child.id().is_some() {
            // SAFETY: the child was spawned as the leader of this process group.
            unsafe { libc::kill(-self.group, libc::SIGKILL) };
        }
    }
}

impl Drop for Starter {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) fn service_env(
    vars: impl Iterator<Item = (OsString, OsString)>,
    home: &Path,
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<_> = vars
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            !(name.starts_with("ELSE_")
                || name.starts_with("SECRET_KEY")
                || name == "DATABASE_URL"
                || name == "OWNER_TOKEN_FILE")
        })
        .collect();
    env.push(("ELSE_HOME".into(), home.into()));
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    const TOKEN: &str = "KKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKKK";
    const CAPABILITIES: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 50\r\n\r\n{\"providers\":[],\"limits\":{\"max_text_bytes\":32768}}";
    const REJECTED: &str = "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";

    fn env(pairs: &[(&str, String)]) -> impl Fn(&str) -> Option<String> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn write(path: &Path, text: &str, mode: u32) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn installation(home: &Path, server: bool, script: &str) {
        write(&home.join("current/bin/else-desktop"), "#!/bin/sh\n", 0o755);
        if server {
            write(&home.join("current/bin/else-api"), "#!/bin/sh\n", 0o755);
            write(
                &home.join("config/.env"),
                "ELSE_API_BIND=127.0.0.1:7878\n",
                0o600,
            );
        }
        write(
            &home.join("config/elsebot.env"),
            "ELSEBOT_MANAGER=process\n",
            0o644,
        );
        std::fs::set_permissions(home.join("config"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        write(&home.join("bin/elsebot"), script, 0o755);
    }

    fn store_path(launch: &Launch) -> PathBuf {
        launch.store.as_ref().unwrap().path().to_owned()
    }

    fn server(launch: Launch) -> LocalServer {
        launch
            .local
            .unwrap_or_else(|why| panic!("no local server: {why:?}"))
    }

    #[test]
    fn explicit_settings_hide_the_installation() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_str().unwrap().to_owned();
        installation(&dir.path().join(".else"), true, "#!/bin/sh\n");
        let installed = Launch::discover(env(&[("HOME", home.clone())]), None);
        assert_eq!(
            store_path(&installed),
            dir.path().join(".else/config/desktop.json")
        );
        assert!(server(installed).manages_service());
        for name in EXPLICIT {
            let launch = Launch::discover(
                env(&[
                    ("HOME", home.clone()),
                    ("ELSE_HOME", format!("{home}/.else")),
                    (name, "/explicit/value".into()),
                ]),
                None,
            );
            assert!(
                !store_path(&launch).starts_with(dir.path().join(".else")),
                "{name}: {:?}",
                store_path(&launch)
            );
            assert!(
                launch.updates.is_err(),
                "{name}: never updates an installation"
            );
            assert_eq!(launch.local.unwrap_err(), NoLocal::Explicit, "{name}");
        }
    }

    #[test]
    fn component_discovery_keeps_the_server_separate_from_the_app() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        installation(&home, true, "#!/bin/sh\n");
        std::fs::rename(home.join("current"), home.join("server-release")).unwrap();
        symlink("server-release", home.join("server-current")).unwrap();
        write(
            &home.join("app-release/bin/else-desktop"),
            "#!/bin/sh\n",
            0o755,
        );
        symlink("app-release", home.join("app-current")).unwrap();
        symlink("app-release", home.join("current")).unwrap();
        let launch = Launch::discover(env(&[("ELSE_HOME", home.to_str().unwrap().into())]), None);
        assert!(launch.local.unwrap().manages_service());
        assert!(launch.updates.unwrap().server().is_some());
        std::fs::remove_file(home.join("server-current")).unwrap();
        symlink("missing", home.join("server-current")).unwrap();
        assert!(matches!(installed(&home), Err(NoLocal::AppOnly)));
    }

    #[test]
    fn updates_need_a_trusted_installation() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let get = env(&[("ELSE_HOME", home.to_str().unwrap().into())]);
        installation(&home, false, "#!/bin/sh\n");
        let app_only = Launch::discover(&get, None).updates.unwrap();
        assert_eq!(app_only.home(), home);
        assert!(app_only.server().is_none());
        std::fs::set_permissions(
            home.join("bin/elsebot"),
            std::fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let refused = Launch::discover(&get, None).updates.unwrap_err();
        assert!(refused.contains("executable you own"), "{refused}");

        installation(&home, true, "#!/bin/sh\n");
        let server = Launch::discover(&get, None).updates.unwrap();
        assert!(server.server().unwrap().manages_service());
        let absent = dir.path().join("absent");
        let none = Launch::discover(env(&[("ELSE_HOME", absent.to_str().unwrap().into())]), None);
        assert!(none.updates.is_err());
    }

    #[test]
    fn explicit_loopback_seed_is_connect_only_and_remote_seed_fills_the_form() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("owner-token");
        write(&key, TOKEN, 0o600);
        let file = key.to_str().unwrap().to_owned();
        let local = server(Launch::discover(
            env(&[
                ("ELSE_API_URL", "http://127.0.0.1:17001".into()),
                ("ELSE_OWNER_TOKEN_FILE", file.clone()),
                ("ELSE_DESKTOP_CONFIG_FILE", "/d/desktop.json".into()),
            ]),
            None,
        ));
        assert!(
            !local.manages_service(),
            "a launcher's server is never started"
        );
        assert_eq!(local.origin().as_str(), "http://127.0.0.1:17001");
        assert_eq!(local.key().unwrap().expose(), TOKEN);

        let remote = Launch::discover(
            env(&[
                ("ELSE_API_URL", "https://else.example.com".into()),
                ("ELSE_OWNER_TOKEN_FILE", file),
            ]),
            None,
        );
        assert_eq!(remote.local.unwrap_err(), NoLocal::Explicit);
        let seed = remote.remote_seed.unwrap();
        assert_eq!(seed.origin, "https://else.example.com");
        assert_eq!(seed.token.unwrap().expose(), TOKEN);

        let given = Launch::discover(env(&[("ELSE_OWNER_TOKEN", TOKEN.into())]), None);
        assert_eq!(given.remote_seed.unwrap().token.unwrap().expose(), TOKEN);
        let nothing = Launch::discover(env(&[("ELSE_DESKTOP_CONFIG_FILE", "/d".into())]), None);
        assert!(nothing.remote_seed.is_none());
    }

    #[test]
    fn remote_addresses_never_borrow_fallback_local_keys() {
        let dir = tempfile::tempdir().unwrap();
        for relative in [".local/share/else-bot/owner-token", "data/owner-token"] {
            write(&dir.path().join(relative), TOKEN, 0o600);
        }
        for extra in [None, Some(("ELSE_DATA_DIR", dir.path().join("data")))] {
            let mut values = vec![
                ("HOME", dir.path().to_str().unwrap().into()),
                ("ELSE_API_URL", "https://remote.example.com".into()),
            ];
            if let Some((name, path)) = extra {
                values.push((name, path.to_str().unwrap().into()));
            }
            let launch = Launch::discover(env(&values), None);
            assert!(launch.remote_seed.unwrap().token.is_none());
        }
    }

    #[test]
    fn a_development_data_directory_does_not_hide_the_saved_standalone_profile() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".else/data")).unwrap();
        let saved = dir.path().join(".config/else-bot/desktop.json");
        let profile = Profile::parse("https://remote.example.com", TOKEN).unwrap();
        let mut registry = crate::config::ServerRegistry::default();
        let id = registry.upsert(profile.clone(), None, None).unwrap();
        registry.selected = Some(id);
        ProfileStore::at(saved.clone()).save(&registry).unwrap();
        let launch = Launch::discover(env(&[("HOME", dir.path().to_str().unwrap().into())]), None);
        assert_eq!(store_path(&launch), saved);
        assert_eq!(launch.store.unwrap().load().unwrap(), registry);
    }

    #[test]
    fn invalid_settings_never_expose_secret_bearing_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        let secret = "private-owner-key";
        write(&path, &format!("SECRET_KEY=\"{secret}\n"), 0o600);
        let Err(NoLocal::Refused(message)) = settings(&path) else {
            panic!("malformed settings were accepted");
        };
        assert!(!message.contains(secret));
        assert!(message.contains(".env syntax"));
    }

    #[test]
    fn else_home_then_the_app_bundle_then_the_default_home() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join(".else");
        let chosen = dir.path().join("chosen");
        let bundled = dir.path().join("bundled");
        for home in [&default, &chosen, &bundled] {
            installation(home, true, "#!/bin/sh\n");
        }
        let exe = dir
            .path()
            .join("Applications/else.bot.app/Contents/MacOS/else-desktop");
        write(
            &exe.parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("Resources/installation.json"),
            &serde_json::json!({ "home": bundled }).to_string(),
            0o644,
        );
        let home = dir.path().to_str().unwrap().to_owned();
        let at = |pairs: &[(&str, String)], exe: Option<&Path>| {
            store_path(&Launch::discover(env(pairs), exe))
        };
        let profile = |home: &Path| home.join("config/desktop.json");
        assert_eq!(
            at(
                &[
                    ("HOME", home.clone()),
                    ("ELSE_HOME", chosen.to_str().unwrap().into())
                ],
                Some(&exe)
            ),
            profile(&chosen)
        );
        assert_eq!(at(&[("HOME", home.clone())], Some(&exe)), profile(&bundled));
        assert_eq!(at(&[("HOME", home.clone())], None), profile(&default));
        let elsewhere = dir.path().join("plain/else-desktop");
        assert_eq!(
            at(&[("HOME", home.clone())], Some(&elsewhere)),
            profile(&default)
        );

        std::fs::remove_dir_all(&default).unwrap();
        let none = Launch::discover(env(&[("HOME", home.clone())]), None);
        assert_eq!(none.local.as_ref().unwrap_err(), &NoLocal::NotInstalled);
        assert_eq!(
            store_path(&none),
            dir.path().join(".config/else-bot/desktop.json")
        );
        let relative = Launch::discover(env(&[("HOME", home), ("ELSE_HOME", "rel".into())]), None);
        assert!(matches!(relative.local, Err(NoLocal::Refused(_))));
    }

    #[test]
    fn app_only_and_missing_installations_offer_no_local_server() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        installation(&home, false, "#!/bin/sh\n");
        let launch = Launch::discover(env(&[("ELSE_HOME", home.to_str().unwrap().into())]), None);
        assert_eq!(launch.local.as_ref().unwrap_err(), &NoLocal::AppOnly);
        assert_eq!(store_path(&launch), home.join("config/desktop.json"));
        let absent = dir.path().join("absent");
        let launch = Launch::discover(env(&[("ELSE_HOME", absent.to_str().unwrap().into())]), None);
        assert_eq!(launch.local.unwrap_err(), NoLocal::NotInstalled);
    }

    #[test]
    fn installed_settings_choose_the_address_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        installation(&home, true, "#!/bin/sh\n");
        let discover = |dotenv: &str| {
            write(&home.join("config/.env"), dotenv, 0o600);
            Launch::discover(env(&[("ELSE_HOME", home.to_str().unwrap().into())]), None).local
        };
        let local = discover("# comment\nELSE_DATA_DIR=/custom/data\n").unwrap();
        assert_eq!(local.origin().as_str(), "http://127.0.0.1:7878");
        assert_eq!(
            local.key(),
            Err(LocalError::KeyMissing("/custom/data/owner-token".into()))
        );
        let local = discover("ELSE_API_BIND=0.0.0.0:17878\nOWNER_TOKEN_FILE=/t/key\n").unwrap();
        assert_eq!(local.origin().as_str(), "http://127.0.0.1:17878");
        assert_eq!(local.key(), Err(LocalError::KeyMissing("/t/key".into())));
        assert_eq!(
            discover("ELSE_API_BIND=[::]:7\n")
                .unwrap()
                .origin()
                .as_str(),
            "http://[::1]:7"
        );
        for (dotenv, why) in [
            ("ELSE_API_BIND=192.168.1.2:7878\n", "not a loopback"),
            ("ELSE_API_BIND=else.example.com\n", "not an address"),
            ("ELSE_DATA_DIR=relative\n", "absolute"),
        ] {
            match discover(dotenv) {
                Err(NoLocal::Refused(err)) => assert!(err.contains(why), "{dotenv}: {err}"),
                other => panic!("{dotenv}: {other:?}"),
            }
        }
    }

    #[test]
    fn untrusted_installations_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let get = env(&[("ELSE_HOME", home.to_str().unwrap().into())]);
        let mode = |path: &str, mode: u32| {
            std::fs::set_permissions(home.join(path), std::fs::Permissions::from_mode(mode))
                .unwrap()
        };
        let refused = |why: &str| match Launch::discover(&get, None).local {
            Err(NoLocal::Refused(err)) => assert!(err.contains(why), "{why}: {err}"),
            other => panic!("{why}: {other:?}"),
        };
        installation(&home, true, "#!/bin/sh\n");
        mode("config", 0o755);
        refused("mode 0700");
        mode("config", 0o700);
        mode("bin/elsebot", 0o775);
        refused("executable you own");
        mode("bin/elsebot", 0o755);
        mode("config/.env", 0o644);
        refused("0600");
        mode("config/.env", 0o600);
        let marker = home.join("config/elsebot.env");
        std::fs::rename(&marker, home.join("elsebot.env")).unwrap();
        symlink(home.join("elsebot.env"), &marker).unwrap();
        refused("file you own");
    }

    #[test]
    fn key_state_follows_the_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner-token");
        let local = LocalServer {
            origin: Origin::parse("127.0.0.1:7878").unwrap(),
            key: KeySource::File(path.clone()),
            service: None,
        };
        assert_eq!(LocalServer::key_state(&local.key()), KeyState::Missing);
        write(&path, TOKEN, 0o644);
        assert!(
            matches!(LocalServer::key_state(&local.key()), KeyState::Unreadable(e) if e.contains("0600"))
        );
        write(&path, "not-a-key", 0o600);
        let state = LocalServer::key_state(&local.key());
        assert!(
            matches!(&state, KeyState::Unreadable(e) if !e.contains("not-a-key")),
            "{state:?}"
        );
        write(&path, TOKEN, 0o600);
        assert_eq!(LocalServer::key_state(&local.key()), KeyState::Loaded);
    }

    #[test]
    fn the_service_environment_drops_api_overrides() {
        let vars = [
            ("HOME", "/Users/me"),
            ("PATH", "/usr/bin"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/bus"),
            ("ELSE_DATA_DIR", "/checkout/data"),
            ("ELSE_HOME", "/other"),
            ("DATABASE_URL", "sqlite:///x"),
            ("OWNER_TOKEN_FILE", "/x"),
            ("SECRET_KEY", "s"),
            ("SECRET_KEY_FILE", "/s"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let env = service_env(vars.into_iter(), Path::new("/i"));
        let names: Vec<_> = env
            .iter()
            .map(|(k, v)| format!("{}={}", k.to_string_lossy(), v.to_string_lossy()))
            .collect();
        assert_eq!(
            names,
            [
                "HOME=/Users/me",
                "PATH=/usr/bin",
                "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/bus",
                "ELSE_HOME=/i"
            ]
        );
    }

    async fn free_origin() -> (u16, Origin) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (
            port,
            Origin::parse(&format!("http://127.0.0.1:{port}")).unwrap(),
        )
    }

    async fn answer(port: u16, response: &'static str) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .unwrap();
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(response.as_bytes()).await;
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        home: PathBuf,
        local: LocalServer,
        client: Arc<Client>,
    }

    fn fixture(origin: &Origin, script: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        installation(
            &home,
            true,
            &format!(
                "#!/bin/sh\necho \"$* ELSE_HOME=$ELSE_HOME HOME=${{HOME:+set}}\" >>\"$ELSE_HOME/calls\"\n{script}\n"
            ),
        );
        let local = LocalServer {
            origin: origin.clone(),
            key: KeySource::Given(OwnerToken::parse(TOKEN).unwrap()),
            service: Some(Service { home: home.clone() }),
        };
        let client = Arc::new(
            Client::new(
                origin.clone(),
                &local.key().unwrap(),
                tokio::runtime::Handle::current(),
            )
            .unwrap(),
        );
        Fixture {
            _dir: dir,
            home,
            local,
            client,
        }
    }

    impl Fixture {
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.home.join("calls"))
                .map(|text| text.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        }

        async fn run(&self, limits: Limits) -> (Vec<Phase>, Result<(), LocalError>) {
            let mut steps = self.local.connect(
                self.client.clone(),
                &tokio::runtime::Handle::current(),
                limits,
            );
            let mut phases = Vec::new();
            loop {
                match steps.recv().await.expect("the attempt ends with Done") {
                    Step::Phase(phase) => phases.push(phase),
                    Step::Done(result) => return (phases, result),
                }
            }
        }
    }

    fn quick() -> Limits {
        Limits {
            start: Duration::from_secs(5),
            ready: Duration::from_secs(5),
            poll: Duration::from_millis(50),
        }
    }

    #[tokio::test]
    async fn a_running_server_is_used_without_starting_it() {
        let (port, origin) = free_origin().await;
        let server = tokio::spawn(answer(port, CAPABILITIES));
        let fixture = fixture(&origin, "exit 0");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            fixture.run(quick()).await,
            (vec![Phase::Connecting], Ok(()))
        );
        assert!(fixture.calls().is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn a_rejected_key_or_bad_answer_never_starts_the_server() {
        for (response, expected) in [
            (REJECTED, LocalError::Rejected),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nno",
                LocalError::Unexpected(String::new()),
            ),
        ] {
            let (port, origin) = free_origin().await;
            let server = tokio::spawn(answer(port, response));
            let fixture = fixture(&origin, "exit 0");
            tokio::time::sleep(Duration::from_millis(50)).await;
            let (phases, result) = fixture.run(quick()).await;
            assert_eq!(phases, [Phase::Connecting]);
            match (result.unwrap_err(), &expected) {
                (LocalError::Unexpected(_), LocalError::Unexpected(_)) => {}
                (got, want) => assert_eq!(&got, want),
            }
            assert!(fixture.calls().is_empty(), "never started");
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_stopped_server_is_started_and_awaited() {
        let (port, origin) = free_origin().await;
        let fixture = fixture(&origin, "touch \"$ELSE_HOME/started\"");
        let started = fixture.home.join("started");
        let server = tokio::spawn(async move {
            while !started.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            answer(port, CAPABILITIES).await
        });
        let (phases, result) = fixture.run(quick()).await;
        assert_eq!(result, Ok(()));
        assert_eq!(phases, [Phase::Connecting, Phase::Starting, Phase::Waiting]);
        assert_eq!(
            fixture.calls(),
            [format!(
                "start ELSE_HOME={} HOME=set",
                fixture.home.display()
            )]
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_connect_only_server_that_is_down_is_not_started() {
        let (_, origin) = free_origin().await;
        let fixture = fixture(&origin, "exit 0");
        let local = LocalServer {
            service: None,
            ..fixture.local.clone()
        };
        let mut steps = local.connect(
            fixture.client.clone(),
            &tokio::runtime::Handle::current(),
            quick(),
        );
        assert!(matches!(
            steps.recv().await,
            Some(Step::Phase(Phase::Connecting))
        ));
        assert!(matches!(
            steps.recv().await,
            Some(Step::Done(Err(LocalError::NotRunning(_))))
        ));
        assert!(fixture.calls().is_empty());
    }

    #[tokio::test]
    async fn start_failures_hide_the_helpers_output() {
        let (_, origin) = free_origin().await;
        let fixture = fixture(&origin, "echo \"secret $0\" >&2; exit 3");
        let (phases, result) = fixture.run(quick()).await;
        assert_eq!(phases, [Phase::Connecting, Phase::Starting]);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("could not be started") && !err.contains("secret"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_hung_start_is_killed_and_reaped() {
        let (_, origin) = free_origin().await;
        let fixture = fixture(&origin, "sleep 30 & echo $! >\"$ELSE_HOME/pid\"; wait");
        let limits = Limits {
            start: Duration::from_millis(400),
            ..quick()
        };
        let (_, result) = fixture.run(limits).await;
        assert!(
            matches!(&result, Err(LocalError::StartFailed(why)) if why.contains("did not finish")),
            "{result:?}"
        );
        let pid: libc::pid_t = std::fs::read_to_string(fixture.home.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut gone = false;
        for _ in 0..50 {
            // SAFETY: signal 0 only checks for existence.
            if unsafe { libc::kill(pid, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the helper's process group was killed");
    }

    #[tokio::test]
    async fn cancelling_the_start_owner_ends_its_helper_group() {
        let (_, origin) = free_origin().await;
        let fixture = fixture(&origin, "sleep 30 & echo $! >\"$ELSE_HOME/pid\"; wait");
        let service = fixture.local.service.clone().unwrap();
        let task = tokio::spawn(async move { service.start(Duration::from_secs(30)).await });
        let pid_file = fixture.home.join("pid");
        tokio::time::timeout(Duration::from_secs(2), async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let pid: libc::pid_t = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), async {
            // SAFETY: signal zero only checks whether the process exists.
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled startup left a helper running");
    }

    #[tokio::test]
    async fn a_server_that_never_answers_times_out() {
        let (_, origin) = free_origin().await;
        let fixture = fixture(&origin, "exit 0");
        let limits = Limits {
            ready: Duration::from_millis(400),
            ..quick()
        };
        let started = std::time::Instant::now();
        let (phases, result) = fixture.run(limits).await;
        assert_eq!(phases, [Phase::Connecting, Phase::Starting, Phase::Waiting]);
        assert_eq!(
            result,
            Err(LocalError::NotReady(Duration::from_millis(400)))
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn dropping_the_receiver_stops_the_wait() {
        let (_, origin) = free_origin().await;
        let fixture = fixture(&origin, "exit 0");
        let (tx, mut rx) = mpsc::channel(4);
        let limits = Limits {
            ready: Duration::from_secs(30),
            ..quick()
        };
        let task = tokio::spawn({
            let (client, origin) = (fixture.client.clone(), origin.as_str().to_owned());
            let service = fixture.local.service.clone();
            async move { attempt(&tx, client, origin, service, limits).await }
        });
        while !matches!(rx.recv().await, Some(Step::Phase(Phase::Waiting))) {}
        drop(rx);
        let ended = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(matches!(ended, Ok(Ok(None))), "{ended:?}");
    }
}
