//! POC-006: snapshots and SSE replay over actual HTTP against PostgreSQL.

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::{TOKEN, TestApp};
use serde_json::{Value, json};
use uuid::Uuid;

/// The API served on an ephemeral loopback port.
struct Server {
    app: Arc<TestApp>,
    base: String,
    client: reqwest::Client,
}

impl Server {
    async fn start() -> Self {
        let app = Arc::new(TestApp::installed().await);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = app.router.clone();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(20))
            .build()
            .unwrap();
        Self { app, base, client }
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = self
            .client
            .get(format!("{}{path}", self.base))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn open(
        &self,
        bot: &str,
        cursor: Option<&str>,
        token: Option<&str>,
    ) -> reqwest::Response {
        let mut request = self
            .client
            .get(format!("{}/v1/bots/{bot}/events", self.base));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(cursor) = cursor {
            request = request.header("last-event-id", cursor);
        }
        request.send().await.unwrap()
    }
}

#[derive(Debug, Clone)]
struct Frame {
    id: Option<String>,
    event: String,
    data: Value,
}

/// Incremental SSE parser over raw bytes. Frames end at a blank line; bytes
/// are only decoded once a full frame is present, so multi-byte characters
/// split across network chunks are handled. Comments are skipped.
#[derive(Default)]
struct Parser {
    buffer: Vec<u8>,
}

impl Parser {
    fn push(&mut self, bytes: &[u8]) -> Vec<Frame> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        while let Some(end) = self.buffer.windows(2).position(|w| w == b"\n\n") {
            let raw: Vec<u8> = self.buffer.drain(..end + 2).collect();
            let text = std::str::from_utf8(&raw[..end]).expect("complete frame is valid UTF-8");
            let (mut id, mut event, mut data) = (None, String::new(), String::new());
            for line in text.split('\n') {
                if line.starts_with(':') {
                    continue;
                }
                let (field, value) = line.split_once(':').unwrap_or((line, ""));
                let value = value.strip_prefix(' ').unwrap_or(value);
                match field {
                    "id" => id = Some(value.to_owned()),
                    "event" => event = value.to_owned(),
                    "data" => data.push_str(value),
                    _ => {}
                }
            }
            if !event.is_empty() {
                frames.push(Frame {
                    id,
                    event,
                    data: serde_json::from_str(&data).unwrap(),
                });
            }
        }
        frames
    }
}

/// The client-side model: applies only the next contiguous event, ignores
/// replayed ones, and rejects offset mismatches.
struct Model {
    cursor_prefix: String,
    applied: i64,
    items: HashMap<String, Value>,
    duplicates_ignored: usize,
}

impl Model {
    fn from_snapshot(snapshot: &Value) -> Self {
        let cursor = snapshot["event_cursor"].as_str().unwrap();
        let (prefix, seq) = cursor.rsplit_once(':').unwrap();
        let items = snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| (item["id"].as_str().unwrap().to_owned(), item.clone()))
            .collect();
        Self {
            cursor_prefix: prefix.to_owned(),
            applied: seq.parse().unwrap(),
            items,
            duplicates_ignored: 0,
        }
    }

    fn cursor(&self) -> String {
        format!("{}:{}", self.cursor_prefix, self.applied)
    }

    fn apply(&mut self, frame: &Frame) {
        let id = frame.id.as_deref().unwrap();
        let (prefix, seq) = id.rsplit_once(':').unwrap();
        assert_eq!(
            prefix, self.cursor_prefix,
            "cursor bound to instance and bot"
        );
        let seq: i64 = seq.parse().unwrap();
        if seq <= self.applied {
            self.duplicates_ignored += 1;
            return;
        }
        assert_eq!(seq, self.applied + 1, "events are contiguous");
        match frame.event.as_str() {
            "item.upsert" => {
                let item = frame.data["item"].clone();
                self.items
                    .insert(item["id"].as_str().unwrap().to_owned(), item);
            }
            "item.delta" => {
                let item = self
                    .items
                    .get_mut(frame.data["item_id"].as_str().unwrap())
                    .expect("delta for a known item");
                let text = item["text"].as_str().unwrap().to_owned();
                assert_eq!(
                    frame.data["offset_utf8"].as_u64().unwrap() as usize,
                    text.len(),
                    "delta offset matches current UTF-8 length"
                );
                item["text"] = json!(text + frame.data["text"].as_str().unwrap());
            }
            "turn.updated" | "bot.updated" => {}
            other => panic!("unknown event kind {other}"),
        }
        self.applied = seq;
    }

    fn texts(&self) -> Vec<(i64, String)> {
        let mut items: Vec<_> = self
            .items
            .values()
            .map(|item| {
                (
                    item["seq"].as_i64().unwrap(),
                    item["text"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        items.sort();
        items
    }
}

fn snapshot_texts(snapshot: &Value) -> Vec<(i64, String)> {
    snapshot["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["seq"].as_i64().unwrap(),
                item["text"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Read frames until `done` is true or the deadline passes. Returns the
/// frames and whether the stream ended.
async fn read_until(
    response: &mut reqwest::Response,
    parser: &mut Parser,
    model: &mut Model,
    frames: &mut Vec<Frame>,
    mut done: impl FnMut(&Model) -> bool,
) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if done(model) {
            return false;
        }
        match tokio::time::timeout_at(deadline, response.chunk()).await {
            Err(_) => panic!("timed out waiting for events"),
            Ok(Ok(Some(bytes))) => {
                // Feed one byte at a time to exercise fragmented frames and
                // multi-byte characters split across reads.
                for byte in bytes.iter() {
                    for frame in parser.push(std::slice::from_ref(byte)) {
                        model.apply(&frame);
                        frames.push(frame);
                    }
                }
            }
            Ok(Ok(None)) => return true,
            Ok(Err(err)) => panic!("stream error: {err}"),
        }
    }
}

async fn wait_completed(server: &Server, turn: &str) {
    server.app.wait_turn(turn, &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_then_stream_captures_changes_in_between() {
    let server = Server::start().await;
    let bot = server.app.main_bot().await;
    let (status, snapshot) = server.get(&format!("/v1/bots/{bot}/conversation")).await;
    assert_eq!(status, 200);
    assert_eq!(snapshot["items"], json!([]));
    let mut model = Model::from_snapshot(&snapshot);

    // Commit a full exchange after the snapshot but before subscribing.
    let sent = server
        .app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:slow] héllo ✓ wörld",
            Some("synthetic"),
        )
        .await;
    wait_completed(&server, sent.body["turn"]["id"].as_str().unwrap()).await;

    let mut response = server.open(&bot, Some(&model.cursor()), Some(TOKEN)).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(
        response.headers()["cache-control"],
        "no-cache, no-transform"
    );
    let (_, latest) = server.get(&format!("/v1/bots/{bot}/conversation")).await;
    let target: i64 = latest["event_cursor"]
        .as_str()
        .unwrap()
        .rsplit_once(':')
        .unwrap()
        .1
        .parse()
        .unwrap();
    let (mut parser, mut frames) = (Parser::default(), Vec::new());
    read_until(&mut response, &mut parser, &mut model, &mut frames, |m| {
        m.applied >= target
    })
    .await;
    assert_eq!(model.texts(), snapshot_texts(&latest));
    assert!(model.texts()[1].1.contains("héllo ✓ wörld"));
    assert!(
        frames.iter().any(|f| f.event == "item.delta"),
        "assistant text streamed as deltas"
    );
    let terminal = frames
        .iter()
        .filter(|f| f.event == "turn.updated")
        .last()
        .unwrap();
    assert_eq!(terminal.data["turn"]["status"], "completed");

    // Nothing internal is exposed.
    for frame in &frames {
        let text = frame.data.to_string();
        for secret in [TOKEN, "request_hash", "\"profile\"", "gpt-6.1-sol"] {
            assert!(!text.contains(secret), "{secret} leaked in {}", frame.event);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_streaming_reconnect_and_duplicate_replay() {
    let server = Server::start().await;
    let bot = server.app.main_bot().await;
    let (_, snapshot) = server.get(&format!("/v1/bots/{bot}/conversation")).await;
    let mut model = Model::from_snapshot(&snapshot);
    let start_cursor = model.cursor();
    let mut response = server.open(&bot, Some(&start_cursor), Some(TOKEN)).await;

    let sent = server
        .app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:slow] stream me",
            Some("synthetic"),
        )
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    let (mut parser, mut frames) = (Parser::default(), Vec::new());
    // Disconnect in the middle of the assistant's output.
    read_until(&mut response, &mut parser, &mut model, &mut frames, |m| {
        m.items.values().any(|i| i["kind"] == "assistant")
    })
    .await;
    drop(response);
    let partial = model.texts();

    // Reconnect from the last applied cursor and finish.
    wait_completed(&server, &turn).await;
    let mut response = server.open(&bot, Some(&model.cursor()), Some(TOKEN)).await;
    let (_, latest) = server.get(&format!("/v1/bots/{bot}/conversation")).await;
    let target: i64 = latest["event_cursor"]
        .as_str()
        .unwrap()
        .rsplit_once(':')
        .unwrap()
        .1
        .parse()
        .unwrap();
    let mut parser = Parser::default();
    read_until(&mut response, &mut parser, &mut model, &mut frames, |m| {
        m.applied >= target
    })
    .await;
    assert_eq!(
        model.texts(),
        snapshot_texts(&latest),
        "no duplicated or corrupted bubble"
    );
    assert_eq!(model.texts().len(), 2);
    assert!(partial.len() <= 2);

    // A replay from an older cursor resends events the model already has;
    // they are ignored by ID and change nothing.
    let mut response = server.open(&bot, Some(&start_cursor), Some(TOKEN)).await;
    let before = model.texts();
    let mut parser = Parser::default();
    let ignored_before = model.duplicates_ignored;
    read_until(
        &mut response,
        &mut parser,
        &mut model,
        &mut Vec::new(),
        |m| m.duplicates_ignored >= ignored_before + target as usize,
    )
    .await;
    assert_eq!(model.texts(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_and_stale_cursors() {
    let server = Server::start().await;
    let bot = server.app.main_bot().await;
    let other = server.app.create_bot("Other", "other").await;
    let (_, snapshot) = server.get(&format!("/v1/bots/{bot}/conversation")).await;
    let cursor = snapshot["event_cursor"].as_str().unwrap();
    let (instance, rest) = cursor.split_once(':').unwrap();
    let (_, seq) = rest.split_once(':').unwrap();
    let cases = [
        (None, 400, "cursor_required"),
        (Some("garbage".to_owned()), 400, "invalid_cursor"),
        (
            Some(format!("{instance}:{other}:{seq}")),
            400,
            "invalid_cursor",
        ),
        (
            Some(format!("{}:{bot}:{seq}", Uuid::new_v4())),
            409,
            "cursor_reset_required",
        ),
        (
            Some(format!(
                "{instance}:{bot}:{}",
                seq.parse::<i64>().unwrap() + 100
            )),
            409,
            "cursor_reset_required",
        ),
    ];
    for (cursor, status, code) in cases {
        let response = server.open(&bot, cursor.as_deref(), Some(TOKEN)).await;
        assert_eq!(response.status().as_u16(), status, "{cursor:?}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["code"], code);
    }
    for token in [None, Some("BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB")] {
        let response = server.open(&bot, Some(cursor), token).await;
        assert_eq!(response.status().as_u16(), 401);
    }
    let (status, _) = server
        .get(&format!("/v1/bots/{}/conversation", Uuid::new_v4()))
        .await;
    assert_eq!(status, 404);
    let (status, _) = server
        .get(&format!("/v1/bots/{bot}/conversation?limit=0"))
        .await;
    assert_eq!(status, 400);
}

#[tokio::test(flavor = "multi_thread")]
async fn older_history_pages_in_order() {
    let server = Server::start().await;
    let bot = server.app.main_bot().await;
    for i in 0..5 {
        let sent = server
            .app
            .send(&bot, Uuid::new_v4(), &format!("m{i}"), Some("synthetic"))
            .await;
        wait_completed(&server, sent.body["turn"]["id"].as_str().unwrap()).await;
    }
    let (_, page) = server
        .get(&format!("/v1/bots/{bot}/conversation?limit=4"))
        .await;
    let seqs = |page: &Value| -> Vec<i64> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["seq"].as_i64().unwrap())
            .collect()
    };
    assert_eq!(seqs(&page), vec![7, 8, 9, 10]);
    assert_eq!(page["older_before_seq"], 7);
    let mut all = seqs(&page);
    let mut before = page["older_before_seq"].as_i64();
    while let Some(seq) = before {
        let (_, older) = server
            .get(&format!(
                "/v1/bots/{bot}/conversation?limit=4&before_seq={seq}"
            ))
            .await;
        let mut page_seqs = seqs(&older);
        page_seqs.extend(all);
        all = page_seqs;
        before = older["older_before_seq"].as_i64();
    }
    assert_eq!(all, (1..=10).collect::<Vec<_>>());
    // Every returned item's turn is included.
    let turn_ids: Vec<_> = page["turns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].clone())
        .collect();
    for item in page["items"].as_array().unwrap() {
        assert!(turn_ids.contains(&item["turn_id"]));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn deletion_closes_the_stream_and_access() {
    let server = Server::start().await;
    let bot = server.app.create_bot("Temp", "temporary").await;
    let (_, snapshot) = server.get(&format!("/v1/bots/{bot}/conversation")).await;
    let mut model = Model::from_snapshot(&snapshot);
    let mut response = server.open(&bot, Some(&model.cursor()), Some(TOKEN)).await;
    assert_eq!(response.status(), 200);
    server
        .app
        .owner("DELETE", &format!("/v1/bots/{bot}"), None)
        .await;
    let started = std::time::Instant::now();
    let ended = read_until(
        &mut response,
        &mut Parser::default(),
        &mut model,
        &mut Vec::new(),
        |_| false,
    )
    .await;
    assert!(ended, "stream closed");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "closed promptly"
    );
    let response = server.open(&bot, Some(&model.cursor()), Some(TOKEN)).await;
    assert_eq!(response.status().as_u16(), 410);
    let (status, body) = server.get(&format!("/v1/bots/{bot}/conversation")).await;
    assert_eq!(
        (status, body["error"]["code"].clone()),
        (410, json!("bot_deleted"))
    );
}

#[test]
fn parser_handles_split_multibyte_characters() {
    let frame = "id: a:b:1\nevent: item.delta\ndata: {\"text\":\"✓é\"}\n\n";
    let bytes = frame.as_bytes();
    for split in 0..bytes.len() {
        let mut parser = Parser::default();
        let mut frames = parser.push(&bytes[..split]);
        frames.extend(parser.push(&bytes[split..]));
        assert_eq!(frames.len(), 1, "split at {split}");
        assert_eq!(frames[0].data["text"], "✓é");
    }
}
