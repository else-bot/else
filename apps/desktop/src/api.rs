//! HTTP client for the else.bot API.
//!
//! Requests run on a private Tokio runtime, away from GPUI's render path.
//! Redirects are never followed, so the bearer token can only reach the one
//! configured origin; it is attached as a sensitive header, never a URL.

use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use crate::config::{Origin, OwnerToken};

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    pub max_text_bytes: usize,
    pub max_queued_turns: usize,
    pub max_bot_name_chars: usize,
    pub max_bot_role_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Capabilities {
    pub api_version: String,
    pub server_instance_id: Uuid,
    pub default_provider_id: Option<String>,
    pub providers: Vec<ProviderInfo>,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Activity {
    pub phase: String,
    pub active_turn_id: Option<Uuid>,
    pub queued_count: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Cleanup {
    pub state: String,
    pub error: Option<serde_json::Value>,
}

/// An active bot, or a deletion receipt (`cleanup` set, no role).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Bot {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub conversation_id: Option<Uuid>,
    #[serde(default)]
    pub selected_provider_id: Option<String>,
    #[serde(default)]
    pub activity: Option<Activity>,
    #[serde(default)]
    pub cleanup: Option<Cleanup>,
}

impl Bot {
    pub fn is_active(&self) -> bool {
        self.cleanup.is_none()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotList {
    pub default_bot_id: Option<Uuid>,
    pub bots: Vec<Bot>,
}

#[derive(Debug, Clone, Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Clone, Deserialize)]
struct ErrorDetail {
    code: String,
    message: String,
    #[serde(default)]
    retryable: bool,
}

/// Errors shown to the owner. None of them contain the token.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClientError {
    #[error("The API rejected the owner token. Check the token file and restart the desktop.")]
    Unauthorized,
    #[error("Cannot reach the API at {0}. Is `else-api serve` running?")]
    Unreachable(String),
    #[error("{message}")]
    Api {
        status: u16,
        code: String,
        message: String,
        retryable: bool,
    },
    #[error("Unexpected response from the API: {0}")]
    Protocol(String),
}

impl ClientError {
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Whether the outcome of a mutation is unknown (the request may or may
    /// not have been applied), as opposed to definitely rejected.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Unreachable(_) | Self::Protocol(_) => true,
            Self::Api { status, .. } => *status >= 500,
            Self::Unauthorized => false,
        }
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    origin: Origin,
    auth: HeaderValue,
    runtime: tokio::runtime::Handle,
}

impl Client {
    pub fn new(origin: Origin, token: &OwnerToken, runtime: tokio::runtime::Handle) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("HTTP client");
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
            .expect("token is a valid header value");
        auth.set_sensitive(true);
        Self {
            http,
            origin,
            auth,
            runtime,
        }
    }

    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    /// Run a future on the client's runtime and await it from any executor.
    pub async fn run<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = T> + Send + 'static,
    ) -> T {
        self.runtime.spawn(fut).await.expect("API task panicked")
    }

    pub(crate) fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.origin.as_str()))
            .header(AUTHORIZATION, self.auth.clone())
    }

    pub(crate) async fn send_json<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(StatusCode, T), ClientError> {
        let response = request
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(|err| {
                if err.is_connect() || err.is_timeout() || err.is_request() {
                    ClientError::Unreachable(self.origin.as_str().to_owned())
                } else {
                    ClientError::Protocol(err.without_url().to_string())
                }
            })?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        if status.is_redirection() {
            return Err(ClientError::Protocol(format!(
                "redirect ({status}) refused; the token is never sent to another origin"
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| ClientError::Protocol(err.without_url().to_string()))?;
        if !status.is_success() {
            return Err(match serde_json::from_slice::<ErrorBody>(&bytes) {
                Ok(body) => ClientError::Api {
                    status: status.as_u16(),
                    code: body.error.code,
                    message: body.error.message,
                    retryable: body.error.retryable,
                },
                Err(_) => ClientError::Protocol(format!("HTTP {status}")),
            });
        }
        serde_json::from_slice(&bytes)
            .map(|value| (status, value))
            .map_err(|err| ClientError::Protocol(err.to_string()))
    }

    pub async fn capabilities(self: Arc<Self>) -> Result<Capabilities, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this.request(Method::GET, "/v1/capabilities");
            this.send_json(request).await.map(|(_, caps)| caps)
        })
        .await
    }

    pub async fn bots(self: Arc<Self>) -> Result<BotList, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this.request(Method::GET, "/v1/bots?include_deleting=true");
            this.send_json(request).await.map(|(_, list)| list)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-shot HTTP server returning `response` and capturing the request.
    async fn serve_once(response: &'static str) -> (Origin, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (origin, task)
    }

    fn client(origin: Origin) -> Arc<Client> {
        let token = crate::config::Config {
            origin: origin.clone(),
            token_source: crate::config::TokenSource::Env,
        }
        .load_token(|_| Some("T".repeat(43)))
        .unwrap();
        Arc::new(Client::new(
            origin,
            &token,
            tokio::runtime::Handle::current(),
        ))
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_token_stays_in_header() {
        let (origin, server) = serve_once(
            "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://evil.example/\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
        let err = client(origin).bots().await.unwrap_err();
        assert!(
            matches!(err, ClientError::Protocol(ref m) if m.contains("redirect")),
            "{err:?}"
        );
        let request = server.await.unwrap();
        assert!(request.contains(&format!("authorization: Bearer {}", "T".repeat(43))));
        assert!(
            !request.lines().next().unwrap().contains(&"T".repeat(43)),
            "token not in URL"
        );
    }

    #[tokio::test]
    async fn unauthorized_and_unreachable_are_distinct() {
        let (origin, _server) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert_eq!(
            client(origin).bots().await.unwrap_err(),
            ClientError::Unauthorized
        );
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let origin = Origin::parse(&format!("http://127.0.0.1:{port}")).unwrap();
        let err = client(origin).bots().await.unwrap_err();
        assert!(matches!(err, ClientError::Unreachable(_)));
        assert!(!err.to_string().contains(&"T".repeat(43)));
    }

    #[tokio::test]
    async fn api_errors_use_the_shared_shape() {
        let body = r#"{"error":{"code":"queue_full","message":"Too many messages.","retryable":true,"request_id":null}}"#;
        let response: &'static str = Box::leak(
            format!(
                "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        );
        let (origin, _server) = serve_once(response).await;
        let err = client(origin).bots().await.unwrap_err();
        assert_eq!(err.code(), Some("queue_full"));
        assert!(!err.outcome_unknown());
    }
}
