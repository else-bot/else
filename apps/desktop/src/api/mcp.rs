use super::{Client, ClientError};
use reqwest::Method;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone, Deserialize)]
pub struct InstalledMcp {
    pub id: String,
    pub name: String,
    pub description: String,
    pub installed_by: String,
    pub installed_at: String,
    /// The HTTP server's origin; absent for local commands and older servers.
    #[serde(default)]
    pub website: Option<String>,
}

impl Client {
    pub async fn installed_mcps(self: Arc<Self>) -> Result<Vec<InstalledMcp>, ClientError> {
        #[derive(Deserialize)]
        struct Library {
            mcps: Vec<InstalledMcp>,
        }
        self.call::<Library>(Method::GET, "/v1/settings/mcp".into(), None)
            .await
            .map(|(_, library)| library.mcps)
    }

    /// The service's favicon as PNG bytes, or `None` when no candidate
    /// serves a decodable image.
    pub async fn service_icon(self: Arc<Self>, website: String) -> Option<Vec<u8>> {
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured((*self.tls).clone())
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 3 || !public_url(attempt.url()) {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .dns_resolver(Arc::new(PublicResolver))
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        self.run(async move {
            for url in icon_candidates(&website) {
                if let Some(png) = fetch_icon(&http, url).await {
                    return Some(png);
                }
            }
            None
        })
        .await
    }

    pub async fn remove_mcp(self: Arc<Self>, id: String) -> Result<(), ClientError> {
        self.call::<()>(Method::DELETE, format!("/v1/settings/mcp/{id}"), None)
            .await
            .map(|_| ())
    }
}

const MAX_ICON_BYTES: usize = 256 * 1024;

/// Bot-installed MCPs choose these origins, so the fetch must not reach this
/// computer's own services: loopback, link-local (cloud metadata),
/// unspecified and multicast addresses are refused at every redirect and DNS
/// answer. Tailnet and LAN addresses stay reachable for self-hosted services.
fn blocked(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || match ip {
            IpAddr::V4(v4) => v4.is_link_local() || v4.is_broadcast(),
            IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
        }
}

fn public_url(url: &reqwest::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && match url.host() {
            Some(url::Host::Domain(domain)) => {
                domain != "localhost" && !domain.ends_with(".localhost")
            }
            Some(url::Host::Ipv4(ip)) => !blocked(ip.into()),
            Some(url::Host::Ipv6(ip)) => !blocked(ip.into()),
            None => false,
        }
}

struct PublicResolver;

impl reqwest::dns::Resolve for PublicResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<_> = tokio::net::lookup_host((host, 0))
                .await?
                .filter(|addr| !blocked(addr.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("the host resolves only to local addresses".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

async fn fetch_icon(http: &reqwest::Client, url: String) -> Option<Vec<u8>> {
    let mut response = http.get(url).send().await.ok()?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|len| len > MAX_ICON_BYTES as u64)
    {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if bytes.len() + chunk.len() > MAX_ICON_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    icon_png(&bytes)
}

/// Hosts whose MCP endpoint serves no favicon, mapped to the site that
/// carries the service's icon.
const ICON_ALIASES: &[(&str, &str)] = &[("githubcopilot.com", "github.com")];

/// Favicon URLs to try for an MCP origin: the host, then its parent domain,
/// each through `ICON_ALIASES`. Loopback and IP hosts are skipped, since
/// they name the server's machine rather than this one.
fn icon_candidates(website: &str) -> Vec<String> {
    let Ok(url) = reqwest::Url::parse(website) else {
        return Vec::new();
    };
    let Some(host) = url.domain().filter(|host| *host != "localhost") else {
        return Vec::new();
    };
    let origin = url.origin().ascii_serialization();
    let mut candidates = vec![format!("{origin}/favicon.ico")];
    let alias = |domain: &str| {
        ICON_ALIASES
            .iter()
            .find(|(alias, _)| *alias == domain)
            .map(|(_, site)| (*site).to_owned())
    };
    let labels: Vec<&str> = host.split('.').collect();
    let parent = (labels.len() > 2).then(|| labels[1..].join("."));
    let domains = alias(host)
        .into_iter()
        .chain(parent.map(|parent| alias(&parent).unwrap_or(parent)));
    for domain in domains {
        let candidate = format!("https://{domain}/favicon.ico");
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

/// Decodes a favicon (ICO, PNG, GIF, JPEG or WebP) and re-encodes it as a
/// small PNG.
fn icon_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(1024);
    limits.max_image_height = Some(1024);
    limits.max_alloc = Some(16 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    if image.width() < 16 || image.height() < 16 {
        return None;
    }
    let mut png = Vec::new();
    image
        .thumbnail(64, 64)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

#[cfg(test)]
mod icon_tests {
    use super::icon_candidates;

    #[test]
    fn icons_come_from_the_host_its_parent_and_known_aliases_but_not_loopback() {
        assert_eq!(
            icon_candidates("https://mcp.sentry.dev"),
            ["https://mcp.sentry.dev/favicon.ico", "https://sentry.dev/favicon.ico"]
        );
        assert_eq!(
            icon_candidates("https://api.githubcopilot.com"),
            [
                "https://api.githubcopilot.com/favicon.ico",
                "https://github.com/favicon.ico"
            ]
        );
        assert_eq!(
            icon_candidates("http://kaneo.internal:8080"),
            ["http://kaneo.internal:8080/favicon.ico"]
        );
        assert!(icon_candidates("http://127.0.0.1:3000").is_empty());
        assert!(icon_candidates("http://localhost:3000").is_empty());
    }

    #[test]
    fn redirects_cannot_reach_this_computer_or_cloud_metadata() {
        use super::public_url;
        let url = |text: &str| reqwest::Url::parse(text).unwrap();
        for local in [
            "http://127.0.0.1:8080/x",
            "http://[::1]/x",
            "http://[::ffff:127.0.0.1]/x",
            "http://169.254.169.254/latest/meta-data",
            "http://0.0.0.0/x",
            "http://localhost/x",
            "http://app.localhost/x",
            "file:///etc/passwd",
        ] {
            assert!(!public_url(&url(local)), "{local}");
        }
        for public in [
            "https://sentry.io/favicon.ico",
            "http://100.102.228.67:7878/favicon.ico",
            "http://192.168.1.4/favicon.ico",
        ] {
            assert!(public_url(&url(public)), "{public}");
        }
    }

    #[test]
    fn oversized_images_are_refused_before_decoding() {
        let mut png = Vec::new();
        image::RgbImage::new(4096, 4096)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        assert!(super::icon_png(&png).is_none());
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct McpConsent {
    pub id: String,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Deserialize)]
pub struct McpAuthorization {
    pub request_id: uuid::Uuid,
    pub authorization_url: String,
    pub expires_in: u64,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

impl Client {
    pub async fn authorize_mcp(
        self: Arc<Self>,
        bot: uuid::Uuid,
        id: String,
        redirect_uri: String,
    ) -> Result<McpAuthorization, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this
                .request(
                    Method::POST,
                    &format!("/v1/bots/{bot}/mcp/{id}/oauth/start"),
                )
                .timeout(std::time::Duration::from_secs(90))
                .json(&serde_json::json!({"redirect_uri":redirect_uri}));
            this.send_json::<McpAuthorization>(request)
                .await
                .map(|(_, body)| body)
        })
        .await
    }

    pub async fn complete_mcp(
        self: Arc<Self>,
        bot: uuid::Uuid,
        id: String,
        callback: serde_json::Value,
    ) -> Result<(), ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this
                .request(
                    Method::POST,
                    &format!("/v1/bots/{bot}/mcp/{id}/oauth/complete"),
                )
                .timeout(std::time::Duration::from_secs(60))
                .json(&callback);
            this.send_json::<serde_json::Value>(request)
                .await
                .map(|_| ())
        })
        .await
    }

    pub fn cancel_mcp(self: Arc<Self>, bot: uuid::Uuid, id: String, request: uuid::Uuid) {
        self.clone().runtime.spawn(async move {
            let _ = self
                .call::<()>(
                    Method::DELETE,
                    format!("/v1/bots/{bot}/mcp/{id}/oauth/{request}"),
                    None,
                )
                .await;
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpState {
    Disconnected,
    Authorizing,
    Connected,
    ReauthRequired,
}

#[derive(Deserialize)]
pub struct McpConnectionStatus {
    pub status: McpState,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

impl Client {
    pub async fn mcp_connection(
        self: Arc<Self>,
        bot: uuid::Uuid,
        id: String,
    ) -> Result<McpConnectionStatus, ClientError> {
        self.call(
            Method::GET,
            format!("/v1/bots/{bot}/mcp/{id}/connection"),
            None,
        )
        .await
        .map(|(_, body)| body)
    }

    pub async fn cancel_pending_mcp(
        self: Arc<Self>,
        bot: uuid::Uuid,
        id: String,
    ) -> Result<McpConnectionStatus, ClientError> {
        self.call(
            Method::DELETE,
            format!("/v1/bots/{bot}/mcp/{id}/oauth"),
            None,
        )
        .await
        .map(|(_, body)| body)
    }
}
