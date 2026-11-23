//! A server's address and its owner token, checked before anything is
//! sent. The token never appears in URLs, logs or error messages, and it is
//! only ever sent to the one origin.

use std::fmt;

use url::{Host, Url};

/// A validated API origin: `https://` for any host, `http://` only for
/// loopback or a Tailscale IP, without credentials, path, query, or fragment.
/// Tailscale membership is checked by the transport before sending credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    serialized: String,
    secure: bool,
}

impl Origin {
    /// A bare address (`127.0.0.1:7878`, `else.example.com`) gets `http://`
    /// for loopback and Tailscale IPs, and `https://` for anything else.
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
            if is_loopback(&plain) || tailscale_ip(&plain).is_some() {
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
        if !secure && !is_loopback(&url) && tailscale_ip(&url).is_none() {
            return Err(
                "HTTP requires a local address or a verified Tailscale IP. The owner token is not sent unencrypted \
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

    /// The host as the owner reads it, without the scheme.
    pub fn display(&self) -> &str {
        self.serialized
            .split_once("://")
            .map_or(self.serialized.as_str(), |(_, rest)| rest)
    }

    /// Whether the owner token sent here stays on this machine.
    pub fn is_loopback(&self) -> bool {
        Url::parse(&self.serialized).is_ok_and(|url| is_loopback(&url))
    }

    pub fn tailscale_ip(&self) -> Option<std::net::IpAddr> {
        if self.secure {
            return None;
        }
        tailscale_ip(&Url::parse(&self.serialized).expect("validated origin"))
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

fn tailscale_ip(url: &Url) -> Option<std::net::IpAddr> {
    let ip = match url.host()? {
        Host::Ipv4(ip) => std::net::IpAddr::V4(ip),
        Host::Ipv6(ip) => std::net::IpAddr::V6(ip),
        Host::Domain(_) => return None,
    };
    else_tailnet::is_tailscale_ip(ip).then_some(ip)
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
