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
//! browser `Origin`). Events are read from the same PostgreSQL journal as
//! snapshots, woken by `pg_notify` with a one-second polling fallback. A
//! deleted bot's channel ends with a `bot_deleted` error and nothing already
//! queued for it is sent. The server pings every 15 seconds and closes a
//! connection that is silent for 60 seconds or cannot accept a write within
//! 30 seconds; outgoing delivery is bounded.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

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

/// One message queued for the client. Channel messages carry the
/// subscription generation that produced them, so the writer drops anything
/// from a subscription that was since replaced, unsubscribed, or revoked.
struct Outgoing {
    tag: Option<Tag>,
    body: Value,
}

struct Tag {
    channel: String,
    generation: u64,
    bot: Option<Uuid>,
    /// The subscription's final message (an error). Delivering it retires
    /// the subscription.
    last: bool,
}

/// Subscription state shared by the reader, the channel workers, and the
/// writer of one connection.
#[derive(Default)]
struct Registry {
    /// The current subscription of each channel.
    channels: HashMap<String, Entry>,
    /// Bots seen deleted on this connection. Their data is never written
    /// again, even after a new subscription.
    deleted: HashSet<Uuid>,
    next_generation: u64,
}

struct Entry {
    generation: u64,
    /// Whether its worker still runs; only running ones count against the
    /// subscription limit.
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

    /// Whether the writer may send this message; retires the subscription
    /// when its final message goes out.
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

type Shared = Arc<Mutex<Registry>>;

fn lock(registry: &Shared) -> MutexGuard<'_, Registry> {
    registry.lock().unwrap_or_else(|p| p.into_inner())
}

/// A channel worker's handle on its own subscription.
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

    /// Queue a message. False once the subscription or connection ended;
    /// a worker never stays blocked on a full queue after it is retired.
    async fn send(&self, body: Value) -> bool {
        let message = self.message(body, false);
        tokio::select! {
            _ = self.token.cancelled() => false,
            sent = self.out.send(message) => sent.is_ok(),
        }
    }

    /// End the subscription with an error, its final message.
    async fn fail(&self, code: &str, message: &str) {
        if let Some(entry) = lock(&self.registry).channels.get_mut(&self.channel)
            && entry.generation == self.generation
        {
            entry.running = false;
        }
        let body =
            json!({ "channel": self.channel, "error": { "code": code, "message": message } });
        let message = self.message(body, true);
        tokio::select! {
            _ = self.token.cancelled() => {}
            _ = self.out.send(message) => {}
        }
    }

    /// End a bot's subscription because the bot was deleted. Data already
    /// queued for it is dropped by the writer from now on.
    async fn deleted(&self, bot: Uuid) {
        lock(&self.registry).deleted.insert(bot);
        self.fail("bot_deleted", "This bot has been deleted.").await;
    }
}

async fn connection(state: Arc<AppState>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(OUTGOING);
    let registry: Shared = Arc::default();
    let closed = CancellationToken::new();

    // Writer: the only task touching the sink.
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
                        && !lock(&writer_registry).admit(tag)
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
        // Closing flushes; a peer that stopped reading must not hold it open.
        let _ = tokio::time::timeout(CLOSE_LIMIT, sink.close()).await;
    });

    // Tokens of subscriptions started by this reader, by channel.
    let mut tokens: HashMap<String, CancellationToken> = HashMap::new();
    loop {
        let next = tokio::select! {
            _ = closed.cancelled() => break,
            next = tokio::time::timeout(IDLE_LIMIT, stream.next()) => next,
        };
        let message = match next {
            Ok(Some(Ok(message))) => message,
            // Silent too long, failed, or closed.
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
                lock(&registry).channels.remove(&channel);
                if let Some(token) = tokens.remove(&channel) {
                    token.cancel();
                }
            }
            Ok(ClientMessage::Subscribe { channel, cursor }) => {
                if let Some(old) = tokens.remove(&channel) {
                    old.cancel();
                }
                let bot = channel.strip_prefix("bot:").map(|id| id.parse::<Uuid>());
                let refusal = {
                    let mut registry = lock(&registry);
                    registry.channels.remove(&channel);
                    tokens.retain(|c, _| registry.channels.contains_key(c));
                    if registry.running() >= MAX_SUBSCRIPTIONS {
                        Some(("too_many_subscriptions", "Unsubscribe first."))
                    } else {
                        match &bot {
                            Some(Err(_)) => Some(("not_found", "Unknown bot.")),
                            None if channel != "bots" => {
                                Some(("unknown_channel", "Use bots or bot:<id>."))
                            }
                            _ => None,
                        }
                    }
                };
                let token = closed.child_token();
                let generation = lock(&registry).open(&channel, refusal.is_none());
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
    /// The subscription ended while reading.
    Cancelled,
    /// The read failed or took too long.
    Failed,
}

/// Run a database read unless the subscription ends or it takes too long.
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

/// Wait for a wake hint (for `bot`, or any bot when `None`) or the fallback.
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

/// `bots`: the full list (with deletion receipts still pending) and the
/// default pointer, sent on subscribe and whenever it changes. A failed read
/// is retried.
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

/// Whether the bot is still active. Otherwise the subscription has been
/// ended with the matching error (or was cancelled).
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

/// `bot:<id>`: committed journal events after the cursor, in order. Any
/// failure ends the subscription with an error, so the client resubscribes
/// or reloads instead of waiting on a silent channel.
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
        // Checked after the read: a deletion committed meanwhile is seen
        // before any of these rows are sent.
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
            // A slow reader must not delay revocation: while waiting for
            // queue space, recheck on this bot's wake hints and on the
            // fallback timer.
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
        // A queued error or event of the replaced subscription is dropped.
        assert!(!registry.admit(&tag("bot:x", old, None, true)));
        assert!(!registry.admit(&tag("bot:x", old, None, false)));
        assert!(registry.admit(&tag("bot:x", new, None, false)));
        // Unsubscribed: nothing more.
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
        // Data queued before the deletion is dropped; the final error goes out.
        assert!(!registry.admit(&tag(&channel, first, Some(bot), false)));
        let second = registry.open(&channel, true);
        assert!(!registry.admit(&tag(&channel, second, Some(bot), false)));
        assert!(registry.admit(&tag(&channel, second, Some(bot), true)));
        // Other bots are unaffected.
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
        // Delivering the final message retires the subscription.
        assert!(registry.admit(&tag("bot:a", g, None, true)));
        assert!(!registry.channels.contains_key("bot:a"));
    }
}
