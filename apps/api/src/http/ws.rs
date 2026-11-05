//! The owner WebSocket: one authenticated connection carrying pub/sub
//! channels for live updates.
//!
//! Commands stay on HTTP (they are idempotent under request IDs); this socket
//! only delivers committed changes. A client subscribes to:
//!
//! - `bots`: the bot list and default pointer, pushed whenever they change
//!   (replaces sidebar polling);
//! - `bot:<id>` with a cursor from a conversation snapshot: that bot's event
//!   journal after the cursor, in order (replaces the per-bot SSE stream).
//!
//! ```text
//! client → {"op":"subscribe","channel":"bots"}
//! client → {"op":"subscribe","channel":"bot:<id>","cursor":"<cursor>"}
//! client → {"op":"unsubscribe","channel":"bot:<id>"}
//! server → {"channel":"bots","event":"bots.snapshot","data":{"default_bot_id":…,"categories":[…],"bots":[…]}}
//! server → {"channel":"bot:<id>","id":"<cursor>","event":"item.delta","data":{…}}
//! server → {"channel":"bot:<id>","error":{"code":"cursor_reset_required","message":"…"}}
//! ```
//!
//! The upgrade is authenticated like every `/v1` route (bearer header, no
//! browser `Origin`). Events are read from the same SQLite journal as
//! snapshots, woken by committed database-version changes with a one-second polling fallback. A
//! deleted bot's channel ends with a `bot_deleted` error and nothing already
//! queued for it is sent. The server pings every 15 seconds and closes a
//! connection that is silent for 60 seconds or cannot accept a write within
//! 30 seconds; outgoing delivery is bounded.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::actor::Actor;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::AppState;
use super::conversation::{bot_is_active, encode_cursor, parse_cursor};
use crate::db::{bots, events};
use crate::views::bot_json;

const MAX_MESSAGE: usize = 64 * 1024;
const MAX_SUBSCRIPTIONS: usize = 32;
const OUTGOING: usize = 256;
const BATCH: i64 = 200;
const POLL_FALLBACK: Duration = Duration::from_secs(1);
const PING_EVERY: Duration = Duration::from_secs(15);
const IDLE_LIMIT: Duration = Duration::from_secs(60);
const WRITE_LIMIT: Duration = Duration::from_secs(30);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
const CLOSE_LIMIT: Duration = Duration::from_secs(5);

pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/ws", get(upgrade))
}

async fn upgrade(State(state): State<Arc<AppState>>, ws: WebSocketUpgrade) -> Response {
    ws.max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| connection(state, socket))
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum ClientMessage {
    Subscribe {
        channel: String,
        cursor: Option<String>,
    },
    Unsubscribe {
        channel: String,
    },
}

struct Outgoing {
    tag: Option<Tag>,
    body: Value,
}

#[derive(Clone)]
struct Tag {
    channel: String,
    generation: u64,
    bot: Option<Uuid>,
    last: bool,
}

#[derive(Default)]
struct Registry {
    channels: HashMap<String, Entry>,
    deleted: HashSet<Uuid>,
    next_generation: u64,
}

struct Entry {
    generation: u64,
    running: bool,
}

impl Registry {
    fn open(&mut self, channel: &str, running: bool) -> u64 {
        self.next_generation += 1;
        let generation = self.next_generation;
        self.channels.insert(
            channel.to_owned(),
            Entry {
                generation,
                running,
            },
        );
        generation
    }

    fn running(&self) -> usize {
        self.channels.values().filter(|e| e.running).count()
    }

    fn admit(&mut self, tag: &Tag) -> bool {
        let current = self
            .channels
            .get(&tag.channel)
            .is_some_and(|e| e.generation == tag.generation);
        if !current {
            return false;
        }
        if tag.last {
            self.channels.remove(&tag.channel);
            return true;
        }
        !tag.bot.is_some_and(|bot| self.deleted.contains(&bot))
    }
}

type Shared = Actor<Registry>;

struct Sub {
    channel: String,
    generation: u64,
    bot: Option<Uuid>,
    out: mpsc::Sender<Outgoing>,
    registry: Shared,
    token: CancellationToken,
}

impl Sub {
    fn message(&self, body: Value, last: bool) -> Outgoing {
        Outgoing {
            tag: Some(Tag {
                channel: self.channel.clone(),
                generation: self.generation,
                bot: self.bot,
                last,
            }),
            body,
        }
    }

    async fn send(&self, body: Value) -> bool {
        let message = self.message(body, false);
        tokio::select! {
            _ = self.token.cancelled() => false,
            sent = self.out.send(message) => sent.is_ok(),
        }
    }

    async fn fail(&self, code: &str, message: &str) {
        let channel = self.channel.clone();
        let generation = self.generation;
        self.registry
            .call(move |registry| {
                if let Some(entry) = registry.channels.get_mut(&channel)
                    && entry.generation == generation
                {
                    entry.running = false;
                }
            })
            .await;
        let body =
            json!({ "channel": self.channel, "error": { "code": code, "message": message } });
        let message = self.message(body, true);
        tokio::select! {
            _ = self.token.cancelled() => {}
            _ = self.out.send(message) => {}
        }
    }

    async fn deleted(&self, bot: Uuid) {
        self.registry
            .call(move |registry| registry.deleted.insert(bot))
            .await;
        self.fail("bot_deleted", "This bot has been deleted.").await;
    }
}

async fn connection(state: Arc<AppState>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(OUTGOING);
    let registry: Shared = Actor::default();
    let closed = CancellationToken::new();

    let writer_registry = registry.clone();
    let writer_closed = closed.clone();
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_EVERY);
        ping.tick().await;
        loop {
            let message = tokio::select! {
                _ = writer_closed.cancelled() => break,
                _ = ping.tick() => Message::Ping(Vec::new().into()),
                next = out_rx.recv() => {
                    let Some(next) = next else { break };
                    if let Some(tag) = &next.tag
                        && !writer_registry.call({ let tag = tag.clone(); move |registry| registry.admit(&tag) }).await
                    {
                        continue;
                    }
                    Message::Text(next.body.to_string().into())
                }
            };
            match tokio::time::timeout(WRITE_LIMIT, sink.send(message)).await {
                Ok(Ok(())) => {}
                _ => break,
            }
        }
        writer_closed.cancel();
        let _ = tokio::time::timeout(CLOSE_LIMIT, sink.close()).await;
    });

    let mut tokens: HashMap<String, CancellationToken> = HashMap::new();
    loop {
        let next = tokio::select! {
            _ = closed.cancelled() => break,
            next = tokio::time::timeout(IDLE_LIMIT, stream.next()) => next,
        };
        let message = match next {
            Ok(Some(Ok(message))) => message,
            _ => break,
        };
        let text = match message {
            Message::Text(text) => text,
            Message::Close(_) => break,
            // Pings are answered automatically; pongs only prove liveness.
            _ => continue,
        };
        let parsed: Result<ClientMessage, _> = serde_json::from_str(&text);
        match parsed {
            Err(_) => {
                let body = json!({ "error": {
                    "code": "invalid_message",
                    "message": "Expected {\"op\":\"subscribe\"|\"unsubscribe\",\"channel\":…}.",
                }});
                if out_tx.send(Outgoing { tag: None, body }).await.is_err() {
                    break;
                }
            }
            Ok(ClientMessage::Unsubscribe { channel }) => {
                registry
                    .call({
                        let channel = channel.clone();
                        move |registry| registry.channels.remove(&channel)
                    })
                    .await;
                if let Some(token) = tokens.remove(&channel) {
                    token.cancel();
                }
            }
            Ok(ClientMessage::Subscribe { channel, cursor }) => {
                if let Some(old) = tokens.remove(&channel) {
                    old.cancel();
                }
                let bot = channel.strip_prefix("bot:").map(|id| id.parse::<Uuid>());
                let channel_for_check = channel.clone();
                let bot_for_check = bot.clone();
                let (refusal, channels) = registry
                    .call(move |registry| {
                        let channel = channel_for_check;
                        let bot = bot_for_check;
                        registry.channels.remove(&channel);
                        let channels: HashSet<String> = registry.channels.keys().cloned().collect();
                        let refusal = if registry.running() >= MAX_SUBSCRIPTIONS {
                            Some(("too_many_subscriptions", "Unsubscribe first."))
                        } else {
                            match &bot {
                                Some(Err(_)) => Some(("not_found", "Unknown bot.")),
                                None if channel != "bots" => {
                                    Some(("unknown_channel", "Use bots or bot:<id>."))
                                }
                                _ => None,
                            }
                        };
                        (refusal, channels)
                    })
                    .await;
                tokens.retain(|c, _| channels.contains(c));
                let token = closed.child_token();
                let generation = registry
                    .call({
                        let channel = channel.clone();
                        move |registry| registry.open(&channel, refusal.is_none())
                    })
                    .await;
                let sub = Sub {
                    channel: channel.clone(),
                    generation,
                    bot: bot.and_then(Result::ok),
                    out: out_tx.clone(),
                    registry: registry.clone(),
                    token: token.clone(),
                };
                if let Some((code, message)) = refusal {
                    sub.fail(code, message).await;
                    continue;
                }
                match sub.bot {
                    Some(bot_id) => {
                        tokio::spawn(bot_channel(state.clone(), bot_id, cursor, sub));
                    }
                    None => {
                        tokio::spawn(bots_channel(state.clone(), sub));
                    }
                }
                tokens.insert(channel, token);
            }
        }
    }
    closed.cancel();
    for token in tokens.into_values() {
        token.cancel();
    }
    let _ = writer.await;
}

enum Read<T> {
    Done(T),
    Cancelled,
    Failed,
}

async fn guarded<T>(
    token: &CancellationToken,
    fut: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Read<T> {
    tokio::select! {
        _ = token.cancelled() => Read::Cancelled,
        result = tokio::time::timeout(QUERY_TIMEOUT, fut) => match result {
            Ok(Ok(value)) => Read::Done(value),
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "websocket database read failed");
                Read::Failed
            }
            Err(_) => {
                tracing::warn!("websocket database read timed out");
                Read::Failed
            }
        },
    }
}

async fn wait(
    wake: &mut broadcast::Receiver<Uuid>,
    bot: Option<Uuid>,
    token: &CancellationToken,
) -> bool {
    let deadline = tokio::time::sleep(POLL_FALLBACK);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = token.cancelled() => return false,
            _ = &mut deadline => return true,
            received = wake.recv() => match received {
                Ok(id) if bot.is_none() || Some(id) == bot || id.is_nil() => return true,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(_)) => return true,
                Err(broadcast::error::RecvError::Closed) => {
                    (&mut deadline).await;
                    return true;
                }
            },
        }
    }
}

async fn bots_channel(state: Arc<AppState>, sub: Sub) {
    let mut wake = state.events.subscribe();
    let mut last: Option<Value> = None;
    loop {
        match guarded(&sub.token, bots::list(&state.db, true)).await {
            Read::Cancelled => return,
            Read::Failed => {}
            Read::Done(listing) => {
                let data = json!({
                    "default_bot_id": listing.default_bot_id,
                    "categories": listing.categories,
                    "bots": listing.bots.iter().map(bot_json).collect::<Vec<_>>(),
                });
                if last.as_ref() != Some(&data) {
                    let body = json!({ "channel": "bots", "event": "bots.snapshot", "data": data });
                    if !sub.send(body).await {
                        return;
                    }
                    last = Some(data);
                }
            }
        }
        if !wait(&mut wake, None, &sub.token).await {
            return;
        }
    }
}

async fn still_active(state: &AppState, bot_id: Uuid, sub: &Sub) -> bool {
    match guarded(&sub.token, bot_is_active(state, bot_id)).await {
        Read::Done(Some((true, _))) => true,
        Read::Done(Some((false, _))) => {
            sub.deleted(bot_id).await;
            false
        }
        Read::Done(None) => {
            sub.fail("not_found", "Unknown bot.").await;
            false
        }
        Read::Cancelled => false,
        Read::Failed => {
            sub.fail("unavailable", "Resubscribe from the last cursor.")
                .await;
            false
        }
    }
}

async fn bot_channel(state: Arc<AppState>, bot_id: Uuid, cursor: Option<String>, sub: Sub) {
    let mut wake = state.events.subscribe();
    let Some(cursor) = cursor else {
        return sub
            .fail(
                "cursor_required",
                "Subscribe with a snapshot's event_cursor.",
            )
            .await;
    };
    let Some((instance, cursor_bot, mut after)) = parse_cursor(&cursor) else {
        return sub
            .fail("invalid_cursor", "The event cursor is malformed.")
            .await;
    };
    if cursor_bot != bot_id {
        return sub
            .fail("invalid_cursor", "The cursor belongs to another bot.")
            .await;
    }
    match guarded(&sub.token, bot_is_active(&state, bot_id)).await {
        Read::Cancelled => return,
        Read::Failed => return sub.fail("unavailable", "Try again.").await,
        Read::Done(None) => return sub.fail("not_found", "Unknown bot.").await,
        Read::Done(Some((false, _))) => return sub.deleted(bot_id).await,
        Read::Done(Some((true, last))) => {
            if instance != state.server_instance_id || after > last {
                return sub
                    .fail(
                        "cursor_reset_required",
                        "The event cursor is no longer valid. Load a fresh snapshot.",
                    )
                    .await;
            }
        }
    }
    loop {
        let rows = match guarded(&sub.token, events::after(&state.db, bot_id, after, BATCH)).await {
            Read::Done(rows) => rows,
            Read::Cancelled => return,
            Read::Failed => {
                return sub
                    .fail("unavailable", "Resubscribe from the last cursor.")
                    .await;
            }
        };
        if !still_active(&state, bot_id, &sub).await {
            return;
        }
        let full = rows.len() as i64 == BATCH;
        for (seq, kind, data) in rows {
            let body = json!({
                "channel": sub.channel,
                "id": encode_cursor(state.server_instance_id, bot_id, seq),
                "event": kind,
                "data": data,
            });
            let mut sending = std::pin::pin!(sub.send(body));
            let mut fallback = tokio::time::interval_at(
                tokio::time::Instant::now() + POLL_FALLBACK,
                POLL_FALLBACK,
            );
            loop {
                let check = tokio::select! {
                    sent = &mut sending => {
                        if !sent {
                            return;
                        }
                        break;
                    }
                    _ = fallback.tick() => true,
                    received = wake.recv() => match received {
                        Ok(id) => id == bot_id || id.is_nil(),
                        Err(broadcast::error::RecvError::Lagged(_)) => true,
                        Err(broadcast::error::RecvError::Closed) => false,
                    },
                };
                if check && !still_active(&state, bot_id, &sub).await {
                    return;
                }
            }
            after = seq;
        }
        if full {
            continue;
        }
        if !wait(&mut wake, Some(bot_id), &sub.token).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(channel: &str, generation: u64, bot: Option<Uuid>, last: bool) -> Tag {
        Tag {
            channel: channel.to_owned(),
            generation,
            bot,
            last,
        }
    }

    #[test]
    fn retired_generations_are_never_written() {
        let mut registry = Registry::default();
        let old = registry.open("bot:x", true);
        let new = registry.open("bot:x", true);
        assert!(!registry.admit(&tag("bot:x", old, None, true)));
        assert!(!registry.admit(&tag("bot:x", old, None, false)));
        assert!(registry.admit(&tag("bot:x", new, None, false)));
        registry.channels.remove("bot:x");
        assert!(!registry.admit(&tag("bot:x", new, None, false)));
    }

    #[test]
    fn deleted_bots_stay_revoked_across_resubscription() {
        let bot = Uuid::new_v4();
        let channel = format!("bot:{bot}");
        let mut registry = Registry::default();
        let first = registry.open(&channel, true);
        registry.deleted.insert(bot);
        assert!(!registry.admit(&tag(&channel, first, Some(bot), false)));
        let second = registry.open(&channel, true);
        assert!(!registry.admit(&tag(&channel, second, Some(bot), false)));
        assert!(registry.admit(&tag(&channel, second, Some(bot), true)));
        let other = Uuid::new_v4();
        let g = registry.open(&format!("bot:{other}"), true);
        assert!(registry.admit(&tag(&format!("bot:{other}"), g, Some(other), false)));
    }

    #[test]
    fn ended_subscriptions_free_their_slot() {
        let mut registry = Registry::default();
        let g = registry.open("bot:a", true);
        registry.open("bot:b", false);
        assert_eq!(registry.running(), 1);
        assert!(registry.admit(&tag("bot:a", g, None, true)));
        assert!(!registry.channels.contains_key("bot:a"));
    }
}
