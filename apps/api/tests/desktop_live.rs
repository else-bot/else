//! The owner's desktop stream against a real guest desktop: how long a
//! pointer move takes to show up in the frames, how many frames arrive while
//! it moves, and the native cursor names for the guest's cursor shapes.
//!
//! Run with scripts/measure-desktop-latency.sh, which starts the guest image
//! in Docker and sets ELSE_DESKTOP_LATENCY_GUEST to its noVNC address and
//! ELSE_DESKTOP_LATENCY_CONTAINER to the container.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::{TOKEN, TestApp};
use else_api::computers::{
    ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, RuntimeError, RuntimeState, Spec,
    cursors, viewer,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[derive(Default)]
struct Runtime(Mutex<HashMap<String, RuntimeState>>);

#[async_trait]
impl ComputerRuntime for Runtime {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|(n, s)| (n.clone(), *s))
            .collect())
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.0
            .lock()
            .unwrap()
            .insert(spec.name.clone(), RuntimeState::Running);
        Ok(())
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.0
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Running);
        Ok(())
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        self.0
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Stopped);
        Ok(())
    }
    async fn pause(&self, name: &str) -> Result<(), RuntimeError> {
        self.0
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Paused);
        Ok(())
    }
    async fn resume(&self, name: &str) -> Result<(), RuntimeError> {
        self.0
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Running);
        Ok(())
    }
    async fn exec(
        &self,
        _: &str,
        _: &str,
        _: &[&str],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
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

/// Forwards the computer's desktop port to the real guest's noVNC.
async fn forward(port: u16, guest: String) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    tokio::spawn(async move {
        while let Ok((mut client, _)) = listener.accept().await {
            let guest = guest.clone();
            tokio::spawn(async move {
                let mut upstream = tokio::net::TcpStream::connect(guest).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
            });
        }
    });
}

struct Frame {
    width: usize,
    rgb: Vec<u8>,
}

impl Frame {
    fn decode(png: &[u8]) -> Self {
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let mut reader = decoder.read_info().unwrap();
        let mut rgb = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut rgb).unwrap();
        rgb.truncate(info.buffer_size());
        Self {
            width: info.width as usize,
            rgb,
        }
    }

    fn patch(&self, x: usize, y: usize) -> Vec<u8> {
        (y..y + 16)
            .flat_map(|row| {
                let at = (row * self.width + x) * 3;
                self.rgb[at..at + 16 * 3].to_vec()
            })
            .collect()
    }
}

async fn next(socket: &mut Socket, limit: Duration) -> Option<Message> {
    match tokio::time::timeout(limit, socket.next()).await {
        Ok(Some(Ok(message))) => Some(message),
        Ok(other) => panic!("stream ended: {other:?}"),
        Err(_) => None,
    }
}

async fn latest_frame(socket: &mut Socket, settle: Duration) -> Option<Frame> {
    let mut latest = None;
    while let Some(message) = next(socket, settle).await {
        match message {
            Message::Binary(png) => latest = Some(Frame::decode(&png)),
            Message::Text(text) => {
                let event: Value = serde_json::from_str(&text).unwrap();
                assert_ne!(event["type"], "error", "{event}");
            }
            _ => {}
        }
    }
    latest
}

async fn pointer(socket: &mut Socket, x: u16, y: u16) {
    let body = json!({ "type": "pointer", "x": x, "y": y, "buttons": 0 });
    socket
        .send(Message::Text(body.to_string().into()))
        .await
        .unwrap();
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// A desktop stream from the API under test to the real guest; the app and
/// its state directory live as long as the stream.
struct Live {
    socket: Socket,
    _app: Arc<TestApp>,
    _dir: tempfile::TempDir,
}

async fn open(query: &str) -> Option<Live> {
    let Ok(guest) = std::env::var("ELSE_DESKTOP_LATENCY_GUEST") else {
        eprintln!("skipped: run scripts/measure-desktop-latency.sh");
        return None;
    };
    let password = std::env::var("ELSE_DESKTOP_LATENCY_PASSWORD").unwrap();
    let db = Arc::new(common::TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        Arc::new(Runtime::default()),
        ComputerConfig {
            desktop: true,
            idle_after: Duration::from_secs(3600),
            lease_ttl: Duration::from_secs(300),
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
    let bot = app.create_bot("Latency").await;
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
    std::fs::write(
        manager.desktop_password_file(bot_id),
        format!("{password}\n"),
    )
    .unwrap();
    forward(port as u16, guest).await;

    let mut request = format!("ws://{addr}/v1/bots/{bot}/desktop/stream?{query}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    Some(Live {
        socket,
        _app: app,
        _dir: dir,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn pointer_moves_reach_the_viewer_quickly() {
    let Some(mut live) = open("mode=control").await else {
        return;
    };
    let socket = &mut live.socket;
    let mut frame = latest_frame(socket, Duration::from_secs(2))
        .await
        .expect("a first frame");

    let spots = [(300u16, 300u16), (900, 500)];
    let mut latencies = Vec::new();
    for i in 0..40 {
        let (x, y) = spots[i % 2];
        pointer(socket, x, y).await;
        frame = latest_frame(socket, Duration::from_millis(400))
            .await
            .unwrap_or(frame);
        let (x, y) = spots[(i + 1) % 2];
        let target = (x as usize, y as usize);
        let before = frame.patch(target.0, target.1);
        let moved = Instant::now();
        pointer(socket, x, y).await;
        let mut arrived = None;
        while moved.elapsed() < Duration::from_secs(2) {
            match next(socket, Duration::from_secs(2)).await {
                Some(Message::Binary(png)) => {
                    let next = Frame::decode(&png);
                    let changed = next.patch(target.0, target.1) != before;
                    frame = next;
                    if changed {
                        arrived = Some(moved.elapsed());
                        break;
                    }
                }
                Some(_) => {}
                None => break,
            }
        }
        let arrived = arrived.expect("the pointer shows up in a frame");
        latencies.push(arrived.as_secs_f64() * 1000.);
    }

    let started = Instant::now();
    let mut frames = 0;
    let mut sweep = tokio::time::interval(Duration::from_millis(8));
    let mut step = 0u16;
    while started.elapsed() < Duration::from_secs(3) {
        tokio::select! {
            _ = sweep.tick() => {
                step = (step + 7) % 800;
                pointer(socket, 200 + step, 200 + step / 2).await;
            }
            message = socket.next() => {
                if matches!(message, Some(Ok(Message::Binary(_)))) {
                    frames += 1;
                }
            }
        }
    }
    let fps = f64::from(frames) / started.elapsed().as_secs_f64();
    latest_frame(socket, Duration::from_millis(500)).await;
    let mut idle_frames = 0;
    let idle = Instant::now();
    while let Some(remaining) = Duration::from_secs(10).checked_sub(idle.elapsed()) {
        match next(socket, remaining).await {
            Some(Message::Binary(_)) => idle_frames += 1,
            Some(_) => {}
            None => break,
        }
    }

    latencies.sort_by(f64::total_cmp);
    let report = json!({
        "samples": latencies.len(),
        "pointer_to_frame_ms": {
            "min": latencies[0],
            "p50": percentile(&latencies, 0.5),
            "p90": percentile(&latencies, 0.9),
            "max": latencies[latencies.len() - 1],
        },
        "frames_per_second_while_moving": fps,
        "frames_while_idle_10s": idle_frames,
    });
    println!("{report:#}");
    if let Ok(path) = std::env::var("ELSE_DESKTOP_LATENCY_OUT") {
        std::fs::write(path, format!("{report:#}\n")).unwrap();
    }
}

/// Shows each named cursor over a full-screen window, printing its name once
/// it shows and waiting for a line on stdin before the next.
const SHOW_CURSORS: &str = r#"
import ctypes, sys, time
x = ctypes.cdll.LoadLibrary("libX11.so.6")
xc = ctypes.cdll.LoadLibrary("libXcursor.so.1")
x.XOpenDisplay.restype = ctypes.c_void_p
xc.XcursorLibraryLoadCursor.restype = ctypes.c_ulong
d = ctypes.c_void_p(x.XOpenDisplay(None))
root = ctypes.c_ulong(x.XDefaultRootWindow(d))
window = ctypes.c_ulong(x.XCreateSimpleWindow(d, root, 0, 0, 1280, 800, 0, 0, 0))
x.XMapRaised(d, window)
for name in sys.argv[1:]:
    cursor = xc.XcursorLibraryLoadCursor(d, name.encode())
    assert cursor, name
    x.XDefineCursor(d, window, ctypes.c_ulong(cursor))
    for step in (640, 641, 640):
        x.XWarpPointer(d, 0, root, 0, 0, 0, 0, step, 400)
        x.XSync(d, 0)
        time.sleep(0.05)
    print(name, flush=True)
    sys.stdin.readline()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn every_native_cursor_name_matches_its_guest_shape() {
    let (Ok(guest), Ok(container)) = (
        std::env::var("ELSE_DESKTOP_LATENCY_GUEST"),
        std::env::var("ELSE_DESKTOP_LATENCY_CONTAINER"),
    ) else {
        eprintln!("skipped: run scripts/measure-desktop-latency.sh");
        return;
    };
    let password = std::env::var("ELSE_DESKTOP_LATENCY_PASSWORD").unwrap();
    let port: u16 = guest.rsplit(':').next().unwrap().parse().unwrap();
    let connection = viewer::connect(port, &password, viewer::Target::Watch)
        .await
        .unwrap();
    let mut writer = connection.writer;
    let (tx, mut events) = tokio::sync::mpsc::channel(64);
    tokio::spawn(viewer::read_events(
        connection.reader,
        connection.width,
        connection.height,
        Some(tx),
    ));
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (width, height) = (connection.width, connection.height);
    writer
        .write_all(&viewer::set_encodings(true))
        .await
        .unwrap();
    writer
        .write_all(&viewer::update_request(false, width, height))
        .await
        .unwrap();

    let mut helper = tokio::process::Command::new("docker")
        .args([
            "exec",
            "-i",
            "-u",
            "elsebot",
            "-e",
            "DISPLAY=:99",
            &container,
        ])
        .args(["python3", "-c", SHOW_CURSORS])
        .args(cursors::STYLES)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut next = helper.stdin.take().unwrap();
    let mut shown = tokio::io::BufReader::new(helper.stdout.take().unwrap()).lines();
    let mut table: Vec<(u64, &str)> = Vec::new();
    let mut mismatches = Vec::new();
    for name in cursors::STYLES {
        assert_eq!(shown.next_line().await.unwrap().as_deref(), Some(*name));
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut shape = None;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Some(viewer::Event::Cursor {
                    hotspot_x,
                    hotspot_y,
                    width: w,
                    height: h,
                    mask,
                })) => shape = Some(((hotspot_x, hotspot_y), (w, h), mask)),
                Ok(Some(viewer::Event::UpdateEnd)) => {
                    writer
                        .write_all(&viewer::update_request(true, width, height))
                        .await
                        .unwrap();
                    if shape.is_some() {
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        next.write_all(b"\n").await.unwrap();
        let (hotspot, size, mask) = shape.unwrap_or_else(|| panic!("no shape for {name}"));
        let print = cursors::fingerprint(hotspot, size, &mask);
        if table.iter().any(|(known, _)| *known == print) {
            continue;
        }
        table.push((print, name));
        if cursors::style(hotspot, size, &mask) != *name {
            mismatches.push(*name);
        }
    }
    let generated: String = table
        .iter()
        .map(|(print, name)| format!("    ({print:#018x}, {name:?}),\n"))
        .collect();
    assert!(
        mismatches.is_empty(),
        "cursors::SHAPES is stale for {mismatches:?}; replace it with:\n{generated}"
    );
}

/// Waits for a `{"type":"cursor"}` message, counting frames meanwhile.
async fn next_cursor(socket: &mut Socket, frames: &mut u32) -> Option<String> {
    while let Some(message) = next(socket, Duration::from_secs(3)).await {
        match message {
            Message::Binary(_) => *frames += 1,
            Message::Text(text) => {
                let event: Value = serde_json::from_str(&text).unwrap();
                assert_ne!(event["type"], "error", "{event}");
                if event["type"] == "cursor" {
                    return Some(event["style"].as_str().unwrap().to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_cursor_moves_without_frames_and_follows_the_guest_shape() {
    let Ok(container) = std::env::var("ELSE_DESKTOP_LATENCY_CONTAINER") else {
        eprintln!("skipped: run scripts/measure-desktop-latency.sh");
        return;
    };
    let Some(mut live) = open("mode=control&cursor=native").await else {
        return;
    };
    let socket = &mut live.socket;
    let mut frames = 0;
    assert_eq!(
        next_cursor(socket, &mut frames).await.as_deref(),
        Some("default")
    );
    latest_frame(socket, Duration::from_millis(500)).await;

    let mut sweep = tokio::time::interval(Duration::from_millis(8));
    let started = Instant::now();
    let mut moving_frames = 0;
    let mut step = 0u16;
    while started.elapsed() < Duration::from_secs(3) {
        tokio::select! {
            _ = sweep.tick() => {
                step = (step + 7) % 800;
                pointer(socket, 200 + step, 200 + step / 2).await;
            }
            message = socket.next() => {
                if matches!(message, Some(Ok(Message::Binary(_)))) {
                    moving_frames += 1;
                }
            }
        }
    }
    println!("frames while the native cursor moved for 3 s: {moving_frames}");
    assert_eq!(
        moving_frames, 0,
        "pointer moves over a still screen need no frames"
    );

    let mut helper = tokio::process::Command::new("docker")
        .args([
            "exec",
            "-i",
            "-u",
            "elsebot",
            "-e",
            "DISPLAY=:99",
            &container,
        ])
        .args(["python3", "-c", SHOW_CURSORS, "text"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    assert_eq!(
        next_cursor(socket, &mut frames).await.as_deref(),
        Some("text")
    );
    helper.kill().await.unwrap();
    assert!(
        latest_frame(socket, Duration::from_secs(1)).await.is_some(),
        "the helper window closed"
    );
    assert!(
        latest_frame(socket, Duration::from_millis(500)).await.is_none(),
        "the screen is still before watching"
    );

    let body = json!({ "type": "mode", "mode": "watch" });
    socket
        .send(Message::Text(body.to_string().into()))
        .await
        .unwrap();
    assert!(
        latest_frame(socket, Duration::from_millis(500))
            .await
            .is_some(),
        "watching paints the cursor into the frames again"
    );
}
