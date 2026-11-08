mod common;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{TOKEN, TestApp};
use else_api::computers::{
    ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, RuntimeError, RuntimeState, Spec,
    viewer,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Default)]
struct Runtime {
    computers: Mutex<HashMap<String, RuntimeState>>,
    calls: Mutex<Vec<String>>,
    hang_exec: AtomicBool,
}

impl Runtime {
    fn set(&self, name: &str, state: RuntimeState) -> Result<(), RuntimeError> {
        self.calls.lock().unwrap().push(format!("{state:?} {name}"));
        self.computers.lock().unwrap().insert(name.into(), state);
        Ok(())
    }
}

#[async_trait]
impl ComputerRuntime for Runtime {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        Ok(self
            .computers
            .lock()
            .unwrap()
            .iter()
            .map(|(n, s)| (n.clone(), *s))
            .collect())
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.set(&spec.name, RuntimeState::Running)
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Running)
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Stopped)
    }
    async fn pause(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Paused)
    }
    async fn resume(&self, name: &str) -> Result<(), RuntimeError> {
        self.set(name, RuntimeState::Running)
    }
    async fn exec(
        &self,
        name: &str,
        _: &str,
        _: &[&str],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        if self.hang_exec.load(Ordering::SeqCst) {
            self.calls.lock().unwrap().push(format!("Setup {name}"));
            std::future::pending::<()>().await;
        }
        Ok(ExecOutput {
            status: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        })
    }
    async fn copy_out(&self, _: &str, _: &str, _: &std::path::Path) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[derive(Default)]
struct Log(Mutex<Vec<String>>);

impl Log {
    fn push(&self, line: String) {
        self.0.lock().unwrap().push(line);
    }
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn input(&self, server: &str) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| l.starts_with(server) && (l.contains(" key ") || l.contains(" pointer ")))
            .collect()
    }
}

#[allow(clippy::result_large_err)] // the handshake callback's signature is tungstenite's
async fn fake_guest(port: u16, password: String, old_image: bool, log: Arc<Log>) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let password = password.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let host = Arc::new(Mutex::new(String::new()));
                let seen = host.clone();
                let ws = tokio_tungstenite::accept_hdr_async(
                    tcp,
                    move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                          mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                        *seen.lock().unwrap() = request
                            .headers()
                            .get("host")
                            .and_then(|h| h.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        // websockify answers noVNC's binary subprotocol.
                        response
                            .headers_mut()
                            .insert("sec-websocket-protocol", "binary".parse().unwrap());
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                let watch = !old_image && host.lock().unwrap().as_str() == "else-watch";
                let server = if watch { "watch" } else { "control" };
                log.push(format!("{server} open"));
                let _ = rfb_server(ws, &password, watch, server, &log).await;
                log.push(format!("{server} closed"));
            });
        }
    });
}

struct Io {
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    buffer: VecDeque<u8>,
}

impl Io {
    async fn take(&mut self, n: usize) -> Option<Vec<u8>> {
        while self.buffer.len() < n {
            match self.ws.next().await? {
                Ok(Message::Binary(data)) => self.buffer.extend(data.iter()),
                Ok(Message::Close(_)) | Err(_) => return None,
                Ok(_) => {}
            }
        }
        Some(self.buffer.drain(..n).collect())
    }
    async fn send(&mut self, bytes: &[u8]) -> Option<()> {
        self.ws
            .send(Message::Binary(bytes.to_vec().into()))
            .await
            .ok()
    }
}

const W: u16 = 64;
const H: u16 = 48;

async fn rfb_server(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    password: &str,
    watch: bool,
    server: &str,
    log: &Log,
) -> Option<()> {
    let mut io = Io {
        ws,
        buffer: VecDeque::new(),
    };
    io.send(b"RFB 003.008\n").await?;
    io.take(12).await?;
    io.send(&[1, 2]).await?;
    assert_eq!(io.take(1).await?, vec![2]);
    let challenge = *b"fedcba9876543210";
    io.send(&challenge).await?;
    let response = io.take(16).await?;
    let ok = response == viewer::vnc_auth_response(password, challenge);
    io.send(&(!ok as u32).to_be_bytes()).await?;
    assert!(ok, "the viewer authenticates with the computer's password");
    io.take(1).await?;
    let name: &[u8] = if watch { b"else-watch" } else { b"x11vnc" };
    let mut init = Vec::new();
    init.extend_from_slice(&W.to_be_bytes());
    init.extend_from_slice(&H.to_be_bytes());
    init.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
    init.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init.extend_from_slice(name);
    io.send(&init).await?;
    let mut shade = 0u8;
    // A real server holds an incremental request until the screen changes;
    // this screen changes every 20 ms.
    let mut held: Option<tokio::time::Instant> = None;
    let mut cursor_shapes = false;
    let mut cursor_due = false;
    loop {
        let kind = tokio::select! {
            kind = io.take(1) => kind?[0],
            _ = tokio::time::sleep_until(held.unwrap_or_else(tokio::time::Instant::now)), if held.is_some() => {
                held = None;
                if cursor_due {
                    cursor_due = false;
                    io.send(&cursor_update()).await?;
                    continue;
                }
                shade = shade.wrapping_add(40);
                io.send(&full_update(shade)).await?;
                continue;
            }
        };
        match kind {
            0 => {
                io.take(19).await?;
            }
            2 => {
                let head = io.take(3).await?;
                let n = u16::from_be_bytes([head[1], head[2]]) as usize;
                let encodings = io.take(n * 4).await?;
                let shapes = encodings
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|e| i32::from_be_bytes(*e) == -239);
                if shapes != cursor_shapes {
                    cursor_shapes = shapes;
                    cursor_due = shapes;
                    log.push(format!("{server} cursor shapes {shapes}"));
                }
            }
            3 => {
                let request = io.take(9).await?;
                if request[0] == 1 {
                    held.get_or_insert_with(|| {
                        tokio::time::Instant::now() + Duration::from_millis(20)
                    });
                } else {
                    shade = shade.wrapping_add(40);
                    io.send(&full_update(shade)).await?;
                }
            }
            4 => {
                let m = io.take(7).await?;
                let key = u32::from_be_bytes([m[3], m[4], m[5], m[6]]);
                log.push(format!("{server} key {key} {}", m[0]));
            }
            5 => {
                let m = io.take(5).await?;
                let x = u16::from_be_bytes([m[1], m[2]]);
                let y = u16::from_be_bytes([m[3], m[4]]);
                log.push(format!("{server} pointer {} {x} {y}", m[0]));
            }
            other => panic!("unexpected client message {other}"),
        }
    }
}

/// A 2x2 cursor shape with its hotspot at the origin.
fn cursor_update() -> Vec<u8> {
    let mut update = vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 2, 0, 2];
    update.extend_from_slice(&(-239i32).to_be_bytes());
    update.extend_from_slice(&[255; 16]);
    update.extend_from_slice(&[0b1100_0000, 0b1000_0000]);
    update
}

fn full_update(shade: u8) -> Vec<u8> {
    let mut update = vec![0, 0, 0, 1, 0, 0, 0, 0];
    update.extend_from_slice(&W.to_be_bytes());
    update.extend_from_slice(&H.to_be_bytes());
    update.extend_from_slice(&0i32.to_be_bytes());
    update.extend(std::iter::repeat_n([shade, 0, 0, 0], W as usize * H as usize).flatten());
    update
}

struct Setup {
    app: Arc<TestApp>,
    runtime: Arc<Runtime>,
    manager: Arc<ComputerManager>,
    addr: String,
    log: Arc<Log>,
    bot: String,
    _dir: tempfile::TempDir,
}

async fn setup(old_image: bool, lease_ttl: Duration) -> Setup {
    let db = Arc::new(common::TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let runtime = Arc::new(Runtime::default());
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        runtime.clone(),
        ComputerConfig {
            desktop: true,
            idle_after: Duration::from_secs(3600),
            lease_ttl,
            state_dir: dir.path().to_owned(),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry = else_api::providers::Registry::new(vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(5)),
    )]);
    let app = Arc::new(TestApp::with_parts(db, 4, registry, Some(manager.clone())));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let router = app.router.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let bot = app.create_bot("Viewer").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let else_api::computers::Acquire::Ready { lease_id, .. } =
        manager.acquire(bot_id, "test:setup").await.unwrap()
    else {
        panic!("not ready");
    };
    manager.release(lease_id).await.unwrap();
    let port: i64 = sqlx::query_scalar("SELECT desktop_port FROM bot_computers WHERE bot_id = ?1")
        .bind(bot_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let password = std::fs::read_to_string(manager.desktop_password_file(bot_id))
        .unwrap()
        .trim()
        .to_owned();
    let log = Arc::new(Log::default());
    fake_guest(port as u16, password, old_image, log.clone()).await;
    Setup {
        app,
        runtime,
        manager,
        addr,
        log,
        bot,
        _dir: dir,
    }
}

impl Setup {
    async fn connect(&self, query: &str, token: Option<&str>) -> Result<Socket, u16> {
        let mut request = format!(
            "ws://{}/v1/bots/{}/desktop/stream?{query}",
            self.addr, self.bot
        )
        .into_client_request()
        .unwrap();
        if let Some(token) = token {
            request
                .headers_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        match tokio_tungstenite::connect_async(request).await {
            Ok((socket, _)) => Ok(socket),
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                Err(response.status().as_u16())
            }
            Err(err) => panic!("connect failed: {err}"),
        }
    }

    async fn open(&self, query: &str) -> Socket {
        self.connect(query, Some(TOKEN)).await.unwrap()
    }

    async fn live_viewer_leases(&self) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM computer_leases WHERE holder LIKE 'desktop-stream:%'
             AND released_at IS NULL AND expires_at > strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')",
        )
        .fetch_one(&self.app.db.pool)
        .await
        .unwrap()
    }

    async fn leases(&self, holder: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT count(*) FROM computer_leases WHERE holder LIKE ?1 AND released_at IS NULL",
        )
        .bind(holder)
        .fetch_one(&self.app.db.pool)
        .await
        .unwrap()
    }

    async fn wait_log(&self, line: &str) {
        for _ in 0..200 {
            if self.log.lines().iter().any(|l| l == line) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never logged {line:?}: {:?}", self.log.lines());
    }
}

async fn next_json(socket: &mut Socket) -> Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a message in time")
        {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
            Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => {}
            other => panic!("stream ended: {other:?}"),
        }
    }
}

async fn next_frame(socket: &mut Socket) -> Vec<u8> {
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("a frame in time")
        {
            Some(Ok(Message::Binary(png))) => return png.to_vec(),
            Some(Ok(_)) => {}
            other => panic!("stream ended: {other:?}"),
        }
    }
}

async fn send(socket: &mut Socket, body: Value) {
    socket
        .send(Message::Text(body.to_string().into()))
        .await
        .unwrap();
}

async fn ends_with(socket: &mut Socket, code: &str) {
    let message = next_json(socket).await;
    assert_eq!(message["type"], "error", "{message}");
    assert_eq!(message["code"], code, "{message}");
    loop {
        match tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .expect("closed in time")
        {
            Some(Ok(Message::Close(_)) | Err(_)) | None => return,
            Some(Ok(_)) => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stream_authenticates_and_never_wakes_unless_asked() {
    let s = setup(false, Duration::from_secs(300)).await;
    assert_eq!(s.connect("mode=watch", None).await.err(), Some(401));
    assert_eq!(
        s.connect("mode=watch", Some("wrong")).await.err(),
        Some(401)
    );
    assert_eq!(s.connect("mode=look", Some(TOKEN)).await.err(), Some(400));

    let reply = s
        .app
        .owner("GET", &format!("/v1/bots/{}/computer", s.bot), None)
        .await;
    assert_eq!(reply.body["desktop_available"], true, "{:?}", reply.body);

    let bot_id: Uuid = s.bot.parse().unwrap();
    s.manager.owner_stop(bot_id).await.unwrap();
    s.manager.owner_resume(bot_id).await.unwrap();
    let calls = s.runtime.calls.lock().unwrap().len();
    let mut socket = s.open("mode=watch").await;
    ends_with(&mut socket, "computer_not_running").await;
    assert_eq!(s.runtime.calls.lock().unwrap().len(), calls, "nothing woke");

    s.manager.owner_stop(bot_id).await.unwrap();
    let calls = s.runtime.calls.lock().unwrap().len();
    let mut socket = s.open("mode=watch&wake=true").await;
    ends_with(&mut socket, "computer_stopped").await;
    assert_eq!(s.runtime.calls.lock().unwrap().len(), calls, "nothing woke");

    s.manager.owner_resume(bot_id).await.unwrap();
    let mut socket = s.open("mode=watch&wake=true").await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"size","width":64,"height":48})
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );
    assert!(
        s.runtime
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("Running")),
        "an explicit wake starts the computer"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watching_sends_frames_and_no_input_reaches_the_desktop() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=watch").await;
    assert_eq!(next_json(&mut socket).await["type"], "size");
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );
    let png = next_frame(&mut socket).await;
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "a PNG frame");
    send(
        &mut socket,
        json!({"type":"pointer","x":5,"y":6,"buttons":1}),
    )
    .await;
    send(&mut socket, json!({"type":"key","key":0x61,"down":true})).await;
    send(&mut socket, json!({"type":"release"})).await;
    next_frame(&mut socket).await;
    next_frame(&mut socket).await;
    assert!(s.log.input("watch").is_empty(), "{:?}", s.log.lines());
    assert!(
        !s.log.lines().iter().any(|l| l.starts_with("control")),
        "watching never connects to the controlling server: {:?}",
        s.log.lines()
    );
    assert_eq!(s.live_viewer_leases().await, 1);
    drop(socket);
    s.wait_log("watch closed").await;
    for _ in 0..100 {
        if s.live_viewer_leases().await == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        s.live_viewer_leases().await,
        0,
        "the lease ends with the stream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn control_sends_input_releases_it_and_excludes_other_controllers() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=watch").await;
    next_json(&mut socket).await;
    next_json(&mut socket).await;
    send(&mut socket, json!({"type":"mode","mode":"control"})).await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"control"})
    );
    s.wait_log("control open").await;
    assert_eq!(
        s.leases("desktop-control:%").await,
        1,
        "bot input batches and browser actions defer to this lease"
    );

    let mut other = s.open("mode=control").await;
    next_json(&mut other).await;
    assert_eq!(next_json(&mut other).await["code"], "control_in_use");
    assert_eq!(
        next_json(&mut other).await,
        json!({"type":"mode","mode":"watch"})
    );

    send(
        &mut socket,
        json!({"type":"pointer","x":500,"y":7,"buttons":1}),
    )
    .await;
    send(&mut socket, json!({"type":"key","key":0x61,"down":true})).await;
    send(&mut socket, json!({"type":"release"})).await;
    s.wait_log("control key 97 0").await;
    assert_eq!(
        s.log.input("control"),
        [
            "control pointer 1 63 7",
            "control key 97 1",
            "control key 97 0",
            "control pointer 0 63 7"
        ],
        "input clamped to the screen, then released"
    );

    send(&mut socket, json!({"type":"key","key":0xffe3,"down":true})).await;
    send(&mut socket, json!({"type":"mode","mode":"watch"})).await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );
    let lines = s.log.lines();
    let closed = lines.iter().position(|l| l == "control closed");
    let released = lines.iter().position(|l| l == "control key 65507 0");
    assert!(
        released.is_some() && closed.is_some() && released < closed,
        "leaving control releases held keys, then closes before the ack: {lines:?}"
    );
    send(&mut socket, json!({"type":"key","key":0x62,"down":true})).await;
    assert_eq!(
        s.leases("desktop-control:%").await,
        0,
        "control handed back"
    );

    send(&mut other, json!({"type":"mode","mode":"control"})).await;
    assert_eq!(
        next_json(&mut other).await,
        json!({"type":"mode","mode":"control"})
    );
    send(
        &mut other,
        json!({"type":"pointer","x":1,"y":2,"buttons":4}),
    )
    .await;
    send(&mut other, json!({"type":"key","key":0x63,"down":true})).await;
    s.wait_log("control key 99 1").await;
    drop(other);
    s.wait_log("control key 99 0").await;
    s.wait_log("control pointer 0 1 2").await;
    assert!(
        !s.log.lines().iter().any(|l| l.contains("key 98")),
        "input after leaving control is dropped"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_cursor_is_named_only_while_controlling() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=control&cursor=native").await;
    let cursor = loop {
        let message = next_json(&mut socket).await;
        if message["type"] == "cursor" {
            break message;
        }
    };
    assert_eq!(cursor, json!({"type":"cursor","style":"default"}));
    s.wait_log("watch cursor shapes true").await;
    send(&mut socket, json!({"type":"mode","mode":"watch"})).await;
    s.wait_log("watch cursor shapes false").await;

    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );

    let mut painted = s.open("mode=control").await;
    loop {
        if next_json(&mut painted).await == json!({"type":"mode","mode":"control"}) {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let asked = s.log.lines();
    assert_eq!(
        asked
            .iter()
            .filter(|l| *l == "watch cursor shapes true")
            .count(),
        1,
        "a stream without cursor=native never asks for shapes: {asked:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_renew_their_lease_and_end_on_owner_stop_or_delete() {
    let s = setup(false, Duration::from_secs(3)).await;
    let mut socket = s.open("mode=control").await;
    next_json(&mut socket).await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"control"})
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(s.live_viewer_leases().await, 1, "renewed past its TTL");

    send(&mut socket, json!({"type":"key","key":0x61,"down":true})).await;
    s.wait_log("control key 97 1").await;
    let stopped = s
        .app
        .owner("POST", &format!("/v1/bots/{}/computer/stop", s.bot), None)
        .await;
    assert_eq!(stopped.status, axum::http::StatusCode::NO_CONTENT);
    ends_with(&mut socket, "computer_stopped").await;
    s.wait_log("control key 97 0").await;

    s.app
        .owner("POST", &format!("/v1/bots/{}/computer/resume", s.bot), None)
        .await;
    let mut socket = s.open("mode=watch&wake=true").await;
    next_json(&mut socket).await;
    next_json(&mut socket).await;
    s.app
        .owner("DELETE", &format!("/v1/bots/{}", s.bot), None)
        .await;
    ends_with(&mut socket, "bot_deleted").await;
    assert_eq!(s.live_viewer_leases().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_image_can_be_taken_over_but_not_watched() {
    let s = setup(true, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=watch").await;
    ends_with(&mut socket, "desktop_watch_unsupported").await;
    s.wait_log("control closed").await;

    let mut socket = s.open("mode=control").await;
    assert_eq!(next_json(&mut socket).await["type"], "size");
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"control"})
    );
    assert!(next_frame(&mut socket).await.starts_with(b"\x89PNG"));
    send(&mut socket, json!({"type":"mode","mode":"watch"})).await;
    assert_eq!(
        next_json(&mut socket).await["code"],
        "desktop_watch_unsupported"
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_releases_held_input_and_refuses_new_streams() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=control").await;
    next_json(&mut socket).await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"control"})
    );
    send(
        &mut socket,
        json!({"type":"pointer","x":3,"y":4,"buttons":1}),
    )
    .await;
    send(&mut socket, json!({"type":"key","key":0xff53,"down":true})).await;
    s.wait_log("control key 65363 1").await;
    assert_eq!(s.leases("desktop-control:%").await, 1);
    assert_eq!(s.live_viewer_leases().await, 1);

    tokio::time::timeout(Duration::from_secs(5), s.app.state.stop_desktop_streams())
        .await
        .expect("streams stop promptly");
    assert_eq!(
        s.leases("desktop-control:%").await,
        0,
        "bot input and browser actions are no longer deferred"
    );
    assert_eq!(
        s.leases("desktop-stream:%").await,
        0,
        "viewer lease released"
    );
    s.wait_log("control key 65363 0").await;
    s.wait_log("control pointer 0 3 4").await;
    let lines = s.log.lines();
    let released = lines.iter().position(|l| l == "control key 65363 0");
    let closed = lines.iter().position(|l| l == "control closed");
    assert!(
        released.is_some() && closed.is_some() && released < closed,
        "held input is released before the control connection closes: {lines:?}"
    );
    ends_with(&mut socket, "server_stopping").await;

    let opened = s.log.lines().len();
    assert_eq!(
        s.connect("mode=control", Some(TOKEN)).await.err(),
        Some(503)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        s.log.lines().len(),
        opened,
        "nothing reaches the desktop after the drain"
    );
    assert_eq!(s.leases("desktop-%").await, 0, "no lease after the drain");
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_abandons_a_stream_waiting_for_boot() {
    let s = setup(false, Duration::from_secs(300)).await;
    let bot_id: Uuid = s.bot.parse().unwrap();
    s.manager.owner_stop(bot_id).await.unwrap();
    s.manager.owner_resume(bot_id).await.unwrap();
    s.runtime.hang_exec.store(true, Ordering::SeqCst);
    let mut socket = s.open("mode=control&wake=true").await;
    for _ in 0..200 {
        if s.runtime
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("Setup"))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        s.runtime
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("Setup")),
        "the stream waits for the boot"
    );
    assert_eq!(
        s.leases("desktop-stream:%").await,
        1,
        "the boot granted a lease"
    );

    tokio::time::timeout(Duration::from_secs(5), s.app.state.stop_desktop_streams())
        .await
        .expect("a pending boot does not hold the API open");
    ends_with(&mut socket, "server_stopping").await;
    assert_eq!(s.leases("desktop-%").await, 0);
    assert!(s.log.lines().is_empty(), "{:?}", s.log.lines());
}

const LEASE_TICK: Duration = Duration::from_millis(1500);

async fn hold_key(s: &Setup, socket: &mut Socket) {
    while next_json(socket).await != json!({"type":"mode","mode":"control"}) {}
    send(socket, json!({"type":"key","key":0xff53,"down":true})).await;
    s.wait_log("control key 65363 1").await;
}

fn key_released(s: &Setup) -> bool {
    s.log
        .input("control")
        .iter()
        .any(|l| l == "control key 65363 0")
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_releases_input_a_non_reading_client_holds() {
    let s = setup(false, Duration::from_secs(300)).await;
    let tcp = tokio::net::TcpSocket::new_v4().unwrap();
    tcp.set_recv_buffer_size(1024).unwrap();
    let tcp = tcp.connect(s.addr.parse().unwrap()).await.unwrap();
    let mut request = format!(
        "ws://{}/v1/bots/{}/desktop/stream?mode=control",
        s.addr, s.bot
    )
    .into_client_request()
    .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut socket, _) =
        tokio_tungstenite::client_async(request, tokio_tungstenite::MaybeTlsStream::Plain(tcp))
            .await
            .unwrap();
    hold_key(&s, &mut socket).await;

    let (mut sink, unread) = socket.split();
    let flood =
        tokio::spawn(
            async move { while sink.send(Message::Text("not json".into())).await.is_ok() {} },
        );
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(!key_released(&s));

    let drained = tokio::time::timeout(Duration::from_secs(5), s.app.state.stop_desktop_streams())
        .await
        .is_ok();
    flood.abort();
    drop(unread);
    assert!(
        key_released(&s),
        "a client that stops reading cannot keep a key held"
    );
    assert!(drained, "the stream does not wait on its client");
    assert_eq!(s.leases("desktop-%").await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_releases_input_before_waiting_on_a_busy_database() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=control").await;
    hold_key(&s, &mut socket).await;
    let pool = &s.app.db.pool;
    let mut held = Vec::new();
    while let Ok(Ok(connection)) =
        tokio::time::timeout(Duration::from_millis(200), pool.acquire()).await
    {
        held.push(connection);
    }
    assert!(!held.is_empty());
    tokio::time::sleep(LEASE_TICK).await;

    let mut drain = Box::pin(s.app.state.stop_desktop_streams());
    for _ in 0..150 {
        if key_released(&s) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        key_released(&s),
        "no database wait precedes the input release"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut drain)
            .await
            .is_err(),
        "leases are still released, once the database answers"
    );
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .expect("streams finish once the database answers");
    assert_eq!(s.leases("desktop-%").await, 0);
    ends_with(&mut socket, "server_stopping").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_while_taking_control_leaks_no_lease_or_control() {
    let s = setup(false, Duration::from_secs(300)).await;
    let bot_id: Uuid = s.bot.parse().unwrap();
    let mut socket = s.open("mode=watch").await;
    while next_json(&mut socket).await != json!({"type":"mode","mode":"watch"}) {}
    let mut writer = s.app.db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("UPDATE bots SET name = name WHERE id = ?1")
        .bind(bot_id)
        .execute(&mut *writer)
        .await
        .unwrap();
    send(&mut socket, json!({"type":"mode","mode":"control"})).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut drain = Box::pin(s.app.state.stop_desktop_streams());
    assert!(
        tokio::time::timeout(Duration::from_millis(300), &mut drain)
            .await
            .is_err(),
        "an abandoned control lease is looked up behind the writer"
    );
    writer.commit().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), drain)
        .await
        .expect("the abandoned admission is cleaned up");
    assert_eq!(
        s.leases("desktop-%").await,
        0,
        "no lease outlives the drain"
    );
    assert!(
        s.manager.claim_control(bot_id, Uuid::new_v4()).await,
        "control ownership was released"
    );
    assert!(s.log.input("control").is_empty());
    ends_with(&mut socket, "server_stopping").await;
}
