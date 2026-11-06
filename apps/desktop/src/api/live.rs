use super::{BotList, Client};
use crate::model::Frame;
use futures_util::{SinkExt as _, StreamExt as _};
use reqwest::StatusCode;
use reqwest::header::AUTHORIZATION;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

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
pub struct LiveCommands(pub(super) tokio::sync::watch::Sender<HashMap<Uuid, String>>);

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

pub(super) fn subscriptions(
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

pub(super) fn route(
    message: &serde_json::Value,
    wanted: &mut HashMap<Uuid, String>,
) -> Option<LiveEvent> {
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

impl Client {
    /// Open the live connection: one WebSocket carrying the `bots` channel
    /// and any `bot:<id>` channels the UI subscribes to. It reconnects with
    /// backoff and resubscribes every channel from its last delivered cursor.
    pub fn live(self: Arc<Self>) -> Live {
        let (commands, command_rx) = tokio::sync::watch::channel(HashMap::new());
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
        mut commands: tokio::sync::watch::Receiver<HashMap<Uuid, String>>,
        events: tokio::sync::mpsc::Sender<LiveEvent>,
    ) {
        use tokio_tungstenite::tungstenite::{Error as WsError, Message};
        const INACTIVITY: Duration = Duration::from_secs(45);
        const HANDSHAKE: Duration = Duration::from_secs(10);
        const STABLE: Duration = Duration::from_secs(30);
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
            let verified = tokio::select! {
                _ = events.closed() => return,
                result = self.verify_transport() => result,
            };
            if let Err(err) = verified {
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
            if connected_at.elapsed() >= STABLE {
                backoff = Duration::from_millis(500);
            }
            pause!();
        }
    }
}
