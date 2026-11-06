use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

pub fn is_tailscale_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]),
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Status {
    backend_state: String,
    #[serde(rename = "TUN")]
    tun: bool,
    #[serde(rename = "TailscaleIPs")]
    addresses: Vec<IpAddr>,
    #[serde(default, rename = "Peer")]
    peers: Option<HashMap<String, Peer>>,
}

#[derive(Deserialize)]
struct Peer {
    #[serde(default, rename = "TailscaleIPs")]
    addresses: Vec<IpAddr>,
}

impl Status {
    pub async fn read() -> Result<Self, String> {
        let cli = if cfg!(target_os = "macos") {
            [
                "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
                "/opt/homebrew/bin/tailscale",
                "/usr/local/bin/tailscale",
            ]
            .into_iter()
            .map(Path::new)
            .find(|path| path.is_file())
            .unwrap_or_else(|| Path::new("tailscale"))
        } else {
            Path::new("tailscale")
        };
        Self::read_from(cli).await
    }

    async fn read_from(cli: &Path) -> Result<Self, String> {
        let output = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::process::Command::new(cli)
                .args(["status", "--json"])
                .env("TAILSCALE_BE_CLI", "1")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| "Tailscale did not respond. Check that Tailscale is running.".to_owned())?
        .map_err(|_| {
            "Cannot reach Tailscale. Install and connect Tailscale on this computer.".to_owned()
        })?;
        if !output.status.success() {
            return Err("Cannot read Tailscale status. Check that Tailscale is connected.".into());
        }
        Self::parse(&output.stdout)
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let status: Self = serde_json::from_slice(bytes).map_err(|_| {
            "Cannot read the response from Tailscale. Update Tailscale and try again.".to_owned()
        })?;
        if status.backend_state != "Running" || !status.tun {
            return Err("Connect Tailscale on this computer to use a Tailscale HTTP address. A Tailscale network interface is required.".into());
        }
        Ok(status)
    }

    pub fn local_ips(&self) -> impl Iterator<Item = IpAddr> + '_ {
        self.addresses
            .iter()
            .copied()
            .filter(|ip| is_tailscale_ip(*ip))
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        is_tailscale_ip(ip)
            && (self.addresses.contains(&ip)
                || self
                    .peers
                    .iter()
                    .flat_map(|peers| peers.values())
                    .any(|peer| peer.addresses.contains(&ip)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(state: &str, tun: bool) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "BackendState": state, "TUN": tun,
            "TailscaleIPs": ["100.88.191.63", "fd7a:115c:a1e0::1", "192.168.1.4"],
            "Peer": {"key": {"TailscaleIPs": ["100.102.228.67", "fd7a:115c:a1e0::2"]}}
        }))
        .unwrap()
    }

    #[test]
    fn only_recognizes_members_of_a_running_tailnet() {
        let status = Status::parse(&fixture("Running", true)).unwrap();
        for ip in ["100.88.191.63", "100.102.228.67", "fd7a:115c:a1e0::2"] {
            assert!(status.contains(ip.parse().unwrap()));
        }
        for ip in [
            "100.64.0.1",
            "100.100.100.100",
            "192.168.1.4",
            "8.8.8.8",
            "fd7a:115c:a1e1::1",
        ] {
            assert!(!status.contains(ip.parse().unwrap()), "{ip}");
        }
        assert_eq!(status.local_ips().count(), 2);
        assert!(Status::parse(&fixture("Stopped", true)).is_err());
        assert!(Status::parse(&fixture("Running", false)).is_err());
        assert!(Status::parse(b"{}").is_err());
    }

    #[test]
    fn address_ranges_have_exact_boundaries() {
        for ip in ["100.64.0.0", "100.127.255.255", "fd7a:115c:a1e0::1"] {
            assert!(is_tailscale_ip(ip.parse().unwrap()));
        }
        for ip in [
            "100.63.255.255",
            "100.128.0.0",
            "fd7a:115c:a1df::1",
            "::ffff:100.64.0.1",
        ] {
            assert!(!is_tailscale_ip(ip.parse().unwrap()));
        }
    }

    #[tokio::test]
    async fn reads_cli_status_and_handles_a_tailnet_without_peers() {
        use std::os::unix::fs::PermissionsExt as _;
        let directory = tempfile::tempdir().unwrap();
        let cli = directory.path().join("tailscale");
        let mut status: serde_json::Value =
            serde_json::from_slice(&fixture("Running", true)).unwrap();
        status["Peer"] = serde_json::Value::Null;
        std::fs::write(
            &cli,
            format!(
                "#!/bin/sh\n[ \"$1 $2\" = 'status --json' ] && [ \"$TAILSCALE_BE_CLI\" = 1 ] || exit 1\nprintf '%s' '{}'\n",
                status
            ),
        )
        .unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let status = Status::read_from(&cli).await.unwrap();
        assert!(status.contains("100.88.191.63".parse().unwrap()));
        assert!(!status.contains("100.102.228.67".parse().unwrap()));
    }

    #[tokio::test]
    async fn unavailable_cli_fails_closed() {
        assert!(
            Status::read_from(Path::new("/nonexistent/tailscale"))
                .await
                .is_err()
        );
    }
}
