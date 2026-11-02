//! Validated process configuration read from the environment.
//!
//! Validation happens before any file, database, or listener is touched, so an
//! invalid configuration exits without creating or replacing saved data.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::credentials::SecretKeySource;

/// Default loopback listener for the owner API.
pub const DEFAULT_BIND: &str = "127.0.0.1:7878";
const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 10;

/// PostgreSQL connection URL. It may contain a password, so it is never
/// printed through `Debug` or error messages.
#[derive(Clone)]
pub struct DatabaseUrl(String);

impl DatabaseUrl {
    pub fn new(url: String) -> Self {
        Self(url)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for DatabaseUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DatabaseUrl(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// `None` when neither `DATABASE_URL` nor the Compose `POSTGRES_*`
    /// variables are set. Commands that need the database report it.
    pub database_url: Option<DatabaseUrl>,
    pub bind: SocketAddr,
    /// Private runtime storage: credentials, the singleton lock, and bot
    /// session directories.
    pub data_dir: PathBuf,
    pub owner_token_file: PathBuf,
    pub secret_key: SecretKeySource,
    pub max_body_bytes: usize,
    pub shutdown_grace: Duration,
    /// Expose the deterministic synthetic provider (tests and demos).
    pub synthetic_provider: bool,
    /// Provider turns running at once across all bots.
    pub max_active_turns: usize,
    /// Explicit Codex CLI path; otherwise `codex` on `PATH`.
    pub codex_bin: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{name}: {reason}")]
    Invalid { name: &'static str, reason: String },
}

fn invalid(name: &'static str, reason: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        name,
        reason: reason.into(),
    }
}

/// Source of configuration values. Tests supply a map; the binary reads the
/// process environment.
pub trait Source {
    fn get(&self, name: &str) -> Option<String>;
}

pub struct ProcessEnv;

impl Source for ProcessEnv {
    /// An empty variable is returned as supplied, so an explicitly empty
    /// override (for example `SECRET_KEY=`) is rejected rather than ignored.
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

impl Config {
    pub fn load(source: &dyn Source) -> Result<Self, ConfigError> {
        let bind = parse_bind(source.get("ELSE_API_BIND").as_deref())?;
        let data_dir = match source.get("ELSE_DATA_DIR") {
            Some(dir) => absolute_path("ELSE_DATA_DIR", dir)?,
            None => default_data_dir(source)?,
        };
        let owner_token_file = match source.get("OWNER_TOKEN_FILE") {
            Some(path) => absolute_path("OWNER_TOKEN_FILE", path)?,
            None => data_dir.join("owner-token"),
        };
        // SECRET_KEY is validated when credentials load; it is never echoed.
        let secret_key = match source.get("SECRET_KEY") {
            Some(value) => SecretKeySource::Env(value),
            None => SecretKeySource::File(match source.get("SECRET_KEY_FILE") {
                Some(path) => absolute_path("SECRET_KEY_FILE", path)?,
                None => data_dir.join("secret-key"),
            }),
        };
        let max_body_bytes = parse_number(
            source,
            "ELSE_MAX_BODY_BYTES",
            DEFAULT_MAX_BODY_BYTES as u64,
            1024,
            1024 * 1024,
        )? as usize;
        let shutdown_grace = Duration::from_secs(parse_number(
            source,
            "ELSE_SHUTDOWN_GRACE_SECS",
            DEFAULT_SHUTDOWN_GRACE_SECS,
            1,
            120,
        )?);
        let synthetic_provider = parse_bool(source, "ELSE_SYNTHETIC_PROVIDER")?;
        let max_active_turns = parse_number(source, "ELSE_MAX_ACTIVE_TURNS", 2, 1, 32)? as usize;
        let codex_bin = match source.get("ELSE_CODEX_BIN") {
            Some(path) => Some(absolute_path("ELSE_CODEX_BIN", path)?),
            None => None,
        };
        Ok(Self {
            codex_bin,
            synthetic_provider,
            max_active_turns,
            database_url: database_url(source)?,
            bind,
            data_dir,
            owner_token_file,
            secret_key,
            max_body_bytes,
            shutdown_grace,
        })
    }
}

/// `DATABASE_URL` wins. Otherwise the development `.env` Compose variables
/// describe the local container on 127.0.0.1.
fn database_url(source: &dyn Source) -> Result<Option<DatabaseUrl>, ConfigError> {
    if let Some(url) = source.get("DATABASE_URL") {
        return Ok(Some(DatabaseUrl(url)));
    }
    let (Some(user), Some(password), Some(db)) = (
        source.get("POSTGRES_USER"),
        source.get("POSTGRES_PASSWORD"),
        source.get("POSTGRES_DB"),
    ) else {
        return Ok(None);
    };
    let port = parse_number(source, "POSTGRES_PORT", 5432, 1, 65535)?;
    Ok(Some(DatabaseUrl(format!(
        "postgresql://{}:{}@127.0.0.1:{port}/{}",
        percent_encode(&user),
        percent_encode(&password),
        percent_encode(&db)
    ))))
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl Config {
    pub fn require_database(&self) -> Result<&DatabaseUrl, ConfigError> {
        self.database_url.as_ref().ok_or_else(|| {
            invalid(
                "DATABASE_URL",
                "not set; set it or the POSTGRES_USER, POSTGRES_PASSWORD, and POSTGRES_DB \
                 variables from .env.example",
            )
        })
    }
}

/// Relative paths resolve against the current directory once, so later
/// directory changes cannot redirect credential reads.
fn absolute_path(name: &'static str, value: String) -> Result<PathBuf, ConfigError> {
    std::path::absolute(&value).map_err(|err| invalid(name, err.to_string()))
}

fn default_data_dir(source: &dyn Source) -> Result<PathBuf, ConfigError> {
    // The XDG specification treats an empty value as unset.
    if let Some(xdg) = source.get("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        return Ok(absolute_path("XDG_DATA_HOME", xdg)?.join("else-bot"));
    }
    match source.get("HOME").filter(|v| !v.is_empty()) {
        Some(home) => Ok(absolute_path("HOME", home)?.join(".local/share/else-bot")),
        None => Err(invalid(
            "ELSE_DATA_DIR",
            "not set, and neither XDG_DATA_HOME nor HOME is available",
        )),
    }
}

/// The API serves plaintext HTTP, so it may only listen on loopback. Remote
/// access goes through an SSH tunnel or a TLS reverse proxy bound privately.
fn parse_bind(value: Option<&str>) -> Result<SocketAddr, ConfigError> {
    const NAME: &str = "ELSE_API_BIND";
    let raw = value.unwrap_or(DEFAULT_BIND);
    let addr: SocketAddr = raw.parse().map_err(|_| {
        invalid(
            NAME,
            "expected an IP address and port such as 127.0.0.1:7878",
        )
    })?;
    if !addr.ip().is_loopback() {
        return Err(invalid(
            NAME,
            "the API serves plaintext HTTP and only binds loopback addresses; \
             use an SSH tunnel or a private TLS reverse proxy for remote access",
        ));
    }
    Ok(addr)
}

fn parse_bool(source: &dyn Source, name: &'static str) -> Result<bool, ConfigError> {
    match source.get(name).as_deref() {
        None => Ok(false),
        Some("1" | "true") => Ok(true),
        Some("0" | "false") => Ok(false),
        Some(_) => Err(invalid(name, "expected 1, 0, true, or false")),
    }
}

fn parse_number(
    source: &dyn Source,
    name: &'static str,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, ConfigError> {
    let Some(raw) = source.get(name) else {
        return Ok(default);
    };
    let value: u64 = raw
        .trim()
        .parse()
        .map_err(|_| invalid(name, "expected a whole number"))?;
    if !(min..=max).contains(&value) {
        return Err(invalid(name, format!("must be between {min} and {max}")));
    }
    Ok(value)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::collections::HashMap;

    pub struct MapSource(pub HashMap<String, String>);

    impl MapSource {
        pub fn new(pairs: &[(&str, &str)]) -> Self {
            Self(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )
        }
    }

    impl Source for MapSource {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    #[test]
    fn defaults_bind_loopback() {
        let config = Config::load(&MapSource::new(&[("HOME", "/home/x")])).unwrap();
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
        assert_eq!(
            config.data_dir,
            PathBuf::from("/home/x/.local/share/else-bot")
        );
        assert_eq!(config.owner_token_file, config.data_dir.join("owner-token"));
    }

    #[test]
    fn database_url_falls_back_to_compose_variables() {
        let config = Config::load(&MapSource::new(&[
            ("HOME", "/h"),
            ("POSTGRES_USER", "else"),
            ("POSTGRES_PASSWORD", "p@ss word"),
            ("POSTGRES_DB", "else"),
            ("POSTGRES_PORT", "5433"),
        ]))
        .unwrap();
        let url = config.require_database().unwrap();
        assert_eq!(
            url.expose(),
            "postgresql://else:p%40ss%20word@127.0.0.1:5433/else"
        );
        assert!(!format!("{config:?}").contains("p%40ss"));
        let config = Config::load(&MapSource::new(&[("HOME", "/h")])).unwrap();
        assert!(
            config
                .require_database()
                .unwrap_err()
                .to_string()
                .starts_with("DATABASE_URL")
        );
    }

    #[test]
    fn empty_overrides_are_not_ignored() {
        let config = Config::load(&MapSource::new(&[("HOME", "/h"), ("SECRET_KEY", "")])).unwrap();
        let err = crate::credentials::load_secret_key(&config.secret_key).unwrap_err();
        assert!(
            err.to_string().contains("SECRET_KEY environment variable"),
            "{err}"
        );
        assert!(Config::load(&MapSource::new(&[("HOME", "/h"), ("ELSE_API_BIND", "")])).is_err());
        assert!(Config::load(&MapSource::new(&[("HOME", "/h"), ("ELSE_DATA_DIR", "")])).is_err());
        let config =
            Config::load(&MapSource::new(&[("HOME", "/h"), ("XDG_DATA_HOME", "")])).unwrap();
        assert_eq!(config.data_dir, PathBuf::from("/h/.local/share/else-bot"));
    }

    #[test]
    fn secret_key_env_takes_precedence_and_is_not_echoed() {
        let config = Config::load(&MapSource::new(&[
            ("ELSE_DATA_DIR", "/srv/else"),
            ("SECRET_KEY", "canary-value"),
        ]))
        .unwrap();
        assert!(matches!(config.secret_key, SecretKeySource::Env(_)));
        assert!(!format!("{:?}", config).contains("canary-value"));
    }

    #[test]
    fn rejects_remote_plaintext_bind() {
        for bind in ["0.0.0.0:7878", "192.168.1.4:7878", "[::]:7878"] {
            let err = Config::load(&MapSource::new(&[("ELSE_API_BIND", bind), ("HOME", "/h")]))
                .unwrap_err();
            assert!(err.to_string().contains("loopback"), "{bind}: {err}");
        }
        Config::load(&MapSource::new(&[
            ("ELSE_API_BIND", "[::1]:9000"),
            ("HOME", "/h"),
        ]))
        .unwrap();
    }

    #[test]
    fn rejects_malformed_numbers_with_variable_name() {
        let err = Config::load(&MapSource::new(&[
            ("ELSE_MAX_BODY_BYTES", "lots"),
            ("HOME", "/h"),
        ]))
        .unwrap_err();
        assert!(err.to_string().starts_with("ELSE_MAX_BODY_BYTES"));
        let err = Config::load(&MapSource::new(&[
            ("ELSE_SHUTDOWN_GRACE_SECS", "0"),
            ("HOME", "/h"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("between"));
    }
}
