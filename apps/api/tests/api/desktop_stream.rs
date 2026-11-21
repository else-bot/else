use crate::common;

use std::collections::VecDeque;
use std::io::Read as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::guest::Guest;
use common::{TOKEN, TestApp};
use else_api::computers::{ComputerConfig, ComputerManager, viewer};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use uuid::Uuid;

mod control;
mod shutdown;
mod watching;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

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
fn fake_guest(listener: tokio::net::TcpListener, password: String, old_image: bool, log: Arc<Log>) {
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
                let _ = rfb_server(ws, &password, watch, !old_image, server, &log).await;
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
const EXTENDED_CLIPBOARD: i32 = 0xC0A1_E5CE_u32 as i32;

/// The clipboard text in a ClientCutText, classic or ExtendedClipboard.
async fn cut_text(io: &mut Io) -> Option<String> {
    let head = io.take(7).await?;
    let len = i32::from_be_bytes([head[3], head[4], head[5], head[6]]);
    let body = io.take(len.unsigned_abs() as usize).await?;
    if len >= 0 {
        return Some(body.iter().map(|&b| char::from(b)).collect());
    }
    let flags = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    assert_eq!(flags, (1 << 28) | 1, "a Provide with text alone");
    let mut plain = Vec::new();
    flate2::read::ZlibDecoder::new(&body[4..])
        .read_to_end(&mut plain)
        .unwrap();
    let size = u32::from_be_bytes([plain[0], plain[1], plain[2], plain[3]]) as usize;
    assert_eq!(plain.len(), 4 + size);
    assert_eq!(plain.last(), Some(&0), "the text ends in a NUL");
    Some(String::from_utf8(plain[4..plain.len() - 1].to_vec()).unwrap())
}

async fn rfb_server(
    ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    password: &str,
    watch: bool,
    utf8_clipboard: bool,
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
                let has = |wanted: i32| {
                    encodings
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .any(|e| i32::from_be_bytes(*e) == wanted)
                };
                let shapes = has(-239);
                if utf8_clipboard && has(EXTENDED_CLIPBOARD) {
                    // libvncserver's capabilities: text, up to 1 MiB.
                    let mut caps = vec![3, 0, 0, 0];
                    caps.extend_from_slice(&(-8i32).to_be_bytes());
                    caps.extend_from_slice(&0x1700_0001u32.to_be_bytes());
                    caps.extend_from_slice(&(1u32 << 20).to_be_bytes());
                    io.send(&caps).await?;
                }
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
            6 => {
                let text = cut_text(&mut io).await?;
                log.push(format!("{server} clipboard {text:?}"));
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
    runtime: Arc<Guest>,
    /// While set, each guest command hangs, as a boot that never finishes.
    hang: Arc<AtomicBool>,
    booting: Arc<Notify>,
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
    let hang = Arc::new(AtomicBool::new(false));
    let booting = Arc::new(Notify::new());
    let runtime = Arc::new(Guest::new().on_exec({
        let (hang, booting) = (hang.clone(), booting.clone());
        move |_| {
            let (hang, booting) = (hang.load(Ordering::SeqCst), booting.clone());
            async move {
                if hang {
                    booting.notify_one();
                    std::future::pending::<()>().await;
                }
                None
            }
        }
    }));
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        runtime.clone(),
        ComputerConfig {
            desktop: true,
            memory_mib: 1024,
            idle_after: Duration::from_secs(3600),
            lease_ttl,
            state_dir: dir.path().to_owned(),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry: common::Providers = vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(5)),
    )];
    let app = Arc::new(
        TestApp::builder()
            .db(db)
            .providers(registry)
            .computers(manager.clone())
            .start()
            .await,
    );
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
    // The computer's port was free when it was chosen; another test may
    // have taken it since, so the guest binds its own and the row follows.
    let guest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    sqlx::query("UPDATE bot_computers SET desktop_port = ?2 WHERE bot_id = ?1")
        .bind(bot_id)
        .bind(i64::from(guest.local_addr().unwrap().port()))
        .execute(&app.db.pool)
        .await
        .unwrap();
    let password = std::fs::read_to_string(manager.desktop_password_file(bot_id))
        .unwrap()
        .trim()
        .to_owned();
    let log = Arc::new(Log::default());
    fake_guest(guest, password, old_image, log.clone());
    Setup {
        app,
        runtime,
        hang,
        booting,
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
        let logged = common::poll(|| {
            std::future::ready(self.log.lines().iter().any(|l| l == line).then_some(()))
        });
        if tokio::time::timeout(common::SETTLE, logged).await.is_err() {
            panic!("never logged {line:?}: {:?}", self.log.lines());
        }
    }

    /// Until the guest has logged a close for every connection it opened.
    async fn wait_closed(&self) {
        let closed = common::poll(|| {
            let lines = self.log.lines();
            let count = |suffix: &str| lines.iter().filter(|l| l.ends_with(suffix)).count();
            std::future::ready((count(" open") == count(" closed")).then_some(()))
        });
        if tokio::time::timeout(common::SETTLE, closed).await.is_err() {
            panic!("connections left open: {:?}", self.log.lines());
        }
    }
}

async fn next_json(socket: &mut Socket) -> Value {
    common::until("a JSON message", async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                Some(Ok(Message::Binary(_) | Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("stream ended: {other:?}"),
            }
        }
    })
    .await
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
    common::until("the desktop stream to close", async {
        while let Some(Ok(message)) = socket.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
}

async fn wait_mode(socket: &mut Socket, mode: &str) {
    let expected = json!({"type":"mode","mode":mode});
    common::until(&format!("desktop stream mode {mode}"), async {
        while next_json(socket).await != expected {}
    })
    .await;
}
