//! The server the phone connects to: its validated origin and owner key,
//! and the one saved profile.
//!
//! The key never appears in URLs, logs or error messages, and it is only
//! ever sent to the one saved origin.

use std::fmt;
use std::net::IpAddr;

use url::{Host, Url};

/// A validated API origin: `https://` for any host, `http://` only for
/// loopback or a Tailscale IP, without credentials, path, query or fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    serialized: String,
    secure: bool,
}

impl Origin {
    /// A bare address (`100.101.102.103:7878`, `else.example.com`) gets
    /// `http://` for loopback and Tailscale IPs and `https://` otherwise.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim().trim_end_matches('/');
        if raw.is_empty() {
            return Err("Enter the server's address.".into());
        }
        let invalid = |err: url::ParseError| format!("The address is invalid: {err}.");
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
            _ => return Err("The address must start with https:// or http://.".into()),
        };
        if !url.username().is_empty() || url.password().is_some() {
            return Err("The address must not contain credentials.".into());
        }
        if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
            return Err("Enter only the server's origin, without a path.".into());
        }
        if url.host().is_none() {
            return Err("The address has no host.".into());
        }
        if !secure && !is_loopback(&url) && tailscale_ip(&url).is_none() {
            return Err(
                "Plain http:// only works with a Tailscale IP. Use https:// for any other address."
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

    /// The Tailscale IP an `http://` origin names; the transport checks the
    /// phone is on a tailnet before it sends the key there.
    pub fn tailscale_ip(&self) -> Option<IpAddr> {
        if self.secure {
            return None;
        }
        tailscale_ip(&Url::parse(&self.serialized).expect("validated origin"))
    }

    /// The WebSocket endpoint: `wss://` for an `https://` origin.
    pub fn ws_url(&self) -> String {
        let scheme = if self.secure { "wss" } else { "ws" };
        format!("{scheme}://{}/v1/ws", self.display())
    }
}

fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]),
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

fn tailscale_ip(url: &Url) -> Option<IpAddr> {
    let ip = match url.host()? {
        Host::Ipv4(ip) => IpAddr::V4(ip),
        Host::Ipv6(ip) => IpAddr::V6(ip),
        Host::Domain(_) => return None,
    };
    is_tailscale_ip(ip).then_some(ip)
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Whether this phone has a Tailscale address on a VPN tunnel (`utun*`):
/// Tailscale is connected, so a Tailscale IP routes into the tailnet. A
/// carrier can hand out `100.64.0.0/10` addresses too, but on its cellular
/// interface (`pdp_ip*`), never on a tunnel.
pub fn tailnet_connected() -> bool {
    let mut addresses: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list we free with freeifaddrs below.
    if unsafe { libc::getifaddrs(&mut addresses) } != 0 {
        return false;
    }
    let mut found = false;
    let mut cursor = addresses;
    while !cursor.is_null() {
        // SAFETY: cursor walks the list getifaddrs returned.
        let entry = unsafe { &*cursor };
        // SAFETY: ifa_name is a NUL-terminated interface name.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) };
        if name.to_bytes().starts_with(b"utun") && !entry.ifa_addr.is_null() {
            // SAFETY: ifa_addr points to a sockaddr of the family it names.
            let ip = unsafe {
                match i32::from((*entry.ifa_addr).sa_family) {
                    libc::AF_INET => {
                        let addr = &*(entry.ifa_addr as *const libc::sockaddr_in);
                        Some(IpAddr::from(
                            u32::from_be(addr.sin_addr.s_addr).to_be_bytes(),
                        ))
                    }
                    libc::AF_INET6 => {
                        let addr = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                        Some(IpAddr::from(addr.sin6_addr.s6_addr))
                    }
                    _ => None,
                }
            };
            found |= ip.is_some_and(is_tailscale_ip);
        }
        cursor = entry.ifa_next;
    }
    // SAFETY: addresses came from getifaddrs.
    unsafe { libc::freeifaddrs(addresses) };
    found
}

/// The owner key. Its `Debug` output is redacted.
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
            Err("Paste the 43-character owner key from ~/.else/data/owner-token.".into())
        }
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// The server this phone uses.
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

const ORIGIN_KEY: &str = "server-origin";

/// Where the owner key is kept: the iOS Keychain, so it stays out of the
/// app's files and backups. A build signed without a Keychain group (the
/// simulator, the Mac dev runner) keeps it in the app's storage instead.
mod vault {
    use nsh_core::storage::Storage;

    fn slot(origin: &str) -> String {
        format!("owner-key:{origin}")
    }

    #[cfg(target_os = "ios")]
    mod keychain {
        const SERVICE: &str = "bot.else.mobile.owner-key";
        /// `errSecMissingEntitlement`: the build has no Keychain group.
        const MISSING_ENTITLEMENT: i32 = -34018;

        pub fn read(origin: &str) -> Option<String> {
            let bytes =
                security_framework::passwords::get_generic_password(SERVICE, origin).ok()?;
            String::from_utf8(bytes).ok()
        }

        /// `Ok(false)` when this build cannot use the Keychain at all. The
        /// item stays on this device (never in backups or other devices) and
        /// opens once the phone was unlocked after a restart.
        pub fn write(origin: &str, key: &str) -> Result<bool, String> {
            use security_framework::access_control::{ProtectionMode, SecAccessControl};
            use security_framework::passwords::{PasswordOptions, set_generic_password_options};
            let access = SecAccessControl::create_with_protection(
                Some(ProtectionMode::AccessibleAfterFirstUnlockThisDeviceOnly),
                0,
            )
            .map_err(|err| format!("Cannot protect the owner key: {err}"))?;
            let mut options = PasswordOptions::new_generic_password(SERVICE, origin);
            options.set_access_control(access);
            // An item's protection cannot change in place: replace it.
            remove(origin);
            match set_generic_password_options(key.as_bytes(), options) {
                Ok(()) => Ok(true),
                Err(err) if err.code() == MISSING_ENTITLEMENT => Ok(false),
                Err(err) => Err(format!("Cannot save the owner key in the Keychain: {err}")),
            }
        }

        pub fn remove(origin: &str) {
            let _ = security_framework::passwords::delete_generic_password(SERVICE, origin);
        }
    }

    #[cfg(not(target_os = "ios"))]
    mod keychain {
        pub fn read(_: &str) -> Option<String> {
            None
        }
        pub fn write(_: &str, _: &str) -> Result<bool, String> {
            Ok(false)
        }
        pub fn remove(_: &str) {}
    }

    pub fn read(origin: &str) -> Option<String> {
        keychain::read(origin).or_else(|| Storage::get_item(&slot(origin)))
    }

    pub fn write(origin: &str, key: &str) -> Result<(), String> {
        if keychain::write(origin, key)? {
            Storage::remove_item(&slot(origin));
            return Ok(());
        }
        log::warn!("no Keychain group in this build; keeping the owner key in app storage");
        Storage::set_item(slot(origin), key)
            .map_err(|err| format!("Cannot save the owner key: {err}"))
    }

    pub fn remove(origin: &str) {
        keychain::remove(origin);
        Storage::remove_item(&slot(origin));
    }
}

/// The servers saved on this phone, by origin, in the order they were
/// added, and the one in use. Their keys live in the vault.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Saved {
    pub origins: Vec<String>,
    pub active: Option<String>,
}

const SERVERS_KEY: &str = "servers";

impl Saved {
    fn load() -> Self {
        use nsh_core::storage::Storage;
        let active = Storage::get_item(ORIGIN_KEY);
        let origins = match Storage::get_item(SERVERS_KEY) {
            Some(json) => serde_json::from_str(&json).unwrap_or_default(),
            // Builds before several servers saved only the one in use.
            None => active.iter().cloned().collect(),
        };
        Self { origins, active }
    }

    fn store(&self) -> Result<(), String> {
        use nsh_core::storage::Storage;
        let json = serde_json::to_string(&self.origins).expect("origins serialize");
        Storage::set_item(SERVERS_KEY, json)
            .map_err(|err| format!("Cannot save the server list: {err}"))?;
        match &self.active {
            Some(origin) => Storage::set_item(ORIGIN_KEY, origin)
                .map_err(|err| format!("Cannot save the server address: {err}")),
            None => {
                Storage::remove_item(ORIGIN_KEY);
                Ok(())
            }
        }
    }

    /// Adds `origin` (once) and uses it.
    fn select(&mut self, origin: &str) {
        if !self.origins.iter().any(|o| o == origin) {
            self.origins.push(origin.to_owned());
        }
        self.active = Some(origin.to_owned());
    }

    /// Removes `origin`; when it was in use, the first one left is used
    /// instead, if any.
    fn remove(&mut self, origin: &str) {
        self.origins.retain(|o| o != origin);
        if self.active.as_deref() == Some(origin) {
            self.active = self.origins.first().cloned();
        }
    }
}

impl Profile {
    /// The server in use, if one is saved and its key is still there.
    pub fn load() -> Option<Self> {
        // Builds before the Keychain kept the key in this JSON entry.
        nsh_core::storage::Storage::remove_item("server-profile");
        // When the one in use lost its key, another saved server opens.
        let saved = Saved::load();
        saved
            .active
            .iter()
            .chain(&saved.origins)
            .find_map(|origin| Self::saved(origin))
    }

    /// The saved server at `origin`, with its key.
    pub fn saved(origin: &str) -> Option<Self> {
        let key = vault::read(origin)?;
        Self::parse(origin, &key).ok()
    }

    /// Every saved server's origin, the one in use among them.
    pub fn list() -> Saved {
        Saved::load()
    }

    /// Saves this server's key and makes it the one in use.
    pub fn save(&self) -> Result<(), String> {
        let origin = self.origin.as_str();
        vault::write(origin, self.token.expose())?;
        let mut saved = Saved::load();
        saved.select(origin);
        saved.store()
    }

    /// Makes this saved server the one in use.
    pub fn activate(&self) -> Result<(), String> {
        let mut saved = Saved::load();
        saved.select(self.origin.as_str());
        saved.store()
    }

    /// Removes the server at `origin` and its key; returns the server now
    /// in use, if another one is saved.
    pub fn forget(origin: &str) -> Option<Self> {
        // The list first: a key left behind by a failed removal is unused,
        // while a listed server without its key would be the one opened.
        let mut saved = Saved::load();
        saved.remove(origin);
        if let Err(err) = saved.store() {
            log::warn!("{err}");
        }
        vault::remove(origin);
        Self::saved(saved.active.as_deref()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_the_server_in_use_moves_to_the_first_one_left() {
        let mut saved = Saved::default();
        saved.select("https://home.example");
        saved.select("https://vps.example");
        saved.select("https://home.example");
        assert_eq!(
            saved.origins,
            ["https://home.example", "https://vps.example"]
        );
        assert_eq!(saved.active.as_deref(), Some("https://home.example"));
        saved.remove("https://home.example");
        assert_eq!(saved.active.as_deref(), Some("https://vps.example"));
        saved.select("https://lab.example");
        saved.remove("https://vps.example");
        assert_eq!(saved.active.as_deref(), Some("https://lab.example"));
        saved.remove("https://lab.example");
        assert_eq!(saved, Saved::default());
    }

    #[test]
    fn bare_tailscale_and_loopback_addresses_use_http_and_others_https() {
        assert_eq!(
            Origin::parse("100.93.170.39:7878").unwrap().as_str(),
            "http://100.93.170.39:7878"
        );
        assert_eq!(
            Origin::parse("127.0.0.1:7878").unwrap().as_str(),
            "http://127.0.0.1:7878"
        );
        assert_eq!(
            Origin::parse("else.example.com/").unwrap().as_str(),
            "https://else.example.com"
        );
    }

    #[test]
    fn plain_http_to_a_public_host_is_refused() {
        assert!(Origin::parse("http://192.168.1.4:7878").is_err());
        assert!(Origin::parse("http://else.example.com").is_err());
    }

    #[test]
    fn paths_and_credentials_are_refused() {
        assert!(Origin::parse("https://else.example.com/api").is_err());
        assert!(Origin::parse("https://me:pw@else.example.com").is_err());
    }

    #[test]
    fn ws_url_follows_the_scheme() {
        assert_eq!(
            Origin::parse("https://else.example.com").unwrap().ws_url(),
            "wss://else.example.com/v1/ws"
        );
        assert_eq!(
            Origin::parse("100.93.170.39:7878").unwrap().ws_url(),
            "ws://100.93.170.39:7878/v1/ws"
        );
    }

    #[test]
    fn the_key_must_be_43_url_safe_characters() {
        assert!(OwnerToken::parse(&"a".repeat(43)).is_ok());
        assert!(OwnerToken::parse(&"a".repeat(42)).is_err());
        assert!(OwnerToken::parse(&format!("{}+", "a".repeat(42))).is_err());
        assert_eq!(
            format!("{:?}", OwnerToken::parse(&"a".repeat(43)).unwrap()),
            "OwnerToken(<redacted>)"
        );
    }
}
