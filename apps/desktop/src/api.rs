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

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::json;

use crate::config::{Origin, OwnerToken};
use crate::model::Frame;
use crate::model::{Item, Snapshot, Turn};
use std::collections::HashMap;

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
    /// Its sidebar category; absent (older servers, receipts) or null means
    /// unassigned.
    #[serde(default)]
    pub category_id: Option<Uuid>,
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
struct BotEnvelope {
    bot: Bot,
}

/// The server's canonical owner item and turn for an accepted message.
#[derive(Debug, Clone, Deserialize)]
pub struct Sent {
    pub item: Item,
    pub turn: Turn,
}

/// What a `bot:<id>` channel delivered.
#[derive(Debug, Clone)]
pub enum StreamMessage {
    Frame(Frame),
    /// The cursor is no longer valid: load a fresh snapshot.
    Reset,
    /// The bot was deleted.
    Deleted,
}

/// Events from the live connection.
#[derive(Debug, Clone)]
pub enum LiveEvent {
    Connected,
    /// Retrying after this problem; loaded data stays visible.
    Disconnected(String),
    Unauthorized,
    /// The `bots` channel: the current list and default pointer.
    Bots(BotList),
    Bot {
        bot: Uuid,
        message: StreamMessage,
    },
}

pub enum LiveCommand {
    Subscribe { bot: Uuid, cursor: String },
    Unsubscribe { bot: Uuid },
}

/// A capacity-one mailbox of desired subscriptions. Intermediate UI changes
/// coalesce while the socket is busy or disconnected; the latest state wins.
#[derive(Clone)]
pub struct LiveCommands(tokio::sync::watch::Sender<HashMap<Uuid, String>>);

impl LiveCommands {
    pub fn send(
        &self,
        command: LiveCommand,
    ) -> Result<(), tokio::sync::watch::error::SendError<LiveCommand>> {
        if self.0.is_closed() {
            return Err(tokio::sync::watch::error::SendError(command));
        }
        self.0.send_modify(|wanted| match command {
            LiveCommand::Subscribe { bot, cursor } => {
                wanted.insert(bot, cursor);
            }
            LiveCommand::Unsubscribe { bot } => {
                wanted.remove(&bot);
            }
        });
        Ok(())
    }
}

fn subscriptions(
    wanted: &mut HashMap<Uuid, String>,
    applied: &mut HashMap<Uuid, String>,
    desired: HashMap<Uuid, String>,
) -> Vec<LiveCommand> {
    let mut changes = Vec::new();
    wanted.retain(|bot, _| {
        if desired.contains_key(bot) {
            true
        } else {
            changes.push(LiveCommand::Unsubscribe { bot: *bot });
            false
        }
    });
    for (bot, cursor) in &desired {
        if applied.get(bot) != Some(cursor) || !wanted.contains_key(bot) {
            wanted.insert(*bot, cursor.clone());
            changes.push(LiveCommand::Subscribe {
                bot: *bot,
                cursor: cursor.clone(),
            });
        }
    }
    *applied = desired;
    changes
}

/// Handles to the live connection; dropping `events` ends it.
pub struct Live {
    pub commands: LiveCommands,
    pub events: tokio::sync::mpsc::Receiver<LiveEvent>,
}

/// Turn one server message into a UI event, tracking resume cursors.
fn route(message: &serde_json::Value, wanted: &mut HashMap<Uuid, String>) -> Option<LiveEvent> {
    let channel = message["channel"].as_str()?;
    if channel == "bots" {
        return serde_json::from_value(message["data"].clone())
            .ok()
            .map(LiveEvent::Bots);
    }
    let bot: Uuid = channel.strip_prefix("bot:")?.parse().ok()?;
    if let Some(code) = message["error"]["code"].as_str() {
        wanted.remove(&bot);
        let message = match code {
            "bot_deleted" | "not_found" => StreamMessage::Deleted,
            _ => StreamMessage::Reset,
        };
        return Some(LiveEvent::Bot { bot, message });
    }
    let id = message["id"].as_str()?.to_owned();
    // Only resume from cursors of channels still wanted.
    if let Some(cursor) = wanted.get_mut(&bot) {
        *cursor = id.clone();
    }
    Some(LiveEvent::Bot {
        bot,
        message: StreamMessage::Frame(Frame {
            id: Some(id),
            event: message["event"].as_str().unwrap_or_default().to_owned(),
            data: message["data"].to_string(),
        }),
    })
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
            Self::Unreachable(_) | Self::Protocol(_) => true,
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
    origin: Origin,
    auth: HeaderValue,
    runtime: tokio::runtime::Handle,
}

impl Client {
    pub fn new(origin: Origin, token: &OwnerToken, runtime: tokio::runtime::Handle) -> Self {
        // A workspace build shares reqwest's Rustls features with the API.
        // The provider must be installed even for our loopback HTTP client.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // Never route the owner token through an environment proxy.
            .no_proxy()
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
    ) -> Result<Sent, ClientError> {
        let body = json!({ "request_id": request_id, "text": text, "provider_id": provider_id });
        self.call(Method::POST, format!("/v1/bots/{bot}/turns"), Some(body))
            .await
            .map(|(_, sent)| sent)
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

    pub async fn create_bot(
        self: Arc<Self>,
        request_id: Uuid,
        name: String,
        role: String,
        profile: Option<ProfileChoice>,
    ) -> Result<Bot, ClientError> {
        let mut body = json!({ "request_id": request_id, "name": name, "role": role });
        if let Some(profile) = profile {
            body["profile"] = json!(profile);
        }
        self.call::<BotEnvelope>(Method::POST, "/v1/bots".into(), Some(body))
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

    pub async fn delete_bot(self: Arc<Self>, bot: Uuid) -> Result<Bot, ClientError> {
        self.call::<BotEnvelope>(Method::DELETE, format!("/v1/bots/{bot}"), None)
            .await
            .map(|(_, v)| v.bot)
    }

    /// Open the live connection: one WebSocket carrying the `bots` channel
    /// and any `bot:<id>` channels the UI subscribes to. It reconnects with
    /// backoff and resubscribes every channel from its last delivered cursor.
    pub fn live(self: Arc<Self>) -> Live {
        let (commands, command_rx) = tokio::sync::watch::channel(HashMap::new());
        // Bounded: a slow UI applies backpressure to the socket instead of
        // buffering an unbounded backlog in memory.
        let (event_tx, events) = tokio::sync::mpsc::channel(256);
        let this = self.clone();
        self.runtime
            .spawn(async move { this.run_live(command_rx, event_tx).await });
        Live {
            commands: LiveCommands(commands),
            events,
        }
    }

    fn ws_request(&self) -> tokio_tungstenite::tungstenite::handshake::client::Request {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let url = format!(
            "ws://{}/v1/ws",
            self.origin.as_str().trim_start_matches("http://")
        );
        let mut request = url.into_client_request().expect("valid ws URL");
        request
            .headers_mut()
            .insert(AUTHORIZATION, self.auth.clone());
        request
    }

    async fn run_live(
        &self,
        mut commands: tokio::sync::watch::Receiver<HashMap<Uuid, String>>,
        events: tokio::sync::mpsc::Sender<LiveEvent>,
    ) {
        use tokio_tungstenite::tungstenite::{Error as WsError, Message};
        /// Longer than the server's 15-second ping.
        const INACTIVITY: Duration = Duration::from_secs(45);
        const HANDSHAKE: Duration = Duration::from_secs(10);
        /// A connection that lasted this long resets the reconnect backoff.
        const STABLE: Duration = Duration::from_secs(30);
        // Wanted `bot:<id>` channels and the cursor to resume from.
        let mut wanted: HashMap<Uuid, String> = HashMap::new();
        let mut applied = HashMap::new();
        let mut backoff = Duration::from_millis(500);
        macro_rules! deliver {
            ($event:expr) => {
                tokio::select! {
                    _ = events.closed() => return,
                    sent = events.send($event) => if sent.is_err() { return },
                }
            };
        }
        // Wait with jittered exponential backoff (500 ms to 15 s).
        macro_rules! pause {
            () => {
                let jitter = Duration::from_millis(u64::from(Uuid::new_v4().as_bytes()[0]) * 2);
                tokio::select! {
                    _ = events.closed() => return,
                    _ = tokio::time::sleep(backoff + jitter) => {}
                }
                backoff = (backoff * 2).min(Duration::from_secs(15));
            };
        }
        loop {
            let connected = tokio::select! {
                _ = events.closed() => return,
                result = tokio::time::timeout(
                    HANDSHAKE,
                    tokio_tungstenite::connect_async(self.ws_request()),
                ) => result,
            };
            let mut socket = match connected {
                Ok(Ok((socket, _))) => socket,
                Ok(Err(WsError::Http(response)))
                    if response.status() == StatusCode::UNAUTHORIZED =>
                {
                    deliver!(LiveEvent::Unauthorized);
                    // Retrying a rejected token cannot help; check rarely.
                    tokio::select! {
                        _ = events.closed() => return,
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    }
                    continue;
                }
                Ok(Err(_)) | Err(_) => {
                    deliver!(LiveEvent::Disconnected(format!(
                        "Cannot reach the API at {}.",
                        self.origin.as_str()
                    )));
                    pause!();
                    continue;
                }
            };
            let connected_at = tokio::time::Instant::now();
            let subscribe = |channel: String, cursor: Option<&String>| {
                Message::Text(
                    json!({ "op": "subscribe", "channel": channel, "cursor": cursor })
                        .to_string()
                        .into(),
                )
            };
            subscriptions(
                &mut wanted,
                &mut applied,
                commands.borrow_and_update().clone(),
            );
            let mut ok = socket.send(subscribe("bots".into(), None)).await.is_ok();
            for (bot, cursor) in &wanted {
                ok = ok
                    && socket
                        .send(subscribe(format!("bot:{bot}"), Some(cursor)))
                        .await
                        .is_ok();
            }
            if ok {
                deliver!(LiveEvent::Connected);
            }
            let reason = loop {
                if !ok {
                    break "The live connection failed.";
                }
                tokio::select! {
                    _ = events.closed() => return,
                    changed = commands.changed() => {
                        if changed.is_err() { return; }
                        let desired = commands.borrow_and_update().clone();
                        for command in subscriptions(&mut wanted, &mut applied, desired) {
                            let message = match command {
                                LiveCommand::Subscribe { bot, cursor } => {
                                    let message = subscribe(format!("bot:{bot}"), Some(&cursor));
                                    wanted.insert(bot, cursor);
                                    message
                                }
                                LiveCommand::Unsubscribe { bot } => {
                                    wanted.remove(&bot);
                                    Message::Text(
                                        json!({ "op": "unsubscribe", "channel": format!("bot:{bot}") })
                                            .to_string()
                                            .into(),
                                    )
                                }
                            };
                            if socket.send(message).await.is_err() {
                                ok = false;
                                break;
                            }
                        }
                    }
                    incoming = tokio::time::timeout(INACTIVITY, socket.next()) => {
                        let text = match incoming {
                            Err(_) => break "The live connection went silent.",
                            Ok(None) | Ok(Some(Err(_))) => break "The live connection closed.",
                            Ok(Some(Ok(Message::Text(text)))) => text,
                            Ok(Some(Ok(Message::Close(_)))) => break "The live connection closed.",
                            Ok(Some(Ok(_))) => continue,
                        };
                        let Ok(message) = serde_json::from_str::<serde_json::Value>(&text) else {
                            continue;
                        };
                        if let Some(event) = route(&message, &mut wanted) {
                            deliver!(event);
                        }
                    }
                }
            };
            deliver!(LiveEvent::Disconnected(reason.into()));
            // A server that accepts and then drops connections must not be
            // hammered: only a connection that stayed up resets the backoff.
            if connected_at.elapsed() >= STABLE {
                backoff = Duration::from_millis(500);
            }
            pause!();
        }
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
