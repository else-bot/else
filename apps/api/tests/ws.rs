//! The WebSocket channel API over a real TCP listener against PostgreSQL:
//! snapshots plus `bot:<id>` replay, the `bots` channel, reconnects, cursor
//! errors, deletion, and authentication.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{TOKEN, TestApp};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Server {
    app: Arc<TestApp>,
    addr: String,
}

impl Server {
    async fn start() -> Self {
        let app = Arc::new(TestApp::installed().await);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let router = app.router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { app, addr }
    }

    async fn snapshot(&self, bot: &str) -> Value {
        let reply = self
            .app
            .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
            .await;
        assert_eq!(reply.status, axum::http::StatusCode::OK, "{:?}", reply.body);
        reply.body
    }

    /// Open a socket; `Err` carries the HTTP status of a refused upgrade.
    async fn connect(&self, token: Option<&str>, origin: Option<&str>) -> Result<Socket, u16> {
        let mut request = format!("ws://{}/v1/ws", self.addr)
            .into_client_request()
            .unwrap();
        if let Some(token) = token {
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _)) => Ok(socket),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                Err(response.status().as_u16())
            }
            Err(err) => panic!("connect failed: {err}"),
        }
    }
}

async fn op(socket: &mut Socket, body: Value) {
    socket
        .send(Message::Text(body.to_string().into()))
        .await
        .unwrap();
}

async fn subscribe_bot(socket: &mut Socket, bot: &str, cursor: &str) {
    op(
        socket,
        json!({ "op": "subscribe", "channel": format!("bot:{bot}"), "cursor": cursor }),
    )
    .await;
}

/// The next JSON message, or `None` on close or after `wait` of silence.
async fn next(socket: &mut Socket, wait: Duration) -> Option<Value> {
    loop {
        match tokio::time::timeout(wait, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => return Some(serde_json::from_str(&text).unwrap()),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) | Err(_) => return None,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// The client model: applies only contiguous events per bot and checks
/// UTF-8 offsets, like the desktop reducer.
struct Model {
    prefix: String,
    applied: i64,
    items: HashMap<String, Value>,
    duplicates: usize,
}

impl Model {
    fn from_snapshot(snapshot: &Value) -> Self {
        let (prefix, seq) = snapshot["event_cursor"]
            .as_str()
            .unwrap()
            .rsplit_once(':')
            .unwrap();
        let items = snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| (i["id"].as_str().unwrap().to_owned(), i.clone()))
            .collect();
        Self {
            prefix: prefix.into(),
            applied: seq.parse().unwrap(),
            items,
            duplicates: 0,
        }
    }

    fn cursor(&self) -> String {
        format!("{}:{}", self.prefix, self.applied)
    }

    fn apply(&mut self, message: &Value) {
        assert!(message.get("error").is_none(), "unexpected error {message}");
        let (prefix, seq) = message["id"].as_str().unwrap().rsplit_once(':').unwrap();
        assert_eq!(prefix, self.prefix);
        let seq: i64 = seq.parse().unwrap();
        if seq <= self.applied {
            self.duplicates += 1;
            return;
        }
        assert_eq!(seq, self.applied + 1, "contiguous");
        let data = &message["data"];
        match message["event"].as_str().unwrap() {
            "item.upsert" => {
                self.items.insert(
                    data["item"]["id"].as_str().unwrap().into(),
                    data["item"].clone(),
                );
            }
            "item.delta" => {
                let item = self
                    .items
                    .get_mut(data["item_id"].as_str().unwrap())
                    .unwrap();
                let text = item["text"].as_str().unwrap().to_owned();
                assert_eq!(data["offset_utf8"].as_u64().unwrap() as usize, text.len());
                item["text"] = json!(text + data["text"].as_str().unwrap());
            }
            "turn.updated" | "bot.updated" => {}
            other => panic!("unknown event {other}"),
        }
        self.applied = seq;
    }

    fn texts(&self) -> Vec<(i64, String)> {
        let mut texts: Vec<_> = self
            .items
            .values()
            .map(|i| {
                (
                    i["seq"].as_i64().unwrap(),
                    i["text"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        texts.sort();
        texts
    }
}

fn snapshot_texts(snapshot: &Value) -> Vec<(i64, String)> {
    snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["seq"].as_i64().unwrap(),
                i["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn latest_seq(snapshot: &Value) -> i64 {
    snapshot["event_cursor"]
        .as_str()
        .unwrap()
        .rsplit_once(':')
        .unwrap()
        .1
        .parse()
        .unwrap()
}

/// Apply `bot:<id>` messages until `done`.
async fn read_until(socket: &mut Socket, model: &mut Model, done: impl Fn(&Model) -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !done(model) {
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        if let Some(message) = next(socket, Duration::from_secs(10)).await {
            if message["channel"]
                .as_str()
                .is_some_and(|c| c.starts_with("bot:"))
            {
                model.apply(&message);
            }
        } else {
            panic!("socket closed");
        }
    }
}

async fn wait_completed(server: &Server, reply: &common::Reply) {
    server
        .app
        .wait_turn(reply.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_then_subscribe_replays_everything_in_between() {
    let server = Server::start().await;
    let bot = server.app.main_bot().await;
    let snapshot = server.snapshot(&bot).await;
    let mut model = Model::from_snapshot(&snapshot);
    // A full exchange commits after the snapshot, before subscribing.
    let sent = server
        .app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:slow] héllo ✓ wörld",
            Some("synthetic"),
        )
        .await;
    wait_completed(&server, &sent).await;
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    subscribe_bot(&mut socket, &bot, &model.cursor()).await;
    let latest = server.snapshot(&bot).await;
    let target = latest_seq(&latest);
    read_until(&mut socket, &mut model, |m| m.applied >= target).await;
    assert_eq!(model.texts(), snapshot_texts(&latest));
    assert!(model.texts()[1].1.contains("héllo ✓ wörld"));
}

#[tokio::test(flavor = "multi_thread")]
async fn live_events_reconnect_and_replay_without_duplicates() {
    let server = Server::start().await;
    let bot = server.app.main_bot().await;
    let snapshot = server.snapshot(&bot).await;
    let mut model = Model::from_snapshot(&snapshot);
    let start = model.cursor();
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    subscribe_bot(&mut socket, &bot, &start).await;
    let sent = server
        .app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:slow] stream me",
            Some("synthetic"),
        )
        .await;
    // Drop the connection in the middle of the reply.
    read_until(&mut socket, &mut model, |m| {
        m.items.values().any(|i| i["kind"] == "assistant")
    })
    .await;
    drop(socket);
    wait_completed(&server, &sent).await;
    let latest = server.snapshot(&bot).await;
    let target = latest_seq(&latest);
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    subscribe_bot(&mut socket, &bot, &model.cursor()).await;
    read_until(&mut socket, &mut model, |m| m.applied >= target).await;
    assert_eq!(
        model.texts(),
        snapshot_texts(&latest),
        "no duplicated or corrupted bubble"
    );
    // Resubscribing from the start replays duplicates that change nothing.
    let before = model.texts();
    subscribe_bot(&mut socket, &bot, &start).await;
    read_until(&mut socket, &mut model, |m| m.duplicates >= target as usize).await;
    assert_eq!(model.texts(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_socket_carries_several_channels_and_unsubscribe_stops_one() {
    let server = Server::start().await;
    let a = server.app.main_bot().await;
    let b = server.app.create_bot("B", "b").await;
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    for bot in [&a, &b] {
        let cursor = server.snapshot(bot).await["event_cursor"]
            .as_str()
            .unwrap()
            .to_owned();
        subscribe_bot(&mut socket, bot, &cursor).await;
    }
    op(
        &mut socket,
        json!({ "op": "unsubscribe", "channel": format!("bot:{a}") }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sa = server
        .app
        .send(&a, Uuid::new_v4(), "to a", Some("synthetic"))
        .await;
    let sb = server
        .app
        .send(&b, Uuid::new_v4(), "to b", Some("synthetic"))
        .await;
    wait_completed(&server, &sa).await;
    wait_completed(&server, &sb).await;
    let mut channels = Vec::new();
    while let Some(message) = next(&mut socket, Duration::from_millis(1500)).await {
        channels.push(message["channel"].as_str().unwrap().to_owned());
    }
    assert!(channels.iter().any(|c| *c == format!("bot:{b}")));
    assert!(
        channels.iter().all(|c| *c != format!("bot:{a}")),
        "unsubscribed channel is silent"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_channel_pushes_list_changes() {
    let server = Server::start().await;
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    op(&mut socket, json!({ "op": "subscribe", "channel": "bots" })).await;
    let first = next(&mut socket, Duration::from_secs(5)).await.unwrap();
    assert_eq!(
        (first["channel"].clone(), first["event"].clone()),
        (json!("bots"), json!("bots.snapshot"))
    );
    assert_eq!(first["data"]["bots"].as_array().unwrap().len(), 1);
    let main = first["data"]["default_bot_id"].as_str().unwrap().to_owned();
    let created = server.app.create_bot("Pushed", "role").await;
    let started = std::time::Instant::now();
    let update = next(&mut socket, Duration::from_secs(5)).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "pushed promptly, not polled"
    );
    let ids: Vec<_> = update["data"]["bots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].clone())
        .collect();
    assert!(ids.contains(&json!(created)));
    // Activity changes are pushed too.
    let sent = server
        .app
        .send(
            &created,
            Uuid::new_v4(),
            "[synthetic:hang]",
            Some("synthetic"),
        )
        .await;
    let mut saw_working = false;
    for _ in 0..10 {
        let Some(update) = next(&mut socket, Duration::from_secs(3)).await else {
            break;
        };
        if update["data"]["bots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["activity"]["phase"] == "working")
        {
            saw_working = true;
            break;
        }
    }
    assert!(saw_working);
    let _ = sent;
    server
        .app
        .owner("DELETE", &format!("/v1/bots/{main}"), None)
        .await;
    let mut cleared = false;
    for _ in 0..10 {
        let Some(update) = next(&mut socket, Duration::from_secs(3)).await else {
            break;
        };
        if update["data"]["default_bot_id"].is_null() {
            cleared = true;
            break;
        }
    }
    assert!(
        cleared,
        "deleting the main bot clears the default for subscribers"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_errors_are_reported_per_channel() {
    let server = Server::start().await;
    let bot = server.app.main_bot().await;
    let other = server.app.create_bot("Other", "other").await;
    let cursor = server.snapshot(&bot).await["event_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let (instance, rest) = cursor.split_once(':').unwrap();
    let (_, seq) = rest.split_once(':').unwrap();
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    let cases = [
        (
            json!({ "op": "subscribe", "channel": format!("bot:{bot}") }),
            "cursor_required",
        ),
        (
            json!({ "op": "subscribe", "channel": format!("bot:{bot}"), "cursor": "garbage" }),
            "invalid_cursor",
        ),
        (
            json!({ "op": "subscribe", "channel": format!("bot:{bot}"), "cursor": format!("{instance}:{other}:{seq}") }),
            "invalid_cursor",
        ),
        (
            json!({ "op": "subscribe", "channel": format!("bot:{bot}"), "cursor": format!("{}:{bot}:{seq}", Uuid::new_v4()) }),
            "cursor_reset_required",
        ),
        (
            json!({ "op": "subscribe", "channel": format!("bot:{bot}"), "cursor": format!("{instance}:{bot}:{}", seq.parse::<i64>().unwrap() + 100) }),
            "cursor_reset_required",
        ),
        (
            json!({ "op": "subscribe", "channel": format!("bot:{}", Uuid::new_v4()), "cursor": cursor }),
            "invalid_cursor",
        ),
        (
            json!({ "op": "subscribe", "channel": "nonsense" }),
            "unknown_channel",
        ),
    ];
    for (message, code) in cases {
        op(&mut socket, message.clone()).await;
        let reply = next(&mut socket, Duration::from_secs(5)).await.unwrap();
        assert_eq!(reply["error"]["code"], code, "{message}");
    }
    op(&mut socket, json!({ "op": "bogus" })).await;
    let reply = next(&mut socket, Duration::from_secs(5)).await.unwrap();
    assert_eq!(reply["error"]["code"], "invalid_message");
}

#[tokio::test(flavor = "multi_thread")]
async fn ended_channels_do_not_use_up_the_subscription_limit() {
    let server = Server::start().await;
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    // 40 channels that end at once (no cursor) leave no subscription behind.
    for _ in 0..40 {
        op(
            &mut socket,
            json!({"op": "subscribe", "channel": format!("bot:{}", Uuid::new_v4())}),
        )
        .await;
    }
    for _ in 0..40 {
        let reply = next(&mut socket, Duration::from_secs(5)).await.unwrap();
        assert_eq!(reply["error"]["code"], "cursor_required", "{reply}");
    }
    op(&mut socket, json!({"op": "subscribe", "channel": "bots"})).await;
    let reply = next(&mut socket, Duration::from_secs(5)).await.unwrap();
    assert_eq!(reply["event"], "bots.snapshot", "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn deletion_ends_a_bot_channel_even_for_a_stalled_reader() {
    let server = Server::start().await;
    let bot = server.app.create_bot("Temp", "temporary").await;
    let cursor = server.snapshot(&bot).await["event_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    subscribe_bot(&mut socket, &bot, &cursor).await;
    for i in 0..3 {
        let sent = server
            .app
            .send(
                &bot,
                Uuid::new_v4(),
                &format!("burst {i}"),
                Some("synthetic"),
            )
            .await;
        wait_completed(&server, &sent).await;
    }
    server
        .app
        .owner("DELETE", &format!("/v1/bots/{bot}"), None)
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // Read everything: the channel ends with bot_deleted and nothing follows.
    let mut ended = false;
    while let Some(message) = next(&mut socket, Duration::from_millis(1500)).await {
        assert!(!ended, "no message after bot_deleted: {message}");
        if message["error"]["code"] == "bot_deleted" {
            ended = true;
        }
    }
    assert!(ended);
    // Subscribing again is refused.
    subscribe_bot(&mut socket, &bot, &cursor).await;
    let reply = next(&mut socket, Duration::from_secs(5)).await.unwrap();
    assert_eq!(reply["error"]["code"], "bot_deleted");
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrade_requires_the_owner_token_and_no_browser_origin() {
    let server = Server::start().await;
    assert_eq!(server.connect(None, None).await.err(), Some(401));
    assert_eq!(
        server
            .connect(Some("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"), None)
            .await
            .err(),
        Some(401)
    );
    assert_eq!(
        server
            .connect(Some(TOKEN), Some("http://evil.example"))
            .await
            .err(),
        Some(403)
    );
    assert!(server.connect(Some(TOKEN), None).await.is_ok());
    // Event payloads never carry internals.
    let bot = server.app.main_bot().await;
    let cursor = server.snapshot(&bot).await["event_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    subscribe_bot(&mut socket, &bot, &cursor).await;
    let sent = server
        .app
        .send(&bot, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    wait_completed(&server, &sent).await;
    while let Some(message) = next(&mut socket, Duration::from_millis(800)).await {
        let text = message.to_string();
        for secret in [TOKEN, "request_hash", "\"profile\"", "gpt-6.1-sol"] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
    }
}
