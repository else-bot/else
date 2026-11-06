//! Saved server identities, credentials, and their private atomic registry.
//!
//! The token never appears in URLs, logs, or error messages, and it is only
//! ever sent to the one configured origin.

use std::collections::HashSet;
use std::fmt;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use url::{Host, Url};
use uuid::Uuid;

const MAX_SERVERS: usize = 64;
const MAX_REGISTRY_BYTES: u64 = 256 * 1024;
const REGISTRY_SCHEMA: u32 = 1;

/// A validated API origin: `https://` for any host, `http://` only for
/// loopback, without credentials, path, query, or fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    serialized: String,
    secure: bool,
}

impl Origin {
    /// A bare address (`127.0.0.1:7878`, `else.example.com`) gets `http://`
    /// for loopback and `https://` for anything else.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(
                "Enter the server's address, such as https://else.example.com or 127.0.0.1:7878."
                    .into(),
            );
        }
        let invalid = |err: url::ParseError| format!("The server address is invalid: {err}.");
        let url = if raw.contains("://") {
            Url::parse(raw).map_err(invalid)?
        } else {
            let plain = Url::parse(&format!("http://{raw}")).map_err(invalid)?;
            if is_loopback(&plain) {
                plain
            } else {
                Url::parse(&format!("https://{raw}")).map_err(invalid)?
            }
        };
        let secure = match url.scheme() {
            "https" => true,
            "http" => false,
            _ => return Err("The server address must start with https:// or http://.".into()),
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err("The server address must not contain credentials.".into());
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err(
                "The server address must be an origin such as https://else.example.com, \
                 without a path, query, or fragment."
                    .into(),
            );
        }
        if url.host().is_none() {
            return Err("The server address has no host.".into());
        }
        if !secure && !is_loopback(&url) {
            return Err(
                "http:// is only for this machine: the owner token is not sent unencrypted \
                 across the network. Use https:// through a reverse proxy, or an SSH tunnel \
                 such as ssh -L 7878:127.0.0.1:7878 <server> and http://127.0.0.1:7878."
                    .into(),
            );
        }
        Ok(Self {
            serialized: url.origin().ascii_serialization(),
            secure,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.serialized
    }

    /// Whether the owner token sent here stays on this machine.
    pub fn is_loopback(&self) -> bool {
        Url::parse(&self.serialized).is_ok_and(|url| is_loopback(&url))
    }

    /// The WebSocket endpoint: `wss://` for an `https://` origin.
    pub fn ws_url(&self) -> String {
        let (_, rest) = self
            .serialized
            .split_once("://")
            .expect("serialized origin");
        let scheme = if self.secure { "wss" } else { "ws" };
        format!("{scheme}://{rest}/v1/ws")
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The owner bearer token. Its `Debug` output is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct OwnerToken(String);

impl fmt::Debug for OwnerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnerToken(<redacted>)")
    }
}

impl OwnerToken {
    pub fn parse(text: &str) -> Result<Self, String> {
        let token = text.trim();
        let valid = token.len() == 43
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if valid {
            Ok(Self(token.to_owned()))
        } else {
            Err(
                "The owner token is malformed: paste the 43-character value from the \
                 server's owner token file (~/.else/data/owner-token)."
                    .into(),
            )
        }
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// The server the desktop uses, as saved by the connection editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub origin: Origin,
    pub token: OwnerToken,
}

impl Profile {
    pub fn parse(origin: &str, token: &str) -> Result<Self, String> {
        Ok(Self {
            origin: Origin::parse(origin)?,
            token: OwnerToken::parse(token)?,
        })
    }
}

pub type ServerId = Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedServer {
    pub id: ServerId,
    pub name: String,
    pub profile: Profile,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerRegistry {
    pub servers: Vec<SavedServer>,
    pub selected: Option<ServerId>,
    pub legacy_server: Option<ServerId>,
}

impl ServerRegistry {
    pub fn find(&self, id: ServerId) -> Option<&SavedServer> {
        self.servers.iter().find(|server| server.id == id)
    }

    pub fn upsert(
        &mut self,
        profile: Profile,
        name: Option<String>,
        editing: Option<ServerId>,
    ) -> Result<ServerId, String> {
        let existing = self
            .servers
            .iter()
            .position(|server| server.profile.origin == profile.origin);
        let target = match editing {
            Some(id) => {
                let index = self
                    .servers
                    .iter()
                    .position(|server| server.id == id)
                    .ok_or("This saved server no longer exists.")?;
                if existing.is_some_and(|other| other != index) {
                    return Err("A connection to this server address is already saved.".into());
                }
                Some(index)
            }
            None => existing,
        };
        let name = name.map(|name| server_name(&name)).transpose()?;
        if let Some(index) = target {
            let server = &mut self.servers[index];
            server.profile = profile;
            if let Some(name) = name {
                server.name = name;
            }
            return Ok(server.id);
        }
        if self.servers.len() >= MAX_SERVERS {
            return Err(format!(
                "You can save up to {MAX_SERVERS} servers. Remove a connection before adding another."
            ));
        }
        let id = Uuid::new_v4();
        let name = name.unwrap_or_else(|| default_server_name(&profile.origin));
        self.servers.push(SavedServer { id, name, profile });
        Ok(id)
    }

    pub fn remove(&mut self, id: ServerId) -> Result<(), String> {
        if self.selected == Some(id) {
            return Err("Switch to another server before removing this connection.".into());
        }
        let index = self
            .servers
            .iter()
            .position(|server| server.id == id)
            .ok_or("This saved server no longer exists.")?;
        self.servers.remove(index);
        if self.legacy_server == Some(id) {
            self.legacy_server = None;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.servers.len() > MAX_SERVERS {
            return Err(format!(
                "The registry contains more than {MAX_SERVERS} servers."
            ));
        }
        let mut ids = HashSet::new();
        let mut origins = HashSet::new();
        for server in &self.servers {
            if server.id.is_nil() || !ids.insert(server.id) {
                return Err("Saved servers must have distinct, nonempty IDs.".into());
            }
            if !origins.insert(server.profile.origin.as_str()) {
                return Err("The same server address is saved more than once.".into());
            }
            server_name(&server.name)?;
        }
        for id in [self.selected, self.legacy_server].into_iter().flatten() {
            if !ids.contains(&id) {
                return Err("The registry refers to a server that is not saved.".into());
            }
        }
        Ok(())
    }
}

fn server_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err(
            "Give the server a name of 1 to 80 characters, without control characters.".into(),
        );
    }
    Ok(name.to_owned())
}

fn default_server_name(origin: &Origin) -> String {
    if origin.is_loopback() {
        return "This computer".into();
    }
    Url::parse(origin.as_str())
        .expect("validated origin")
        .host_str()
        .expect("origin has a host")
        .chars()
        .take(80)
        .collect()
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredProfile {
    origin: String,
    owner_token: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredServer {
    id: ServerId,
    name: String,
    origin: String,
    owner_token: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredRegistry {
    schema_version: u32,
    selected: Option<ServerId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_server: Option<ServerId>,
    servers: Vec<StoredServer>,
}

/// Where the profile is kept: `ELSE_DESKTOP_CONFIG_FILE` (an installation's
/// `config/desktop.json`), else `$XDG_CONFIG_HOME/else-bot/desktop.json`,
/// else `~/.config/else-bot/desktop.json`.
#[derive(Debug, Clone)]
pub struct ProfileStore {
    path: PathBuf,
}

impl ProfileStore {
    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    #[cfg(test)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let path = match (
            get("ELSE_DESKTOP_CONFIG_FILE"),
            get("XDG_CONFIG_HOME"),
            get("HOME"),
        ) {
            (Some(file), _, _) => PathBuf::from(file),
            (None, Some(xdg), _) if Path::new(&xdg).is_absolute() => {
                Path::new(&xdg).join("else-bot/desktop.json")
            }
            (None, _, Some(home)) => Path::new(&home).join(".config/else-bot/desktop.json"),
            _ => {
                return Err(
                    "There is no home directory for the desktop profile; set ELSE_DESKTOP_CONFIG_FILE."
                        .into(),
                );
            }
        };
        Ok(Self { path })
    }

    pub(crate) fn preferences_path(&self) -> PathBuf {
        self.path.with_extension("layout.json")
    }

    pub(crate) fn server_preferences_path(&self, id: ServerId) -> PathBuf {
        self.path.with_extension(format!("{id}.layout.json"))
    }

    fn registry_path(&self) -> PathBuf {
        self.path.with_extension("servers.json")
    }

    pub fn load(&self) -> Result<ServerRegistry, String> {
        let registry_path = self.registry_path();
        let canonical = read_private_limited(&registry_path, MAX_REGISTRY_BYTES)?;
        let migrated = canonical.is_some();
        let text = match canonical {
            Some(text) => text,
            None => match read_private_limited(&self.path, MAX_REGISTRY_BYTES)? {
                Some(text) => text,
                None => return Ok(ServerRegistry::default()),
            },
        };
        let shown = if migrated {
            registry_path.display()
        } else {
            self.path.display()
        };
        let invalid_json = |err: serde_json::Error| {
            format!(
                "The desktop profile {shown} is not valid (line {}, column {}).",
                err.line(),
                err.column()
            )
        };
        let value: serde_json::Value = serde_json::from_str(&text).map_err(invalid_json)?;
        let profile = |stored: StoredProfile| {
            Profile::parse(&stored.origin, &stored.owner_token)
                .map_err(|err| format!("The desktop profile {shown} is not valid. {err}"))
        };
        if value.get("schema_version").is_some() {
            let stored: StoredRegistry = serde_json::from_str(&text).map_err(invalid_json)?;
            if stored.schema_version != REGISTRY_SCHEMA {
                return Err(format!(
                    "The desktop profile {shown} uses unsupported schema {}. Update the desktop app before opening it.",
                    stored.schema_version
                ));
            }
            let registry = ServerRegistry {
                servers: stored
                    .servers
                    .into_iter()
                    .map(|stored| {
                        Ok(SavedServer {
                            id: stored.id,
                            name: server_name(&stored.name)?,
                            profile: profile(StoredProfile {
                                origin: stored.origin,
                                owner_token: stored.owner_token,
                            })?,
                        })
                    })
                    .collect::<Result<_, String>>()?,
                selected: stored.selected,
                legacy_server: stored.legacy_server,
            };
            registry
                .validate()
                .map_err(|err| format!("The desktop profile {shown} is not valid. {err}"))?;
            if !migrated {
                self.save(&registry)?;
            }
            return Ok(registry);
        }
        let stored: StoredProfile = serde_json::from_str(&text).map_err(invalid_json)?;
        let mut registry = ServerRegistry::default();
        let id = registry.upsert(profile(stored)?, None, None)?;
        registry.selected = Some(id);
        registry.legacy_server = Some(id);
        self.save(&registry)?;
        Ok(registry)
    }

    /// Replace the profile atomically: a private temporary file in the same
    /// private directory, synced, then renamed over the old one.
    pub fn save(&self, registry: &ServerRegistry) -> Result<(), String> {
        registry.validate()?;
        let json = serde_json::to_vec_pretty(&StoredRegistry {
            schema_version: REGISTRY_SCHEMA,
            selected: registry.selected,
            legacy_server: registry.legacy_server,
            servers: registry
                .servers
                .iter()
                .map(|server| StoredServer {
                    id: server.id,
                    name: server.name.clone(),
                    origin: server.profile.origin.as_str().to_owned(),
                    owner_token: server.profile.token.expose().to_owned(),
                })
                .collect(),
        })
        .expect("registry serializes");
        if json.len() as u64 + 1 > MAX_REGISTRY_BYTES {
            return Err("The saved server registry is too large.".into());
        }
        Self::at(self.registry_path()).write_json(&json)?;
        if let Some(selected) = registry.selected.and_then(|id| registry.find(id)) {
            let legacy = serde_json::to_vec_pretty(&StoredProfile {
                origin: selected.profile.origin.as_str().to_owned(),
                owner_token: selected.profile.token.expose().to_owned(),
            })
            .expect("profile serializes");
            self.write_json(&legacy)?;
        }
        Ok(())
    }

    fn write_json(&self, json: &[u8]) -> Result<(), String> {
        let shown = self.path.display();
        let fail = |err: std::io::Error| format!("Cannot save the desktop profile {shown}: {err}");
        let parent = match self.path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(fail)?;
        let meta = std::fs::metadata(parent).map_err(fail)?;
        if !meta.is_dir() || meta.uid() != current_uid() || meta.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "Cannot save the desktop profile: {} must be a directory only you can access (mode 0700).",
                parent.display()
            ));
        }
        let name = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("desktop.json");
        let tmp = parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()));
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)?;
            file.write_all(json)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.path)?;
            std::fs::File::open(parent)?.sync_all()
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        written.map_err(fail)
    }
}

pub(crate) fn current_uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

pub(crate) fn read_private(path: &Path) -> Result<Option<String>, String> {
    read_private_limited(path, 16 * 1024)
}

fn read_private_limited(path: &Path, limit: u64) -> Result<Option<String>, String> {
    let shown = path.display();
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {
            return Err(format!("{shown} is a symbolic link; refusing to read it."));
        }
        Err(err) => return Err(format!("Cannot read {shown}: {err}")),
    };
    let meta = file.metadata().map_err(|err| format!("{shown}: {err}"))?;
    if !meta.is_file() || meta.uid() != current_uid() || meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "{shown} must be a regular file owned by you with mode 0600."
        ));
    }
    let mut text = String::new();
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_string(&mut text)
        .map_err(|err| format!("{shown}: {err}"))?;
    if text.len() as u64 > limit {
        return Err(format!("{shown} exceeds the file size limit."));
    }
    Ok(Some(text))
}

pub(crate) fn read_token_file(path: &Path) -> Result<OwnerToken, String> {
    let text = read_private(path)?.ok_or_else(|| {
        format!(
            "Owner token file {} not found; run `else-api setup` or set ELSE_OWNER_TOKEN_FILE.",
            path.display()
        )
    })?;
    OwnerToken::parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn origins_normalize_schemes_and_websocket_urls() {
        for (raw, origin, ws) in [
            (
                "http://127.0.0.1:7878",
                "http://127.0.0.1:7878",
                "ws://127.0.0.1:7878/v1/ws",
            ),
            (
                "http://localhost:17001/",
                "http://localhost:17001",
                "ws://localhost:17001/v1/ws",
            ),
            (
                "http://[::1]:7878",
                "http://[::1]:7878",
                "ws://[::1]:7878/v1/ws",
            ),
            (
                "127.0.0.1:7878",
                "http://127.0.0.1:7878",
                "ws://127.0.0.1:7878/v1/ws",
            ),
            (
                "localhost:7878",
                "http://localhost:7878",
                "ws://localhost:7878/v1/ws",
            ),
            ("[::1]:7878", "http://[::1]:7878", "ws://[::1]:7878/v1/ws"),
            (
                "else.example.com",
                "https://else.example.com",
                "wss://else.example.com/v1/ws",
            ),
            (
                "10.0.0.2:8443",
                "https://10.0.0.2:8443",
                "wss://10.0.0.2:8443/v1/ws",
            ),
            (
                "https://Else.Example.com:443/",
                "https://else.example.com",
                "wss://else.example.com/v1/ws",
            ),
            (
                "https://203.0.113.9:7443",
                "https://203.0.113.9:7443",
                "wss://203.0.113.9:7443/v1/ws",
            ),
            (
                "  https://else.example.com  ",
                "https://else.example.com",
                "wss://else.example.com/v1/ws",
            ),
        ] {
            let parsed = Origin::parse(raw).unwrap_or_else(|err| panic!("{raw}: {err}"));
            assert_eq!(parsed.as_str(), origin, "{raw}");
            assert_eq!(parsed.ws_url(), ws, "{raw}");
        }
    }

    #[test]
    fn origins_refuse_plaintext_remote_credentials_and_paths() {
        for (bad, why) in [
            ("http://example.com:7878", "unencrypted"),
            ("http://10.0.0.2:7878", "unencrypted"),
            ("http://user:pass@127.0.0.1:7878", "credentials"),
            ("https://user@else.example.com", "credentials"),
            ("http://127.0.0.1:7878/v1", "path"),
            ("https://else.example.com/else", "path"),
            ("http://127.0.0.1:7878?token=x", "path"),
            ("https://else.example.com/#x", "path"),
            ("http://127.0.0.1:99999", "invalid"),
            ("https://exa mple.com", "invalid"),
            ("ftp://else.example.com", "https://"),
            ("", "Enter"),
        ] {
            let err = Origin::parse(bad).expect_err(bad);
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn profile_round_trips_privately_and_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config/desktop.json");
        let store = ProfileStore { path: path.clone() };
        assert_eq!(
            store.load().unwrap(),
            ServerRegistry::default(),
            "nothing saved yet"
        );
        let profile = Profile {
            origin: Origin::parse("else.example.com").unwrap(),
            token: OwnerToken::parse(&"A".repeat(43)).unwrap(),
        };
        let mut registry = ServerRegistry::default();
        let id = registry
            .upsert(profile.clone(), Some("Studio".into()), None)
            .unwrap();
        registry.selected = Some(id);
        store.save(&registry).unwrap();
        assert_eq!(store.load().unwrap(), registry);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let replaced = Profile {
            origin: Origin::parse("127.0.0.1:7878").unwrap(),
            ..profile.clone()
        };
        registry.upsert(replaced.clone(), None, Some(id)).unwrap();
        store.save(&registry).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.find(id).unwrap().profile, replaced);
        assert_eq!(loaded.find(id).unwrap().name, "Studio");
        assert_eq!(loaded.selected, Some(id));
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let mut names = names;
        names.sort();
        assert_eq!(
            names,
            ["desktop.json", "desktop.servers.json"],
            "no temporary file is left behind"
        );
        assert!(!format!("{profile:?}").contains(&"A".repeat(43)));
    }

    #[test]
    fn profile_refuses_shared_files_and_hides_malformed_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        let store = ProfileStore { path: path.clone() };
        let secret = "S".repeat(43);
        std::fs::write(
            &path,
            format!("{{\"origin\": 5, \"owner_token\": \"{secret}\""),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = store.load().unwrap_err();
        assert!(err.contains("not valid") && !err.contains(&secret), "{err}");
        std::fs::write(
            &path,
            format!("{{\"origin\": \"http://10.0.0.2\", \"owner_token\": \"{secret}\"}}"),
        )
        .unwrap();
        let err = store.load().unwrap_err();
        assert!(
            err.contains("unencrypted") && !err.contains(&secret),
            "{err}"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load().unwrap_err().contains("0600"));
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(
            ProfileStore { path: link }
                .load()
                .unwrap_err()
                .contains("symbolic link")
        );

        let shared = dir.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        let profile = Profile {
            origin: Origin::parse("127.0.0.1:7878").unwrap(),
            token: OwnerToken::parse(&secret).unwrap(),
        };
        let mut registry = ServerRegistry::default();
        registry.upsert(profile, None, None).unwrap();
        let err = ProfileStore {
            path: shared.join("desktop.json"),
        }
        .save(&registry)
        .unwrap_err();
        assert!(err.contains("0700"), "{err}");
        assert!(!shared.join("desktop.json").exists());
    }

    fn profile(address: &str) -> Profile {
        Profile::parse(address, &"A".repeat(43)).unwrap()
    }

    fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn legacy_profile_migrates_once_without_changing_credentials() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.path().join("desktop.json");
        let store = ProfileStore::at(path.clone());
        write_private(
            &path,
            format!(
                r#"{{"origin":"https://studio.example.com","owner_token":"{}"}}"#,
                "A".repeat(43)
            ),
        );
        let migrated = store.load().unwrap();
        let id = migrated.selected.unwrap();
        assert_eq!(migrated.legacy_server, Some(id));
        assert_eq!(
            migrated.find(id).unwrap().profile,
            profile("https://studio.example.com")
        );
        let bytes = std::fs::read(store.registry_path()).unwrap();
        let second = store.load().unwrap();
        assert_eq!(second, migrated);
        assert_eq!(std::fs::read(store.registry_path()).unwrap(), bytes);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["schema_version"],
            1
        );
    }

    #[test]
    fn older_desktop_reconnections_cannot_overwrite_saved_servers() {
        #[derive(serde::Deserialize)]
        struct LegacyProfile {
            origin: String,
            owner_token: String,
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = ProfileStore::at(dir.path().join("desktop.json"));
        let mut registry = ServerRegistry::default();
        let a = registry
            .upsert(
                profile("https://a.example.com"),
                Some("Studio".into()),
                None,
            )
            .unwrap();
        registry
            .upsert(profile("https://b.example.com"), Some("Build".into()), None)
            .unwrap();
        registry.selected = Some(a);
        store.save(&registry).unwrap();
        let legacy: LegacyProfile =
            serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
        assert_eq!(legacy.origin, "https://a.example.com");
        assert_eq!(legacy.owner_token, "A".repeat(43));
        write_private(
            store.path(),
            serde_json::to_vec(&StoredProfile {
                origin: "https://c.example.com".into(),
                owner_token: "B".repeat(43),
            })
            .unwrap(),
        );
        assert_eq!(store.load().unwrap(), registry);
    }

    #[test]
    fn registry_preserves_identity_and_names_and_rejects_duplicate_edits() {
        let mut registry = ServerRegistry::default();
        let first = registry
            .upsert(
                profile("https://studio.example.com"),
                Some(" Studio ".into()),
                None,
            )
            .unwrap();
        let second = registry
            .upsert(
                profile("https://home.example.com"),
                Some("Home".into()),
                None,
            )
            .unwrap();
        registry.selected = Some(first);
        let reconnected = registry
            .upsert(profile("https://STUDIO.example.com:443/"), None, None)
            .unwrap();
        assert_eq!(reconnected, first);
        assert_eq!(registry.servers.len(), 2);
        assert_eq!(registry.find(first).unwrap().name, "Studio");
        let before = registry.clone();
        assert!(
            registry
                .upsert(
                    profile("https://home.example.com"),
                    Some("Renamed".into()),
                    Some(first)
                )
                .is_err()
        );
        assert!(
            registry
                .upsert(
                    profile("https://third.example.com"),
                    Some(" ".into()),
                    Some(first)
                )
                .is_err()
        );
        assert_eq!(registry, before);
        assert!(registry.remove(first).is_err());
        registry.remove(second).unwrap();
        assert!(registry.find(second).is_none());
        assert_eq!(registry.selected, Some(first));
        assert_eq!(
            registry
                .upsert(
                    profile("https://new.example.com"),
                    Some("Renamed".into()),
                    Some(first)
                )
                .unwrap(),
            first
        );
        assert_eq!(registry.find(first).unwrap().name, "Renamed");
        assert_eq!(
            registry.find(first).unwrap().profile.origin.as_str(),
            "https://new.example.com"
        );
    }

    #[test]
    fn invalid_registries_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        let store = ProfileStore::at(path.clone());
        let id = Uuid::new_v4();
        let saved = serde_json::json!({
            "id": id,
            "name": "Studio",
            "origin": "https://studio.example.com",
            "owner_token": "A".repeat(43),
        });
        let different_id = {
            let mut value = saved.clone();
            value["id"] = serde_json::json!(Uuid::new_v4());
            value
        };
        for invalid in [
            serde_json::json!({"schema_version":2,"selected":id,"servers":[saved.clone()]}),
            serde_json::json!({"schema_version":1,"selected":Uuid::new_v4(),"servers":[saved.clone()]}),
            serde_json::json!({"schema_version":1,"selected":id,"servers":[saved.clone(),saved.clone()]}),
            serde_json::json!({"schema_version":1,"selected":id,"servers":[saved.clone(),different_id]}),
            serde_json::json!({"schema_version":1,"selected":id,"legacy_server":Uuid::new_v4(),"servers":[saved]}),
        ] {
            let bytes = serde_json::to_vec(&invalid).unwrap();
            write_private(&path, &bytes);
            assert!(store.load().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
        write_private(&path, vec![b' '; MAX_REGISTRY_BYTES as usize + 1]);
        assert!(store.load().unwrap_err().contains("size limit"));
    }

    #[test]
    fn registry_size_and_invalid_selection_are_checked_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = ProfileStore::at(dir.path().join("desktop.json"));
        let mut registry = ServerRegistry::default();
        for index in 0..MAX_SERVERS {
            registry
                .upsert(
                    profile(&format!("https://server-{index}.example.com")),
                    None,
                    None,
                )
                .unwrap();
        }
        let before = registry.clone();
        assert!(
            registry
                .upsert(profile("https://extra.example.com"), None, None)
                .is_err()
        );
        assert_eq!(registry, before);
        store.save(&registry).unwrap();
        assert_eq!(store.load().unwrap(), registry);
        let bytes = std::fs::read(store.registry_path()).unwrap();
        registry.selected = Some(Uuid::new_v4());
        assert!(store.save(&registry).is_err());
        assert_eq!(std::fs::read(store.registry_path()).unwrap(), bytes);
    }

    #[test]
    fn profile_location() {
        let path = |pairs| ProfileStore::from_env(env(pairs)).unwrap().path;
        assert_eq!(
            path(&[
                ("ELSE_DESKTOP_CONFIG_FILE", "/i/config/desktop.json"),
                ("HOME", "/h")
            ]),
            Path::new("/i/config/desktop.json")
        );
        assert_eq!(
            path(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")]),
            Path::new("/x/else-bot/desktop.json")
        );
        assert_eq!(
            path(&[("HOME", "/h")]),
            Path::new("/h/.config/else-bot/desktop.json")
        );
        assert!(ProfileStore::from_env(env(&[])).is_err());
    }

    #[test]
    fn token_file_rules() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owner-token");
        let token = "A".repeat(43);
        std::fs::write(&path, format!("{token}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token_file(&path).unwrap_err().contains("0600"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_token_file(&path).unwrap().expose(), token);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(
            read_token_file(&link)
                .unwrap_err()
                .contains("symbolic link")
        );
        assert!(
            read_token_file(&dir.path().join("missing"))
                .unwrap_err()
                .contains("else-api setup")
        );
        let fifo = dir.path().join("fifo");
        let c_path = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert!(read_token_file(&fifo).unwrap_err().contains("regular file"));
        std::fs::write(&path, "short").unwrap();
        let err = read_token_file(&path).unwrap_err();
        assert!(err.contains("malformed") && !err.contains("short"));
        assert_eq!(
            format!("{:?}", OwnerToken::parse(&token).unwrap()),
            "OwnerToken(<redacted>)"
        );
    }
}
