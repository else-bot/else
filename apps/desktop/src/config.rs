//! Desktop configuration: the API origin and the owner token.
//!
//! The token is read from a private file (or, for a development launcher,
//! the `ELSE_OWNER_TOKEN` variable). It never appears in URLs, logs, or
//! error messages, and it is only ever sent to the one configured origin.

use std::fmt;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

pub const DEFAULT_API_URL: &str = "http://127.0.0.1:7878";

/// A validated API origin such as `http://127.0.0.1:7878`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin(String);

impl Origin {
    /// Accept `http://<loopback>:<port>` only. This build has no TLS; reach a
    /// remote API through an SSH tunnel to a local port.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim().trim_end_matches('/');
        let rest = raw
            .strip_prefix("http://")
            .ok_or("ELSE_API_URL must start with http:// (use an SSH tunnel for remote access)")?;
        if rest.contains(['/', '?', '#', '@']) || rest.is_empty() {
            return Err(
                "ELSE_API_URL must be an origin like http://127.0.0.1:7878, \
                        without a path, query, or credentials"
                    .into(),
            );
        }
        let (host, port) = match rest.strip_prefix('[') {
            Some(v6) => {
                let (host, tail) = v6
                    .split_once(']')
                    .ok_or("ELSE_API_URL has a malformed IPv6 host")?;
                (host, tail.strip_prefix(':').unwrap_or(""))
            }
            None => rest.rsplit_once(':').unwrap_or((rest, "")),
        };
        if !port.is_empty() && port.parse::<u16>().is_err() {
            return Err("ELSE_API_URL has an invalid port".into());
        }
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback());
        if !loopback {
            return Err(
                "ELSE_API_URL must name a loopback host; plaintext HTTP is not sent \
                        across the network. Use an SSH tunnel such as \
                        ssh -L 7878:127.0.0.1:7878 <server>"
                    .into(),
            );
        }
        Ok(Self(raw.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The owner bearer token. Its `Debug` output is redacted.
#[derive(Clone)]
pub struct OwnerToken(String);

impl fmt::Debug for OwnerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnerToken(<redacted>)")
    }
}

impl OwnerToken {
    fn parse(text: &str) -> Result<Self, String> {
        let token = text.strip_suffix('\n').unwrap_or(text);
        let valid = token.len() == 43
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if valid {
            Ok(Self(token.to_owned()))
        } else {
            Err(
                "the owner token is malformed; expected the value written by `else-api setup`"
                    .into(),
            )
        }
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub origin: Origin,
    pub token_source: TokenSource,
}

#[derive(Debug, Clone)]
pub enum TokenSource {
    Env,
    File(PathBuf),
}

impl TokenSource {
    #[cfg(test)]
    pub fn describe(&self) -> String {
        match self {
            Self::Env => "ELSE_OWNER_TOKEN".into(),
            Self::File(path) => path.display().to_string(),
        }
    }
}

impl Config {
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let origin = Origin::parse(&get("ELSE_API_URL").unwrap_or_else(|| DEFAULT_API_URL.into()))?;
        let token_source = if get("ELSE_OWNER_TOKEN").is_some() {
            TokenSource::Env
        } else if let Some(path) = get("ELSE_OWNER_TOKEN_FILE") {
            TokenSource::File(PathBuf::from(path))
        } else {
            let data = match (get("ELSE_DATA_DIR"), get("XDG_DATA_HOME"), get("HOME")) {
                (Some(dir), _, _) => PathBuf::from(dir),
                (None, Some(xdg), _) if !xdg.is_empty() => Path::new(&xdg).join("else-bot"),
                (None, _, Some(home)) => Path::new(&home).join(".local/share/else-bot"),
                _ => return Err("set ELSE_OWNER_TOKEN_FILE to the owner token file".into()),
            };
            TokenSource::File(data.join("owner-token"))
        };
        Ok(Self {
            origin,
            token_source,
        })
    }

    /// Read the token now. Called again after the owner fixes credentials.
    pub fn load_token(&self, get: impl Fn(&str) -> Option<String>) -> Result<OwnerToken, String> {
        match &self.token_source {
            TokenSource::Env => OwnerToken::parse(&get("ELSE_OWNER_TOKEN").unwrap_or_default()),
            TokenSource::File(path) => read_token_file(path),
        }
    }
}

fn read_token_file(path: &Path) -> Result<OwnerToken, String> {
    let shown = path.display();
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => {
                format!("owner token file {shown} not found; run `else-api setup` or set ELSE_OWNER_TOKEN_FILE")
            }
            _ if err.raw_os_error() == Some(libc::ELOOP) => {
                format!("owner token file {shown} is a symbolic link; refusing to read it")
            }
            _ => format!("cannot read owner token file {shown}: {err}"),
        })?;
    let meta = file.metadata().map_err(|err| format!("{shown}: {err}"))?;
    // SAFETY: getuid has no preconditions.
    let uid = unsafe { libc::getuid() };
    if !meta.is_file() || meta.uid() != uid || meta.permissions().mode() & 0o077 != 0 {
        return Err(format!(
            "owner token file {shown} must be a regular file owned by you with mode 0600"
        ));
    }
    let mut text = String::new();
    file.by_ref()
        .take(4096)
        .read_to_string(&mut text)
        .map_err(|err| format!("{shown}: {err}"))?;
    OwnerToken::parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        for ok in [
            "http://127.0.0.1:7878",
            "http://localhost:17001/",
            "http://[::1]:7878",
        ] {
            assert!(Origin::parse(ok).is_ok(), "{ok}");
        }
        for bad in [
            "https://example.com",
            "http://example.com:7878",
            "http://10.0.0.2:7878",
            "http://user:pass@127.0.0.1:7878",
            "http://127.0.0.1:7878/v1",
            "http://127.0.0.1:7878?token=x",
            "127.0.0.1:7878",
            "http://127.0.0.1:99999",
        ] {
            assert!(Origin::parse(bad).is_err(), "{bad}");
        }
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
        std::fs::write(&path, "short").unwrap();
        let err = read_token_file(&path).unwrap_err();
        assert!(err.contains("malformed") && !err.contains("short"));
        assert_eq!(
            format!("{:?}", OwnerToken::parse(&token).unwrap()),
            "OwnerToken(<redacted>)"
        );
    }

    #[test]
    fn token_source_defaults() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| v.to_string())
            }
        };
        let config = Config::from_env(env(&[("HOME", "/home/x")])).unwrap();
        assert_eq!(
            config.token_source.describe(),
            "/home/x/.local/share/else-bot/owner-token"
        );
        let config = Config::from_env(env(&[("ELSE_DATA_DIR", "/srv/e")])).unwrap();
        assert_eq!(config.token_source.describe(), "/srv/e/owner-token");
        assert!(matches!(
            Config::from_env(env(&[("ELSE_OWNER_TOKEN", "x"), ("HOME", "/h")]))
                .unwrap()
                .token_source,
            TokenSource::Env
        ));
    }
}
