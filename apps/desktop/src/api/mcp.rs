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
            .redirect(reqwest::redirect::Policy::limited(3))
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .ok()?;
        self.run(async move {
            for url in icon_candidates(&website) {
                let Ok(response) = http.get(url).send().await else {
                    continue;
                };
                if !response.status().is_success()
                    || response.content_length().is_some_and(|len| len > MAX_ICON_BYTES)
                {
                    continue;
                }
                let Ok(bytes) = response.bytes().await else {
                    continue;
                };
                if let Some(png) = (bytes.len() as u64 <= MAX_ICON_BYTES)
                    .then(|| icon_png(&bytes))
                    .flatten()
                {
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

const MAX_ICON_BYTES: u64 = 256 * 1024;

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
    let image = image::load_from_memory(bytes).ok()?;
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
