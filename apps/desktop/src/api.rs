//! HTTP client for the else.bot API.
//!
//! Requests run on a private Tokio runtime, away from GPUI's render path.
//! Redirects are never followed, so the bearer token can only reach the one
//! configured origin; it is attached as a sensitive header, never a URL.
//! HTTPS and WSS share one TLS configuration that verifies certificates
//! with the platform's trust store.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

mod attachments;
mod live;
pub use live::{Live, LiveCommand, LiveCommands, LiveEvent, StreamMessage};
mod desktop;
pub use desktop::{DesktopCommand, DesktopCommands, DesktopEvent, DesktopMode, MAX_TEXT_CHARS};
use serde_json::json;

use crate::avatar::Avatar;
use crate::config::{Origin, OwnerToken};
use crate::model::{Item, Reaction, Routine, Snapshot, Turn};

/// The complete create payload, retained unchanged when delivery is uncertain.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CreateBotRequest {
    pub request_id: Uuid,
    pub name: String,
    pub profile: Option<ProfileChoice>,
    pub category_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avatar: Option<Avatar>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderInfo {
    pub id: String,
    pub label: String,
    /// Whether the provider's CLI runs on the API's host (older APIs do
    /// not say: assume it does).
    #[serde(default = "yes")]
    pub cli_available: bool,
    #[serde(default)]
    pub cli_unavailable_reason: Option<String>,
    /// The exact models, efforts, and tiers it applies; empty for a
    /// provider without models (it is not offered in the pickers).
    #[serde(default)]
    pub catalog: Catalog,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Catalog {
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub featured_models: Vec<String>,
    #[serde(default)]
    pub efforts: Vec<String>,
    #[serde(default)]
    pub tiers: Vec<String>,
}

/// A provider and an exact profile for it, as the API takes and reports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct ProfileChoice {
    pub provider_id: String,
    pub model: String,
    pub reasoning_effort: String,
    pub service_tier: String,
}

/// The profile a bot's next messages use: its own (`own`), else the
/// defaults it follows.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BotProfile {
    #[serde(flatten)]
    pub choice: ProfileChoice,
    pub own: bool,
}

#[derive(Deserialize)]
struct BotProfileEnvelope<P> {
    profile: P,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Limits {
    pub max_text_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Capabilities {
    pub providers: Vec<ProviderInfo>,
    pub limits: Limits,
    /// What new bots start with.
    #[serde(default)]
    pub defaults: Option<ProfileChoice>,
    /// The welcome screen is still to be completed.
    #[serde(default)]
    pub onboarding_required: bool,
    #[serde(default)]
    pub bot_avatar_on_create: bool,
}

#[derive(Deserialize)]
struct Onboarded {
    main_bot_id: Uuid,
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

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SidebarStatus {
    WaitingForAnswer {
        preview: String,
    },
    NeedsApproval {
        preview: String,
    },
    NeedsCredentials {
        preview: String,
    },
    Failed,
    Stopped,
    Uncertain,
    #[serde(other)]
    Unknown,
}

/// An active bot, or a deletion receipt (`cleanup` set).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Bot {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub metadata_revision: Option<i64>,
    /// Absent from older servers, which draw every bot the same.
    #[serde(default)]
    pub avatar: Avatar,
    #[serde(default)]
    pub conversation_id: Option<Uuid>,
    #[serde(default)]
    pub selected_provider_id: Option<String>,
    #[serde(default)]
    pub activity: Option<Activity>,
    #[serde(default)]
    pub cleanup: Option<Cleanup>,
    /// Its sidebar category; absent (older servers, receipts) or null means
    /// unassigned.
    #[serde(default)]
    pub category_id: Option<Uuid>,
    /// Absent from receipts and older servers.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    /// The newest owner or bot message; `None` before the first one and
    /// from older servers.
    #[serde(default)]
    pub last_message_at: Option<DateTime<Utc>>,
    /// Absent from older servers and on receipts.
    #[serde(default)]
    pub read_state: Option<ReadState>,
    #[serde(default)]
    pub sidebar_status: Option<SidebarStatus>,
}

/// A point in a bot's replies: the assistant item, then the UTF-8 bytes of
/// its text. Field order makes the derived ordering lexicographic.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, Deserialize,
)]
pub struct ReplyPosition {
    pub item_seq: i64,
    pub text_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LatestReply {
    pub turn_id: Uuid,
    pub item_id: Uuid,
    pub position: ReplyPosition,
    #[serde(default)]
    pub preview: String,
}

/// The owner's saved read state for a bot's owner conversation.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ReadState {
    pub latest: Option<LatestReply>,
    pub read_through: ReplyPosition,
    pub marked_unread: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReadCommand {
    Through { position: ReplyPosition },
    MarkRead { position: Option<ReplyPosition> },
    MarkUnread,
}

/// A sidebar category (POC-030).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Category {
    pub id: Uuid,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CategoryEnvelope {
    category: Category,
}

impl Bot {
    pub fn is_active(&self) -> bool {
        self.cleanup.is_none()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotList {
    pub default_bot_id: Option<Uuid>,
    /// Absent from servers without categories.
    #[serde(default)]
    pub categories: Vec<Category>,
    pub bots: Vec<Bot>,
}

#[derive(Debug, Clone, Deserialize)]
struct RoutineEnvelope {
    routine: Routine,
}

#[derive(Debug, Clone, Deserialize)]
struct BotEnvelope {
    bot: Bot,
}

/// The server's canonical owner item and turn for an accepted message.
#[derive(Debug, Clone, Deserialize)]
pub struct Sent {
    pub item: Item,
    pub turn: Turn,
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
    #[error("The API rejected the owner token. Use Change server to update it.")]
    Unauthorized,
    #[error("Cannot reach the API at {origin} ({cause}). Is `else-api serve` running?")]
    Unreachable { origin: String, cause: String },
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
    #[cfg(test)]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }

    pub fn is_request_conflict(&self) -> bool {
        matches!(self, Self::Api { code, .. } if code == "request_conflict")
    }

    /// Whether the outcome of a mutation is unknown (the request may or may
    /// not have been applied), as opposed to definitely rejected.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Unreachable { .. } | Self::Protocol(_) => true,
            // Refused before acceptance, so nothing was accepted.
            Self::Api { code, .. } if code == "provider_unavailable" => false,
            Self::Api { status, .. } => *status >= 500,
            Self::Unauthorized => false,
        }
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    tls: Arc<rustls::ClientConfig>,
    origin: Origin,
    auth: HeaderValue,
    runtime: tokio::runtime::Handle,
}

impl Client {
    pub fn new(
        origin: Origin,
        token: &OwnerToken,
        runtime: tokio::runtime::Handle,
    ) -> Result<Self, String> {
        use rustls_platform_verifier::BuilderVerifierExt as _;
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|builder| builder.with_platform_verifier())
            .map(|builder| builder.with_no_client_auth())
            .map_err(|err| format!("Cannot set up TLS: {err}"))?;
        let http = reqwest::Client::builder()
            .tls_backend_preconfigured(tls.clone())
            .redirect(reqwest::redirect::Policy::none())
            // Never route the owner token through an environment proxy.
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|err| format!("Cannot set up the HTTP client: {err}"))?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
            .expect("token is a valid header value");
        auth.set_sensitive(true);
        Ok(Self {
            http,
            tls: Arc::new(tls),
            origin,
            auth,
            runtime,
        })
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
            .timeout(Duration::from_secs(15))
            .header(AUTHORIZATION, self.auth.clone())
    }

    pub(crate) async fn send_json<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<(StatusCode, T), ClientError> {
        let response = request.send().await.map_err(|err| {
            if err.is_connect() || err.is_timeout() || err.is_request() {
                ClientError::Unreachable {
                    origin: self.origin.as_str().to_owned(),
                    cause: root_cause(&err),
                }
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

    async fn call<T: DeserializeOwned + Send + 'static>(
        self: Arc<Self>,
        method: Method,
        path: String,
        body: Option<serde_json::Value>,
    ) -> Result<(StatusCode, T), ClientError> {
        let this = self.clone();
        self.run(async move {
            let mut request = this.request(method, &path);
            if let Some(body) = body {
                request = request.json(&body);
            }
            this.send_json(request).await
        })
        .await
    }

    pub async fn snapshot(
        self: Arc<Self>,
        bot: Uuid,
        before_seq: Option<i64>,
    ) -> Result<Snapshot, ClientError> {
        let path = match before_seq {
            Some(seq) => format!("/v1/bots/{bot}/conversation?limit=50&before_seq={seq}"),
            None => format!("/v1/bots/{bot}/conversation?limit=50"),
        };
        self.call(Method::GET, path, None).await.map(|(_, s)| s)
    }

    pub async fn send(
        self: Arc<Self>,
        bot: Uuid,
        request_id: Uuid,
        text: String,
        provider_id: Option<String>,
        reaction: Option<Reaction>,
        parts: Vec<crate::attachments::MessagePart>,
        email_draft: Option<crate::model::EmailAttachment>,
    ) -> Result<Sent, ClientError> {
        let mut body =
            json!({ "request_id": request_id, "text": text, "provider_id": provider_id });
        if let Some(reaction) = reaction {
            body["reaction"] = json!(reaction);
        }
        if !parts.is_empty() {
            body["parts"] = json!(parts);
        }
        if let Some(draft) = email_draft {
            body["email_draft"] = json!(draft);
        }
        self.call(Method::POST, format!("/v1/bots/{bot}/turns"), Some(body))
            .await
            .map(|(_, sent)| sent)
    }

    pub async fn approve_email(
        self: Arc<Self>,
        bot: Uuid,
        draft: Uuid,
        edit: crate::model::EmailEdit,
    ) -> Result<crate::model::EmailDraft, ClientError> {
        self.email_action(bot, draft, "approve", Some(json!(edit)))
            .await
    }

    pub async fn discard_email(
        self: Arc<Self>,
        bot: Uuid,
        draft: Uuid,
    ) -> Result<crate::model::EmailDraft, ClientError> {
        self.email_action(bot, draft, "discard", None).await
    }

    async fn email_action(
        self: Arc<Self>,
        bot: Uuid,
        draft: Uuid,
        action: &str,
        body: Option<serde_json::Value>,
    ) -> Result<crate::model::EmailDraft, ClientError> {
        #[derive(Deserialize)]
        struct Envelope {
            draft: crate::model::EmailDraft,
        }
        self.call::<Envelope>(
            Method::POST,
            format!("/v1/bots/{bot}/email-drafts/{draft}/{action}"),
            body,
        )
        .await
        .map(|(_, response)| response.draft)
    }

    /// Resolve a send whose response was lost. `Ok(None)` means the server
    /// has no record of it, so it was not accepted.
    pub async fn by_request(
        self: Arc<Self>,
        bot: Uuid,
        request_id: Uuid,
    ) -> Result<Option<Sent>, ClientError> {
        let path = format!("/v1/bots/{bot}/turns/by-request/{request_id}");
        match self.call(Method::GET, path, None).await {
            Ok((_, sent)) => Ok(Some(sent)),
            Err(ClientError::Api { status: 404, .. }) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub async fn cancel(
        self: Arc<Self>,
        bot: Uuid,
        request_id: Uuid,
    ) -> Result<String, ClientError> {
        let body = json!({ "request_id": request_id });
        self.call::<serde_json::Value>(Method::POST, format!("/v1/bots/{bot}/cancel"), Some(body))
            .await
            .map(|(_, v)| v["state"].as_str().unwrap_or_default().to_owned())
    }

    /// Answer a bot's secret request: store `env` (exactly the requested
    /// variables) or, with `None`, decline it. Values are never returned.
    pub async fn answer_secret(
        self: Arc<Self>,
        bot: Uuid,
        request: Uuid,
        env: Option<std::collections::BTreeMap<String, String>>,
    ) -> Result<(), ClientError> {
        let (action, body) = match env {
            Some(env) => ("fulfill", Some(json!({ "env": env }))),
            None => ("cancel", None),
        };
        let path = format!("/v1/bots/{bot}/secret-requests/{request}/{action}");
        self.call::<serde_json::Value>(Method::POST, path, body)
            .await
            .map(|_| ())
    }

    /// Answer a bot's question by one of its choice ids. A stale card gets
    /// 409 `question_settled` from the server and never changes anything.
    pub async fn answer_question(
        self: Arc<Self>,
        bot: Uuid,
        question: Uuid,
        choice: String,
    ) -> Result<(), ClientError> {
        let path = format!("/v1/bots/{bot}/questions/{question}/answer");
        self.call::<serde_json::Value>(Method::POST, path, Some(json!({ "choice": choice })))
            .await
            .map(|_| ())
    }

    pub async fn create_bot(
        self: Arc<Self>,
        request: CreateBotRequest,
    ) -> Result<Bot, ClientError> {
        self.call::<BotEnvelope>(Method::POST, "/v1/bots".into(), Some(json!(request)))
            .await
            .map(|(_, v)| v.bot)
    }

    pub async fn create_category(
        self: Arc<Self>,
        request_id: Uuid,
        name: String,
    ) -> Result<Category, ClientError> {
        let body = json!({ "request_id": request_id, "name": name });
        self.call::<CategoryEnvelope>(Method::POST, "/v1/categories".into(), Some(body))
            .await
            .map(|(_, v)| v.category)
    }

    pub async fn rename_category(
        self: Arc<Self>,
        id: Uuid,
        name: String,
    ) -> Result<Category, ClientError> {
        let body = json!({ "name": name });
        self.call::<CategoryEnvelope>(Method::PATCH, format!("/v1/categories/{id}"), Some(body))
            .await
            .map(|(_, v)| v.category)
    }

    pub async fn delete_category(self: Arc<Self>, id: Uuid) -> Result<(), ClientError> {
        self.call::<serde_json::Value>(Method::DELETE, format!("/v1/categories/{id}"), None)
            .await
            .map(|_| ())
    }

    /// Assign a bot to a category, or unassign it with `None`.
    pub async fn set_category(
        self: Arc<Self>,
        bot: Uuid,
        category: Option<Uuid>,
    ) -> Result<Bot, ClientError> {
        let body = json!({ "category_id": category });
        self.call::<BotEnvelope>(
            Method::PATCH,
            format!("/v1/bots/{bot}/category"),
            Some(body),
        )
        .await
        .map(|(_, v)| v.bot)
    }

    /// Change a bot's name, avatar, or both in one save; `None` keeps the
    /// current value. Saving values it already has is a no-op, so a retry
    /// after an unknown outcome is safe.
    pub async fn update_bot(
        self: Arc<Self>,
        bot: Uuid,
        name: Option<String>,
        avatar: Option<Avatar>,
    ) -> Result<Bot, ClientError> {
        let mut body = serde_json::Map::new();
        if let Some(name) = name {
            body.insert("name".into(), json!(name));
        }
        if let Some(avatar) = avatar {
            body.insert("avatar".into(), json!(avatar));
        }
        let body = serde_json::Value::Object(body);
        self.call::<BotEnvelope>(Method::PATCH, format!("/v1/bots/{bot}"), Some(body))
            .await
            .map(|(_, v)| v.bot)
    }

    /// The bot's effective profile; `None` before any defaults exist.
    pub async fn bot_profile(
        self: Arc<Self>,
        bot: Uuid,
    ) -> Result<Option<BotProfile>, ClientError> {
        self.call::<BotProfileEnvelope<Option<BotProfile>>>(
            Method::GET,
            format!("/v1/bots/{bot}/profile"),
            None,
        )
        .await
        .map(|(_, v)| v.profile)
    }

    /// Give the bot its own profile, used from its next message.
    pub async fn set_bot_profile(
        self: Arc<Self>,
        bot: Uuid,
        choice: ProfileChoice,
    ) -> Result<BotProfile, ClientError> {
        self.call::<BotProfileEnvelope<BotProfile>>(
            Method::PUT,
            format!("/v1/bots/{bot}/profile"),
            Some(json!(choice)),
        )
        .await
        .map(|(_, v)| v.profile)
    }

    /// Complete the welcome screen: name the main bot and set its provider
    /// and model (also the defaults). Returns the main bot's ID.
    pub async fn onboard(
        self: Arc<Self>,
        name: String,
        profile: ProfileChoice,
    ) -> Result<Uuid, ClientError> {
        let body = json!({ "name": name, "profile": profile });
        self.call::<Onboarded>(Method::POST, "/v1/onboarding".into(), Some(body))
            .await
            .map(|(_, done)| done.main_bot_id)
    }

    /// Switch a routine on or off. Setting its current value is a no-op, so
    /// a retry is safe; switching on schedules the next run from now.
    pub async fn set_routine_enabled(
        self: Arc<Self>,
        bot: Uuid,
        routine: Uuid,
        enabled: bool,
    ) -> Result<Routine, ClientError> {
        self.call::<RoutineEnvelope>(
            Method::PATCH,
            format!("/v1/bots/{bot}/routines/{routine}"),
            Some(json!({ "enabled": enabled })),
        )
        .await
        .map(|(_, v)| v.routine)
    }

    /// Delete a routine: it never runs again and a run already admitted
    /// finishes. Deleting it again succeeds.
    pub async fn delete_routine(
        self: Arc<Self>,
        bot: Uuid,
        routine: Uuid,
    ) -> Result<(), ClientError> {
        self.call::<serde_json::Value>(
            Method::DELETE,
            format!("/v1/bots/{bot}/routines/{routine}"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// Change the owner's read state. Every command is safe to repeat.
    pub async fn read(self: Arc<Self>, bot: Uuid, command: ReadCommand) -> Result<(), ClientError> {
        self.call::<BotEnvelope>(
            Method::POST,
            format!("/v1/bots/{bot}/read"),
            Some(json!(command)),
        )
        .await
        .map(|_| ())
    }

    pub async fn delete_bot(self: Arc<Self>, bot: Uuid) -> Result<Bot, ClientError> {
        self.call::<BotEnvelope>(Method::DELETE, format!("/v1/bots/{bot}"), None)
            .await
            .map(|(_, v)| v.bot)
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

fn root_cause(err: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = err;
    while let Some(source) = cause.source() {
        cause = source;
    }
    cause.to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::live::{route, subscriptions};
    use super::*;
    use futures_util::{SinkExt as _, StreamExt as _};
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-shot HTTP server returning `response` and capturing the request.
    pub(crate) async fn serve_once(
        response: &'static str,
    ) -> (Origin, tokio::task::JoinHandle<String>) {
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
        let token = OwnerToken::parse(&"T".repeat(43)).unwrap();
        Arc::new(Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap())
    }

    #[tokio::test]
    async fn create_sends_avatar_only_when_supported() {
        let capabilities: Capabilities = serde_json::from_value(json!({
            "providers": [], "limits": { "max_text_bytes": 1024 }
        }))
        .unwrap();
        assert!(!capabilities.bot_avatar_on_create);
        for avatar in [None, Some(Avatar::default())] {
            let (origin, server) = serve_once(
                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}",
            )
            .await;
            let _ = client(origin)
                .create_bot(CreateBotRequest {
                    request_id: Uuid::new_v4(),
                    name: "Research".into(),
                    profile: None,
                    category_id: None,
                    avatar,
                })
                .await;
            let request = server.await.unwrap();
            let (_, body) = request.split_once("\r\n\r\n").unwrap();
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            if avatar.is_some() {
                assert_eq!(body["avatar"], json!({ "shape": "star", "color": "blue" }));
            } else {
                assert!(body.get("avatar").is_none());
            }
        }
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
        assert!(matches!(err, ClientError::Unreachable { .. }), "{err:?}");
        assert!(!err.to_string().contains(&"T".repeat(43)));
    }

    async fn first_bytes() -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            buf.truncate(n);
            buf
        });
        (address, task)
    }

    #[tokio::test]
    async fn https_and_wss_origins_send_the_token_only_over_tls() {
        let token = "T".repeat(43);
        let (address, server) = first_bytes().await;
        let origin = Origin::parse(&format!("https://{address}")).unwrap();
        let err = client(origin).bots().await.unwrap_err();
        assert!(matches!(err, ClientError::Unreachable { .. }), "{err:?}");
        let sent = server.await.unwrap();
        assert_eq!(sent.first(), Some(&0x16), "a TLS handshake record");
        assert!(!String::from_utf8_lossy(&sent).contains(&token));

        let (address, server) = first_bytes().await;
        let origin = Origin::parse(&format!("https://{address}")).unwrap();
        let live = client(origin).live();
        let sent = server.await.unwrap();
        assert_eq!(sent.first(), Some(&0x16), "the WebSocket uses TLS");
        assert!(!String::from_utf8_lossy(&sent).contains(&token));
        drop(live);
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

    #[test]
    fn subscription_mailbox_coalesces_and_preserves_delivered_cursors() {
        let (tx, mut rx) = tokio::sync::watch::channel(HashMap::new());
        let commands = LiveCommands(tx);
        let old = Uuid::new_v4();
        let current = Uuid::new_v4();
        commands
            .send(LiveCommand::Subscribe {
                bot: old,
                cursor: "1".into(),
            })
            .unwrap();
        commands
            .send(LiveCommand::Unsubscribe { bot: old })
            .unwrap();
        commands
            .send(LiveCommand::Subscribe {
                bot: current,
                cursor: "2".into(),
            })
            .unwrap();
        let mut wanted = HashMap::new();
        let mut applied = HashMap::new();
        let changes = subscriptions(&mut wanted, &mut applied, rx.borrow_and_update().clone());
        assert!(matches!(&changes[..], [LiveCommand::Subscribe { bot, .. }] if *bot == current));
        assert!(!wanted.contains_key(&old));
        wanted.insert(current, "3".into());
        assert!(subscriptions(&mut wanted, &mut applied, rx.borrow().clone()).is_empty());
        assert_eq!(
            wanted[&current], "3",
            "reconnect keeps the latest delivered cursor"
        );
        commands
            .send(LiveCommand::Unsubscribe { bot: current })
            .unwrap();
        let changes = subscriptions(&mut wanted, &mut applied, rx.borrow_and_update().clone());
        assert!(matches!(&changes[..], [LiveCommand::Unsubscribe { bot }] if *bot == current));
        assert!(wanted.is_empty());
    }

    #[test]
    fn routing_tracks_cursors_and_maps_channel_errors() {
        let bot = Uuid::new_v4();
        let mut wanted = HashMap::from([(bot, "i:b:1".to_owned())]);
        let frame = json!({ "channel": format!("bot:{bot}"), "id": "i:b:2", "event": "item.delta", "data": { "x": 1 } });
        match route(&frame, &mut wanted) {
            Some(LiveEvent::Bot {
                message: StreamMessage::Frame(f),
                ..
            }) => {
                assert_eq!(f.id.as_deref(), Some("i:b:2"));
                assert_eq!(f.event, "item.delta");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(wanted[&bot], "i:b:2", "resume cursor advanced");
        let reset = json!({ "channel": format!("bot:{bot}"), "error": { "code": "cursor_reset_required" } });
        assert!(matches!(
            route(&reset, &mut wanted),
            Some(LiveEvent::Bot {
                message: StreamMessage::Reset,
                ..
            })
        ));
        assert!(
            !wanted.contains_key(&bot),
            "a failed channel is not resubscribed"
        );
        let deleted =
            json!({ "channel": format!("bot:{bot}"), "error": { "code": "bot_deleted" } });
        assert!(matches!(
            route(&deleted, &mut wanted),
            Some(LiveEvent::Bot {
                message: StreamMessage::Deleted,
                ..
            })
        ));
        let bots = json!({ "channel": "bots", "event": "bots.snapshot", "data": { "default_bot_id": null, "bots": [] } });
        assert!(matches!(
            route(&bots, &mut wanted),
            Some(LiveEvent::Bots(_))
        ));
    }

    #[tokio::test]
    async fn live_connection_authenticates_and_resumes_after_reconnect() {
        use tokio_tungstenite::tungstenite::Message;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let bot = Uuid::new_v4();
        let (seen_tx, mut seen) = tokio::sync::mpsc::channel::<String>(16);
        tokio::spawn(async move {
            for round in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let seen_tx = seen_tx.clone();
                let mut socket = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     response| {
                        let auth = request.headers()["authorization"]
                            .to_str()
                            .unwrap()
                            .to_owned();
                        let _ = seen_tx.try_send(format!("auth {auth}"));
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                // Read the client's subscriptions for a moment.
                while let Ok(Some(Ok(Message::Text(text)))) =
                    tokio::time::timeout(Duration::from_millis(300), socket.next()).await
                {
                    let _ = seen_tx.send(text.to_string()).await;
                }
                if round == 0 {
                    let frame = json!({ "channel": format!("bot:{bot}"), "id": "i:b:7", "event": "turn.updated", "data": {} });
                    socket
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .unwrap();
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    // Drop the connection: the client must reconnect.
                }
            }
        });
        let client = client(origin);
        let mut live = client.live();
        assert!(matches!(
            live.events.recv().await,
            Some(LiveEvent::Connected)
        ));
        live.commands
            .send(LiveCommand::Subscribe {
                bot,
                cursor: "i:b:5".into(),
            })
            .unwrap();
        assert!(matches!(
            live.events.recv().await,
            Some(LiveEvent::Bot { .. })
        ));
        assert!(matches!(
            live.events.recv().await,
            Some(LiveEvent::Disconnected(_))
        ));
        let dropped = tokio::time::Instant::now();
        assert!(matches!(
            live.events.recv().await,
            Some(LiveEvent::Connected)
        ));
        // A connection that dropped quickly is retried after a backoff.
        assert!(dropped.elapsed() >= Duration::from_millis(500));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut log = Vec::new();
        while let Ok(line) = seen.try_recv() {
            log.push(line);
        }
        let token = format!("auth Bearer {}", "T".repeat(43));
        assert_eq!(log.iter().filter(|l| **l == token).count(), 2, "{log:?}");
        assert!(
            log.iter().any(|l| l.contains("\"cursor\":\"i:b:5\"")),
            "{log:?}"
        );
        assert!(
            log.iter().any(|l| l.contains("\"cursor\":\"i:b:7\"")),
            "resubscribed from the last delivered cursor: {log:?}"
        );
    }
}
