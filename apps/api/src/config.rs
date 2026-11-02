//! Validated process configuration read from the environment.
//!
//! Validation happens before any file, database, or listener is touched, so an
//! invalid configuration exits without creating or replacing saved data.

use std::net::SocketAddr;
use std::time::Duration;

/// Default loopback listener for the owner API.
pub const DEFAULT_BIND: &str = "127.0.0.1:7878";
const DEFAULT_MAX_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 10;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub max_body_bytes: usize,
    pub shutdown_grace: Duration,
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
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|value| !value.is_empty())
    }
}

impl Config {
    pub fn load(source: &dyn Source) -> Result<Self, ConfigError> {
        let bind = parse_bind(source.get("ELSE_API_BIND").as_deref())?;
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
        Ok(Self {
            bind,
            max_body_bytes,
            shutdown_grace,
        })
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
        let config = Config::load(&MapSource::new(&[])).unwrap();
        assert_eq!(config.bind.to_string(), DEFAULT_BIND);
    }

    #[test]
    fn rejects_remote_plaintext_bind() {
        for bind in ["0.0.0.0:7878", "192.168.1.4:7878", "[::]:7878"] {
            let err = Config::load(&MapSource::new(&[("ELSE_API_BIND", bind)])).unwrap_err();
            assert!(err.to_string().contains("loopback"), "{bind}: {err}");
        }
        Config::load(&MapSource::new(&[("ELSE_API_BIND", "[::1]:9000")])).unwrap();
    }

    #[test]
    fn rejects_malformed_numbers_with_variable_name() {
        let err = Config::load(&MapSource::new(&[("ELSE_MAX_BODY_BYTES", "lots")])).unwrap_err();
        assert!(err.to_string().starts_with("ELSE_MAX_BODY_BYTES"));
        let err = Config::load(&MapSource::new(&[("ELSE_SHUTDOWN_GRACE_SECS", "0")])).unwrap_err();
        assert!(err.to_string().contains("between"));
    }
}
