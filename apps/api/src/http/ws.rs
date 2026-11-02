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
//! server → {"channel":"bots","event":"bots.snapshot","data":{"default_bot_id":…,"bots":[…]}}
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
use std::sync::{Arc, Mutex};
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

/// One message queued for the client, tagged with its channel so messages
/// of a revoked channel can be dropped before they are written.
struct Outgoing {
    channel: Option<String>,
    body: Value,
}

/// Channels whose data must no longer be delivered (deleted bots).
type Revoked = Arc<Mutex<HashSet<String>>>;

async fn connection(state: Arc<AppState>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Outgoing>(OUTGOING);
    let revoked: Revoked = Arc::default();
    let closed = CancellationToken::new();

    // Writer: the only task touching the sink.
    let writer_revoked = revoked.clone();
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
                    let is_error = next.body.get("error").is_some();
                    let dropped = next.channel.as_ref().is_some_and(|c| {
                        writer_revoked.lock().unwrap_or_else(|p| p.into_inner()).contains(c)
                    });
                    // A revoked channel's final error is still delivered.
                    if dropped && !is_error {
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
        let _ = sink.close().await;
    });

    let mut subscriptions: HashMap<String, CancellationToken> = HashMap::new();
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
                send(
                    &out_tx,
                    None,
                    json!({ "error": {
                        "code": "invalid_message",
                        "message": "Expected {\"op\":\"subscribe\"|\"unsubscribe\",\"channel\":…}.",
                    }}),
                )
                .await;
            }
            Ok(ClientMessage::Unsubscribe { channel }) => {
                if let Some(token) = subscriptions.remove(&channel) {
                    token.cancel();
                }
            }
            Ok(ClientMessage::Subscribe { channel, cursor }) => {
                if let Some(old) = subscriptions.remove(&channel) {
                    old.cancel();
                }
                if subscriptions.len() >= MAX_SUBSCRIPTIONS {
                    channel_error(
                        &out_tx,
                        &channel,
                        "too_many_subscriptions",
                        "Unsubscribe first.",
                    )
                    .await;
                    continue;
                }
                let token = closed.child_token();
                if channel == "bots" {
                    tokio::spawn(bots_channel(state.clone(), out_tx.clone(), token.clone()));
                } else if let Some(bot) = channel.strip_prefix("bot:") {
                    let Ok(bot_id) = bot.parse::<Uuid>() else {
                        channel_error(&out_tx, &channel, "not_found", "Unknown bot.").await;
                        continue;
                    };
                    revoked
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .remove(&channel);
                    tokio::spawn(bot_channel(
                        state.clone(),
                        bot_id,
                        channel.clone(),
                        cursor,
                        out_tx.clone(),
                        revoked.clone(),
                        token.clone(),
                    ));
                } else {
                    channel_error(
                        &out_tx,
                        &channel,
                        "unknown_channel",
                        "Use bots or bot:<id>.",
                    )
                    .await;
                    continue;
                }
                subscriptions.insert(channel, token);
            }
        }
    }
    closed.cancel();
    for token in subscriptions.into_values() {
        token.cancel();
    }
    let _ = writer.await;
}

async fn send(out: &mpsc::Sender<Outgoing>, channel: Option<&str>, body: Value) -> bool {
    out.send(Outgoing {
        channel: channel.map(str::to_owned),
        body,
    })
    .await
    .is_ok()
}

async fn channel_error(out: &mpsc::Sender<Outgoing>, channel: &str, code: &str, message: &str) {
    send(
        out,
        Some(channel),
        json!({ "channel": channel, "error": { "code": code, "message": message } }),
    )
    .await;
}

/// Run a database read unless the channel ends or it takes too long.
async fn guarded<T>(
    token: &CancellationToken,
    fut: impl std::future::Future<Output = Result<T, sqlx::Error>>,
) -> Option<T> {
    tokio::select! {
        _ = token.cancelled() => None,
        result = tokio::time::timeout(QUERY_TIMEOUT, fut) => match result {
            Ok(Ok(value)) => Some(value),
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "websocket database read failed");
                None
            }
            Err(_) => None,
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
/// default pointer, sent on subscribe and whenever it changes.
async fn bots_channel(state: Arc<AppState>, out: mpsc::Sender<Outgoing>, token: CancellationToken) {
    let mut wake = state.events.subscribe();
    let mut last: Option<Value> = None;
    loop {
        let Some(listing) = guarded(&token, bots::list(&state.db, true)).await else {
            if token.is_cancelled() {
                return;
            }
            if !wait(&mut wake, None, &token).await {
                return;
            }
            continue;
        };
        let data = json!({
            "default_bot_id": listing.default_bot_id,
            "bots": listing.bots.iter().map(bot_json).collect::<Vec<_>>(),
        });
        if last.as_ref() != Some(&data) {
            let body = json!({ "channel": "bots", "event": "bots.snapshot", "data": data });
            if !send(&out, Some("bots"), body).await {
                return;
            }
            last = Some(data);
        }
        if !wait(&mut wake, None, &token).await {
            return;
        }
    }
}

/// `bot:<id>`: committed journal events after the cursor, in order.
async fn bot_channel(
    state: Arc<AppState>,
    bot_id: Uuid,
    channel: String,
    cursor: Option<String>,
    out: mpsc::Sender<Outgoing>,
    revoked: Revoked,
    token: CancellationToken,
) {
    let mut wake = state.events.subscribe();
    let Some(cursor) = cursor else {
        return channel_error(
            &out,
            &channel,
            "cursor_required",
            "Subscribe with a snapshot's event_cursor.",
        )
        .await;
    };
    let Some((instance, cursor_bot, mut after)) = parse_cursor(&cursor) else {
        return channel_error(
            &out,
            &channel,
            "invalid_cursor",
            "The event cursor is malformed.",
        )
        .await;
    };
    if cursor_bot != bot_id {
        return channel_error(
            &out,
            &channel,
            "invalid_cursor",
            "The cursor belongs to another bot.",
        )
        .await;
    }
    match guarded(&token, bot_is_active(&state, bot_id)).await {
        None if token.is_cancelled() => return,
        None => return channel_error(&out, &channel, "unavailable", "Try again.").await,
        Some(None) => return channel_error(&out, &channel, "not_found", "Unknown bot.").await,
        Some(Some((false, _))) => {
            return channel_error(&out, &channel, "bot_deleted", "This bot has been deleted.")
                .await;
        }
        Some(Some((true, last))) => {
            if instance != state.server_instance_id || after > last {
                return channel_error(
                    &out,
                    &channel,
                    "cursor_reset_required",
                    "The event cursor is no longer valid. Load a fresh snapshot.",
                )
                .await;
            }
        }
    }
    let revoke = |revoked: &Revoked| {
        revoked
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(channel.clone());
    };
    loop {
        let Some(rows) = guarded(&token, events::after(&state.db, bot_id, after, BATCH)).await
        else {
            return;
        };
        // Checked after the read: a deletion committed meanwhile is seen
        // before any of these rows are sent.
        match guarded(&token, bot_is_active(&state, bot_id)).await {
            Some(Some((true, _))) => {}
            Some(_) => {
                revoke(&revoked);
                return channel_error(&out, &channel, "bot_deleted", "This bot has been deleted.")
                    .await;
            }
            None => return,
        }
        let full = rows.len() as i64 == BATCH;
        for (seq, kind, data) in rows {
            let body = json!({
                "channel": channel,
                "id": encode_cursor(state.server_instance_id, bot_id, seq),
                "event": kind,
                "data": data,
            });
            // A slow reader must not delay revocation: watch this bot's
            // wake hints while waiting for queue space.
            let mut sending = std::pin::pin!(send(&out, Some(&channel), body));
            loop {
                tokio::select! {
                    _ = token.cancelled() => return,
                    sent = &mut sending => {
                        if !sent {
                            return;
                        }
                        break;
                    }
                    received = wake.recv() => {
                        let ours = match received {
                            Ok(id) => id == bot_id || id.is_nil(),
                            Err(broadcast::error::RecvError::Lagged(_)) => true,
                            Err(broadcast::error::RecvError::Closed) => false,
                        };
                        if ours && !matches!(guarded(&token, bot_is_active(&state, bot_id)).await, Some(Some((true, _)))) {
                            revoke(&revoked);
                            return channel_error(&out, &channel, "bot_deleted", "This bot has been deleted.").await;
                        }
                    }
                }
            }
            after = seq;
        }
        if full {
            continue;
        }
        if !wait(&mut wake, Some(bot_id), &token).await {
            return;
        }
    }
}
