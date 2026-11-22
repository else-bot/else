//! The else.bot API client.
//!
//! Requests run on a private Tokio runtime, away from NSH's engine thread;
//! [`run`] hands each result back as a future the engine awaits. Redirects
//! are never followed, so the key can only reach the one saved origin; it
//! travels as a sensitive header, never in a URL. HTTPS and WSS verify the
//! certificate with the platform's trust store.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use else_wire::bots::BotList;
use else_wire::conversation::{Sent, Snapshot};
use else_wire::settings::ErrorBody;
use futures_util::{SinkExt as _, StreamExt as _};
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::json;
use uuid::Uuid;

use crate::server::{Origin, Profile};

fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("else-net")
            .enable_all()
            .build()
            .expect("the network runtime starts")
    })
}

/// Runs `work` on the network runtime; the engine awaits its result.
pub fn run<T: Send + 'static>(
    work: impl Future<Output = T> + Send + 'static,
) -> impl Future<Output = T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let task = runtime().spawn(async move {
        let _ = tx.send(work.await);
    });
    let guard = AbortOnDrop(task.abort_handle());
    async move {
        let value = rx.await.expect("the network task finished");
        drop(guard);
        value
    }
}

struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Errors shown to the owner. None of them contain the key.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClientError {
    #[error("The server rejected the owner key.")]
    Unauthorized,
    #[error("Cannot reach {origin}. {cause}")]
    Unreachable { origin: String, cause: String },
    #[error("{0}")]
    Transport(String),
    #[error("{message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },
    #[error("Unexpected response from the server: {0}")]
    Protocol(String),
}

impl ClientError {
    fn from_http(status: StatusCode, bytes: &[u8]) -> Self {
        match serde_json::from_slice::<ErrorBody>(bytes) {
            Ok(body) => Self::Api {
                status: status.as_u16(),
                code: body.error.code,
                message: body.error.message,
            },
            Err(_) => Self::Protocol(format!("HTTP {status}")),
        }
    }

    /// Whether a send may or may not have reached the server.
    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Unreachable { .. } | Self::Protocol(_) => true,
            Self::Api { status, .. } => *status >= 500,
            Self::Unauthorized | Self::Transport(_) => false,
        }
    }
}

pub struct Client {
    http: reqwest::Client,
    tls: Arc<rustls::ClientConfig>,
    origin: Origin,
    auth: HeaderValue,
}

impl Client {
    pub fn new(profile: &Profile) -> Result<Arc<Self>, String> {
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
            .no_proxy()
            .connect_timeout(Duration::from_secs(8))
            .build()
            .map_err(|err| format!("Cannot set up the HTTP client: {err}"))?;
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", profile.token.expose()))
            .expect("a validated key is a valid header value");
        auth.set_sensitive(true);
        Ok(Arc::new(Self {
            http,
            tls: Arc::new(tls),
            origin: profile.origin.clone(),
            auth,
        }))
    }

    fn verify_transport(&self) -> Result<(), ClientError> {
        if self.origin.tailscale_ip().is_some() && !crate::server::tailnet_connected() {
            return Err(ClientError::Transport(
                "Connect Tailscale on this iPhone first. The key is only sent over your tailnet."
                    .into(),
            ));
        }
        Ok(())
    }

    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<T, ClientError> {
        self.verify_transport()?;
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.origin.as_str()))
            .timeout(Duration::from_secs(20))
            .header(AUTHORIZATION, self.auth.clone());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|err| {
            if err.is_connect() || err.is_timeout() || err.is_request() {
                ClientError::Unreachable {
                    origin: self.origin.display().to_owned(),
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
                "redirect ({status}) refused; the key is never sent to another origin"
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|err| ClientError::Protocol(err.without_url().to_string()))?;
        if !status.is_success() {
            return Err(ClientError::from_http(status, &bytes));
        }
        let body = if bytes.is_empty() {
            b"null".as_slice()
        } else {
            bytes.as_ref()
        };
        serde_json::from_slice(body).map_err(|err| ClientError::Protocol(err.to_string()))
    }

    pub async fn bots(self: Arc<Self>) -> Result<BotList, ClientError> {
        run(async move { self.call(Method::GET, "/v1/bots", None).await }).await
    }

    pub async fn latest(self: Arc<Self>, bot: Uuid) -> Result<Snapshot, ClientError> {
        let path = format!("/v1/bots/{bot}/conversation?limit=50");
        run(async move { self.call(Method::GET, &path, None).await }).await
    }

    pub async fn older(self: Arc<Self>, bot: Uuid, before: i64) -> Result<Snapshot, ClientError> {
        let path = format!("/v1/bots/{bot}/conversation?limit=50&before_seq={before}");
        run(async move { self.call(Method::GET, &path, None).await }).await
    }

    pub async fn send(
        self: Arc<Self>,
        bot: Uuid,
        request_id: Uuid,
        text: String,
    ) -> Result<Sent, ClientError> {
        let body = json!({ "request_id": request_id, "text": text, "provider_id": null });
        let path = format!("/v1/bots/{bot}/turns");
        run(async move { self.call(Method::POST, &path, Some(body)).await }).await
    }

    /// Stops the bot's current reply.
    pub async fn cancel(self: Arc<Self>, bot: Uuid, request_id: Uuid) -> Result<(), ClientError> {
        let body = json!({ "request_id": request_id });
        let path = format!("/v1/bots/{bot}/cancel");
        run(async move {
            self.call::<serde::de::IgnoredAny>(Method::POST, &path, Some(body))
                .await
                .map(drop)
        })
        .await
    }

    /// Marks everything up to the newest message read.
    pub async fn mark_read(self: Arc<Self>, bot: Uuid) -> Result<(), ClientError> {
        let body = json!({ "action": "mark_read", "position": null });
        let path = format!("/v1/bots/{bot}/read");
        run(async move {
            self.call::<serde::de::IgnoredAny>(Method::POST, &path, Some(body))
                .await
                .map(drop)
        })
        .await
    }
}

fn root_cause(err: &reqwest::Error) -> String {
    let mut cause: &dyn std::error::Error = err;
    while let Some(source) = cause.source() {
        cause = source;
    }
    let text = cause.to_string();
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!(
            "{}{}.",
            first.to_uppercase(),
            chars.as_str().trim_end_matches('.')
        ),
        None => String::new(),
    }
}

/// Events from the live connection.
#[derive(Debug, Clone)]
pub enum LiveEvent {
    Connected,
    /// Retrying after this problem; loaded data stays visible.
    Disconnected(String),
    Unauthorized,
    /// The `bots` channel: the current list.
    Bots(BotList),
    /// One event of a `bot:<id>` channel, with its cursor.
    Bot {
        bot: Uuid,
        id: String,
        message: serde_json::Value,
    },
    /// The bot's cursor is no longer valid, or it was deleted.
    BotReset {
        bot: Uuid,
    },
}

/// Handles to the live connection; dropping `events` ends it.
pub struct Live {
    /// The `bot:<id>` channels wanted, each from its cursor.
    pub wanted: tokio::sync::watch::Sender<HashMap<Uuid, String>>,
    pub wake: Arc<tokio::sync::Notify>,
    pub events: tokio::sync::mpsc::Receiver<LiveEvent>,
}

fn route(message: &serde_json::Value, wanted: &mut HashMap<Uuid, String>) -> Option<LiveEvent> {
    let channel = message["channel"].as_str()?;
    if channel == "bots" {
        return serde_json::from_value(message["data"].clone())
            .ok()
            .map(LiveEvent::Bots);
    }
    let bot: Uuid = channel.strip_prefix("bot:")?.parse().ok()?;
    if message["error"]["code"].as_str().is_some() {
        wanted.remove(&bot);
        return Some(LiveEvent::BotReset { bot });
    }
    let id = message["id"].as_str()?.to_owned();
    if let Some(cursor) = wanted.get_mut(&bot) {
        *cursor = id.clone();
    }
    Some(LiveEvent::Bot {
        bot,
        id,
        message: message.clone(),
    })
}

impl Client {
    /// Opens the live connection: one WebSocket carrying the `bots` channel
    /// and the `bot:<id>` channels in `wanted`. It reconnects with backoff
    /// and resubscribes each channel from its last delivered cursor.
    pub fn live(self: Arc<Self>) -> Live {
        let (wanted, wanted_rx) = tokio::sync::watch::channel(HashMap::new());
        let (event_tx, events) = tokio::sync::mpsc::channel(256);
        let wake = Arc::new(tokio::sync::Notify::new());
        let woken = wake.clone();
        runtime().spawn(async move { self.run_live(wanted_rx, event_tx, woken).await });
        Live {
            wanted,
            wake,
            events,
        }
    }

    fn ws_request(&self) -> tokio_tungstenite::tungstenite::handshake::client::Request {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = self
            .origin
            .ws_url()
            .into_client_request()
            .expect("valid ws URL");
        request
            .headers_mut()
            .insert(AUTHORIZATION, self.auth.clone());
        request
    }

    async fn run_live(
        &self,
        mut desired: tokio::sync::watch::Receiver<HashMap<Uuid, String>>,
        events: tokio::sync::mpsc::Sender<LiveEvent>,
        wake: Arc<tokio::sync::Notify>,
    ) {
        use tokio_tungstenite::tungstenite::{Error as WsError, Message};
        const INACTIVITY: Duration = Duration::from_secs(45);
        const HANDSHAKE: Duration = Duration::from_secs(10);
        const STABLE: Duration = Duration::from_secs(30);
        let mut wanted: HashMap<Uuid, String> = HashMap::new();
        let mut requested: HashMap<Uuid, String> = HashMap::new();
        let mut backoff = Duration::from_millis(500);
        macro_rules! deliver {
            ($event:expr) => {
                tokio::select! {
                    _ = events.closed() => return,
                    sent = events.send($event) => if sent.is_err() { return },
                }
            };
        }
        macro_rules! pause {
            () => {
                backoff = tokio::select! {
                    _ = events.closed() => return,
                    _ = wake.notified() => Duration::from_millis(500),
                    _ = tokio::time::sleep(backoff) => (backoff * 2).min(Duration::from_secs(15)),
                };
            };
        }
        let subscribe = |channel: String, cursor: Option<&String>| {
            Message::Text(
                json!({ "op": "subscribe", "channel": channel, "cursor": cursor })
                    .to_string()
                    .into(),
            )
        };
        loop {
            if let Err(err) = self.verify_transport() {
                deliver!(LiveEvent::Disconnected(err.to_string()));
                pause!();
                continue;
            }
            let connected = tokio::select! {
                _ = events.closed() => return,
                result = tokio::time::timeout(
                    HANDSHAKE,
                    tokio_tungstenite::connect_async_tls_with_config(
                        self.ws_request(),
                        None,
                        false,
                        Some(tokio_tungstenite::Connector::Rustls(self.tls.clone())),
                    ),
                ) => result,
            };
            let mut socket = match connected {
                Ok(Ok((socket, _))) => socket,
                Ok(Err(WsError::Http(response)))
                    if response.status() == StatusCode::UNAUTHORIZED =>
                {
                    deliver!(LiveEvent::Unauthorized);
                    tokio::select! {
                        _ = events.closed() => return,
                        _ = wake.notified() => {}
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                    }
                    continue;
                }
                Ok(Err(_)) | Err(_) => {
                    deliver!(LiveEvent::Disconnected(format!(
                        "Cannot reach {}.",
                        self.origin.display()
                    )));
                    pause!();
                    continue;
                }
            };
            let connected_at = tokio::time::Instant::now();
            // Channels already live keep their newest delivered cursor.
            let latest = desired.borrow_and_update().clone();
            wanted = latest
                .iter()
                .map(|(bot, cursor)| {
                    let cursor = match requested.get(bot) {
                        Some(asked) if asked == cursor => {
                            wanted.get(bot).cloned().unwrap_or_else(|| cursor.clone())
                        }
                        _ => cursor.clone(),
                    };
                    (*bot, cursor)
                })
                .collect();
            requested = latest;
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
                    changed = desired.changed() => {
                        if changed.is_err() { return; }
                        let next = desired.borrow_and_update().clone();
                        let mut messages = Vec::new();
                        wanted.retain(|bot, _| {
                            let keep = next.contains_key(bot);
                            if !keep {
                                messages.push(Message::Text(
                                    json!({ "op": "unsubscribe", "channel": format!("bot:{bot}") })
                                        .to_string()
                                        .into(),
                                ));
                            }
                            keep
                        });
                        for (bot, cursor) in &next {
                            if requested.get(bot) != Some(cursor) || !wanted.contains_key(bot) {
                                messages.push(subscribe(format!("bot:{bot}"), Some(cursor)));
                                wanted.insert(*bot, cursor.clone());
                            }
                        }
                        requested = next;
                        for message in messages {
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
            if connected_at.elapsed() >= STABLE {
                backoff = Duration::from_millis(500);
            }
            pause!();
        }
    }
}
