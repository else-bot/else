use crate::common;

use std::collections::HashMap;
use std::time::Duration;

use common::recorder::{self, GATE_AFTER_OPENING};
use common::{TOKEN, TestApp};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Server<'a> {
    app: &'a TestApp,
    addr: String,
}

impl<'a> Server<'a> {
    async fn start(app: &'a TestApp) -> Self {
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

async fn next_text(socket: &mut Socket) -> Option<Value> {
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return Some(serde_json::from_str(&text).unwrap()),
            Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
            Some(Ok(_)) => continue,
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
    common::until("the bot channel to catch up", async {
        while !done(model) {
            let message = next_text(socket).await.expect("socket closed");
            if message["channel"]
                .as_str()
                .is_some_and(|c| c.starts_with("bot:"))
            {
                model.apply(&message);
            }
        }
    })
    .await;
}

async fn wait_completed(server: &Server<'_>, reply: &common::Reply) {
    server
        .app
        .wait_turn(reply.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_then_subscribe_replays_everything_in_between() {
    let h = recorder::harness(TestApp::builder()).await;
    let server = Server::start(&h.app).await;
    let bot = server.app.main_bot().await;
    let snapshot = server.snapshot(&bot).await;
    let mut model = Model::from_snapshot(&snapshot);
    let (turn, _) = h
        .running(&bot, &format!("{GATE_AFTER_OPENING} héllo ✓ wörld"))
        .await;
    common::steer::wait_for_text(&h.app, turn, 0).await;
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
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
    let h = recorder::harness(TestApp::builder()).await;
    let server = Server::start(&h.app).await;
    let bot = server.app.main_bot().await;
    let snapshot = server.snapshot(&bot).await;
    let mut model = Model::from_snapshot(&snapshot);
    let start = model.cursor();
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    subscribe_bot(&mut socket, &bot, &start).await;
    let (turn, _) = h
        .running(&bot, &format!("{GATE_AFTER_OPENING} stream me"))
        .await;
    read_until(&mut socket, &mut model, |m| {
        m.items.values().any(|i| i["kind"] == "assistant")
    })
    .await;
    drop(socket);
    h.release(turn).await;
    server.app.wait_turn(turn, &["completed"]).await;
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
    let app = TestApp::installed().await;
    let server = Server::start(&app).await;
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
    let a_channel = format!("bot:{a}");
    let b_channel = format!("bot:{b}");
    let check = |message: Value| {
        let channel = message["channel"].as_str().unwrap();
        assert_ne!(channel, a_channel, "unsubscribed channel is silent");
        channel == b_channel
    };
    common::until("an event from the subscribed bot", async {
        while !check(next_text(&mut socket).await.expect("socket closed")) {}
    })
    .await;
    // Drain pending messages until the socket has been quiet for this window.
    const QUIET: Duration = Duration::from_millis(1500);
    while let Ok(Some(message)) = tokio::time::timeout(QUIET, next_text(&mut socket)).await {
        check(message);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_channel_pushes_list_changes() {
    let app = TestApp::installed().await;
    let server = Server::start(&app).await;
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    op(&mut socket, json!({ "op": "subscribe", "channel": "bots" })).await;
    let first = common::until("the initial bots snapshot", next_text(&mut socket))
        .await
        .unwrap();
    assert_eq!(
        (first["channel"].clone(), first["event"].clone()),
        (json!("bots"), json!("bots.snapshot"))
    );
    assert_eq!(first["data"]["bots"].as_array().unwrap().len(), 1);
    let main = first["data"]["default_bot_id"].as_str().unwrap().to_owned();
    let created = server.app.create_bot("Pushed").await;
    let started = std::time::Instant::now();
    let update = common::until("the newly created bot to appear", next_text(&mut socket))
        .await
        .unwrap();
    // A push must arrive before the 1 s polling fallback.
    const PUSH_BEFORE_POLL: Duration = Duration::from_millis(900);
    assert!(
        started.elapsed() < PUSH_BEFORE_POLL,
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
    common::until("the sidebar to show a working bot", async {
        loop {
            let update = next_text(&mut socket).await.expect("socket closed");
            if update["data"]["bots"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["activity"]["phase"] == "working")
            {
                break;
            }
        }
    })
    .await;
    let _ = sent;
    server
        .app
        .owner("DELETE", &format!("/v1/bots/{created}"), None)
        .await;
    common::until("the bot deletion to reach subscribers", async {
        loop {
            let update = next_text(&mut socket).await.expect("socket closed");
            if !update["data"]["bots"]
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["id"] == json!(created))
            {
                assert_eq!(update["data"]["default_bot_id"], json!(main));
                break;
            }
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_channel_carries_background_replies_and_reads() {
    let app = TestApp::installed().await;
    let server = Server::start(&app).await;
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
    let first = common::until("the initial bots snapshot", next_text(&mut socket))
        .await
        .unwrap();
    assert_eq!(state(&first)["unread"], false);
    let sent = server
        .app
        .send(&background, Uuid::new_v4(), "ping ✓", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    server.app.wait_turn(&turn, &["completed"]).await;
    let item = server.snapshot(&background).await["items"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|i| i["kind"] == "assistant")
        .cloned()
        .unwrap();
    let text_bytes = json!(item["text"].as_str().unwrap().len());
    let mut latest = Value::Null;
    let mut previous: Option<Value> = None;
    let mut apply = |update: Value| {
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
        latest["position"]["text_bytes"] == text_bytes
    };
    common::until("the whole background reply to reach the sidebar", async {
        while !apply(next_text(&mut socket).await.expect("socket closed")) {}
    })
    .await;
    // Drain pending messages until the socket has been quiet for this window.
    const QUIET: Duration = Duration::from_secs(1);
    while let Ok(Some(update)) = tokio::time::timeout(QUIET, next_text(&mut socket)).await {
        apply(update);
    }
    assert!(
        latest.is_object(),
        "the background reply reached the sidebar"
    );
    assert_eq!(
        latest["position"]["text_bytes"], text_bytes,
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
    let update = common::until(
        "the sidebar to show the reply as read",
        next_text(&mut socket),
    )
    .await
    .unwrap();
    assert_eq!(
        state(&update)["unread"],
        false,
        "another client sees the read"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_errors_are_reported_per_channel() {
    let app = TestApp::installed().await;
    let server = Server::start(&app).await;
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
        let reply = common::until("the subscription error", next_text(&mut socket))
            .await
            .unwrap();
        assert_eq!(reply["error"]["code"], code, "{message}");
    }
    op(&mut socket, json!({ "op": "bogus" })).await;
    let reply = common::until("the invalid operation error", next_text(&mut socket))
        .await
        .unwrap();
    assert_eq!(reply["error"]["code"], "invalid_message");
}

#[tokio::test(flavor = "multi_thread")]
async fn ended_channels_do_not_use_up_the_subscription_limit() {
    let app = TestApp::installed().await;
    let server = Server::start(&app).await;
    let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
    for _ in 0..40 {
        op(
            &mut socket,
            json!({"op": "subscribe", "channel": format!("bot:{}", Uuid::new_v4())}),
        )
        .await;
    }
    common::until("all 40 subscription errors", async {
        for _ in 0..40 {
            let reply = next_text(&mut socket).await.expect("socket closed");
            assert_eq!(reply["error"]["code"], "cursor_required", "{reply}");
        }
    })
    .await;
    op(&mut socket, json!({"op": "subscribe", "channel": "bots"})).await;
    let reply = common::until(
        "the bots snapshot after the rejected subscriptions",
        next_text(&mut socket),
    )
    .await
    .unwrap();
    assert_eq!(reply["event"], "bots.snapshot", "{reply}");
}

#[tokio::test(flavor = "multi_thread")]
async fn deletion_ends_a_bot_channel_even_for_a_stalled_reader() {
    let app = TestApp::installed().await;
    let server = Server::start(&app).await;
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
    common::until("the bot_deleted event for the stalled reader", async {
        loop {
            let message = next_text(&mut socket).await.expect("socket closed");
            if message["error"]["code"] == "bot_deleted" {
                break;
            }
        }
    })
    .await;
    common::never(
        "the socket to close or send another message after bot_deleted",
        Duration::from_millis(1500),
        next_text(&mut socket),
    )
    .await;
    subscribe_bot(&mut socket, &bot, &cursor).await;
    let reply = common::until("the deleted bot subscription error", next_text(&mut socket))
        .await
        .unwrap();
    assert_eq!(reply["error"]["code"], "bot_deleted");
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrade_requires_the_owner_token_and_no_browser_origin() {
    let app = TestApp::installed().await;
    let server = Server::start(&app).await;
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
    // Inspect queued messages until the socket has been quiet for this window.
    const QUIET: Duration = Duration::from_millis(800);
    while let Ok(Some(message)) = tokio::time::timeout(QUIET, next_text(&mut socket)).await {
        let text = message.to_string();
        for secret in [TOKEN, "request_hash", "\"profile\"", "gpt-6.1-sol"] {
            assert!(!text.contains(secret), "{secret} leaked");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn bots_channel_carries_categories_to_every_client() {
    let app = TestApp::installed().await;
    let server = Server::start(&app).await;
    let mut clients = Vec::new();
    for _ in 0..2 {
        let mut socket = server.connect(Some(TOKEN), None).await.unwrap();
        op(&mut socket, json!({ "op": "subscribe", "channel": "bots" })).await;
        let first = common::until("the initial categories snapshot", next_text(&mut socket))
            .await
            .unwrap();
        assert_eq!(first["data"]["categories"], json!([]));
        clients.push(socket);
    }
    async fn each(clients: &mut [Socket], check: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        for socket in clients.iter_mut() {
            last = common::until("the changed categories snapshot", async {
                loop {
                    let message = next_text(socket).await.expect("socket closed");
                    if check(&message["data"]) {
                        break message["data"].clone();
                    }
                }
            })
            .await;
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
    let fresh = common::until("the new client's snapshot", next_text(&mut again))
        .await
        .unwrap();
    assert_eq!(fresh["data"], after);
}
