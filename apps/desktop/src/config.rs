//! Desktop configuration: the saved server profile (API origin and owner
//! token) and the private files it is read from.
//!
//! The token never appears in URLs, logs, or error messages, and it is only
//! ever sent to the one configured origin.

use std::fmt;
use std::fs::{DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use url::{Host, Url};

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

#[derive(serde::Serialize, serde::Deserialize)]
struct StoredProfile {
    origin: String,
    owner_token: String,
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

    /// `Ok(None)` when nothing was saved yet.
    pub fn load(&self) -> Result<Option<Profile>, String> {
        let shown = self.path.display();
        let Some(text) = read_private(&self.path)? else {
            return Ok(None);
        };
        let stored: StoredProfile = serde_json::from_str(&text).map_err(|err| {
            format!(
                "The desktop profile {shown} is not valid (line {}, column {}).",
                err.line(),
                err.column()
            )
        })?;
        let origin = Origin::parse(&stored.origin)
            .map_err(|err| format!("The desktop profile {shown} is not valid. {err}"))?;
        let token = OwnerToken::parse(&stored.owner_token)
            .map_err(|err| format!("The desktop profile {shown} is not valid. {err}"))?;
        Ok(Some(Profile { origin, token }))
    }

    /// Replace the profile atomically: a private temporary file in the same
    /// private directory, synced, then renamed over the old one.
    pub fn save(&self, profile: &Profile) -> Result<(), String> {
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
        let json = serde_json::to_vec_pretty(&StoredProfile {
            origin: profile.origin.as_str().to_owned(),
            owner_token: profile.token.expose().to_owned(),
        })
        .expect("profile serializes");
        let written = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)?;
            file.write_all(&json)?;
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
        .take(16 * 1024)
        .read_to_string(&mut text)
        .map_err(|err| format!("{shown}: {err}"))?;
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
        assert_eq!(store.load().unwrap(), None, "nothing saved yet");
        let profile = Profile {
            origin: Origin::parse("else.example.com").unwrap(),
            token: OwnerToken::parse(&"A".repeat(43)).unwrap(),
        };
        store.save(&profile).unwrap();
        assert_eq!(store.load().unwrap().as_ref(), Some(&profile));
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        let replaced = Profile {
            origin: Origin::parse("127.0.0.1:7878").unwrap(),
            ..profile.clone()
        };
        store.save(&replaced).unwrap();
        assert_eq!(store.load().unwrap(), Some(replaced));
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["desktop.json"], "no temporary file is left behind");
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
        let err = ProfileStore {
            path: shared.join("desktop.json"),
        }
        .save(&profile)
        .unwrap_err();
        assert!(err.contains("0700"), "{err}");
        assert!(!shared.join("desktop.json").exists());
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
