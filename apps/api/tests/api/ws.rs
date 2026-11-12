use crate::common;

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

async fn next(socket: &mut Socket, wait: Duration) -> Option<Value> {
    loop {
        match tokio::time::timeout(wait, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => return Some(serde_json::from_str(&text).unwrap()),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Ok(Some(Err(_))) | Err(_) => return None,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

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
    let before = model.texts();
    subscribe_bot(&mut socket, &bot, &start).await;
    read_until(&mut socket, &mut model, |m| m.duplicates >= target as usize).await;
    assert_eq!(model.texts(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn one_socket_carries_several_channels_and_unsubscribe_stops_one() {
    let server = Server::start().await;
    let a = server.app.main_bot().await;
    let b = server.app.create_bot("B").await;
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
    let created = server.app.create_bot("Pushed").await;
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
        .owner("DELETE", &format!("/v1/bots/{created}"), None)
        .await;
    let mut removed = false;
    for _ in 0..10 {
        let Some(update) = next(&mut socket, Duration::from_secs(3)).await else {
            break;
        };
        if !update["data"]["bots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["id"] == json!(created))
        {
            assert_eq!(update["data"]["default_bot_id"], json!(main));
            removed = true;
            break;
        }
    }
    assert!(removed, "deleting a bot reaches subscribers");
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_channel_carries_background_replies_and_reads() {
    let server = Server::start().await;
    let background = server.app.create_bot("Background").await;
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    op(&mut socket, json!({ "op": "subscribe", "channel": "bots" })).await;
    let state = |update: &Value| {
        update["data"]["bots"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == json!(background))
            .unwrap()["read_state"]
            .clone()
    };
    let first = next(&mut socket, Duration::from_secs(5)).await.unwrap();
    assert_eq!(state(&first)["unread"], false);
    let sent = server
        .app
        .send(&background, Uuid::new_v4(), "ping ✓", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    server.app.wait_turn(&turn, &["completed"]).await;
    let mut latest = Value::Null;
    let mut previous: Option<Value> = None;
    while let Some(update) = next(&mut socket, Duration::from_secs(1)).await {
        assert_ne!(
            previous.as_ref(),
            Some(&update["data"]),
            "only changes are sent"
        );
        let read = state(&update);
        if read["unread"] == true {
            assert_eq!(read["latest"]["turn_id"], json!(turn));
            latest = read["latest"].clone();
        }
        previous = Some(update["data"].clone());
    }
    assert!(
        latest.is_object(),
        "the background reply reached the sidebar"
    );
    let item = server.snapshot(&background).await["items"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|i| i["kind"] == "assistant")
        .cloned()
        .unwrap();
    assert_eq!(
        latest["position"]["text_bytes"],
        json!(item["text"].as_str().unwrap().len()),
        "the last push carries the whole reply"
    );

    server
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{background}/read"),
            Some(json!({ "action": "through", "position": latest["position"] })),
        )
        .await;
    let update = next(&mut socket, Duration::from_secs(5)).await.unwrap();
    assert_eq!(
        state(&update)["unread"],
        false,
        "another client sees the read"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_errors_are_reported_per_channel() {
    let server = Server::start().await;
    let bot = server.app.main_bot().await;
    let other = server.app.create_bot("Other").await;
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
    let bot = server.app.create_bot("Temp").await;
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
    let mut ended = false;
    while let Some(message) = next(&mut socket, Duration::from_millis(1500)).await {
        assert!(!ended, "no message after bot_deleted: {message}");
        if message["error"]["code"] == "bot_deleted" {
            ended = true;
        }
    }
    assert!(ended);
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

#[tokio::test(flavor = "multi_thread")]
async fn bots_channel_carries_categories_to_every_client() {
    let server = Server::start().await;
    let mut clients = Vec::new();
    for _ in 0..2 {
        let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
        op(&mut socket, json!({ "op": "subscribe", "channel": "bots" })).await;
        let first = next(&mut socket, Duration::from_secs(5)).await.unwrap();
        assert_eq!(first["data"]["categories"], json!([]));
        clients.push(socket);
    }
    async fn each(clients: &mut [Socket], check: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        for socket in clients.iter_mut() {
            loop {
                let message = next(socket, Duration::from_secs(5))
                    .await
                    .expect("a snapshot arrived");
                if check(&message["data"]) {
                    last = message["data"].clone();
                    break;
                }
            }
        }
        last
    }
    let made = server
        .app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Empty" })),
        )
        .await;
    let id = made.body["category"]["id"].clone();
    each(&mut clients, |d| d["categories"][0]["name"] == "Empty").await;
    server
        .app
        .owner(
            "PATCH",
            &format!("/v1/categories/{}", id.as_str().unwrap()),
            Some(json!({ "name": "Renamed" })),
        )
        .await;
    each(&mut clients, |d| d["categories"][0]["name"] == "Renamed").await;
    let other = server
        .app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Zulu" })),
        )
        .await
        .body["category"]["id"]
        .clone();
    each(&mut clients, |d| d["categories"][1]["name"] == "Zulu").await;
    let placed = server
        .app
        .owner(
            "PUT",
            "/v1/categories/order",
            Some(json!({ "category_ids": [other, id] })),
        )
        .await;
    assert_eq!(
        placed.status,
        axum::http::StatusCode::OK,
        "{:?}",
        placed.body
    );
    each(&mut clients, |d| {
        d["categories"][0]["name"] == "Zulu" && d["categories"][1]["name"] == "Renamed"
    })
    .await;
    server
        .app
        .owner(
            "DELETE",
            &format!("/v1/categories/{}", other.as_str().unwrap()),
            None,
        )
        .await;
    each(&mut clients, |d| {
        d["categories"].as_array().unwrap().len() == 1
    })
    .await;
    let main = server.app.main_bot().await;
    server
        .app
        .owner(
            "PATCH",
            &format!("/v1/bots/{main}/category"),
            Some(json!({ "category_id": id })),
        )
        .await;
    each(&mut clients, |d| d["bots"][0]["category_id"] == id).await;
    server
        .app
        .owner(
            "DELETE",
            &format!("/v1/categories/{}", id.as_str().unwrap()),
            None,
        )
        .await;
    let after = each(&mut clients, |d| {
        d["categories"] == json!([]) && d["bots"][0]["category_id"].is_null()
    })
    .await;
    assert_eq!(
        after["bots"][0]["category_id"],
        Value::Null,
        "same snapshot"
    );
    let empty = server
        .app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Brief" })),
        )
        .await
        .body["category"]["id"]
        .clone();
    each(&mut clients, |d| d["categories"][0]["name"] == "Brief").await;
    server
        .app
        .owner(
            "DELETE",
            &format!("/v1/categories/{}", empty.as_str().unwrap()),
            None,
        )
        .await;
    let after = each(&mut clients, |d| d["categories"] == json!([])).await;
    let mut again = server.connect(Some(TOKEN), None).await.unwrap();
    op(&mut again, json!({ "op": "subscribe", "channel": "bots" })).await;
    let fresh = next(&mut again, Duration::from_secs(5)).await.unwrap();
    assert_eq!(fresh["data"], after);
}
