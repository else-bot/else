//! The owner's in-app desktop viewer: one WebSocket per viewer that carries
//! PNG frames out and, in control mode, pointer and key input in.
//!
//! ```text
//! GET /v1/bots/{id}/desktop/stream?mode=watch|control[&wake=true]
//! server → {"type":"size","width":W,"height":H}   first, and on every resize
//! server → {"type":"mode","mode":"watch"|"control"}  initial mode and each ack
//! server → binary PNG of the whole screen, only when it changed, ≤ 5 per second
//! server → {"type":"error","code":C,"message":M}   fatal ones close the stream
//! client → {"type":"mode","mode":"watch"|"control"}
//! client → {"type":"pointer","x":X,"y":Y,"buttons":MASK}   RFB button mask
//! client → {"type":"key","key":KEYSYM,"down":BOOL}          X11 keysym
//! client → {"type":"release"}   release every held key and button
//! ```
//!
//! Authenticated like every `/v1` route. Without `wake` the stream attaches
//! only to a running computer; with it, connecting is a use that may start
//! the computer, but never one the owner stopped. Each stream holds its own
//! lease, renewed while connected and released on close; an owner stop,
//! delete, or reboot ends the stream. Leases are checked every second. Frames come from the
//! guest's view-only VNC server, which the bot's input guard ignores, so
//! watching leaves the bot working. Control opens a second, input-only VNC
//! connection to the controlling server, which makes the bot's pointer and
//! keyboard batches fail with `OWNER_USING_DESKTOP` until control ends; at
//! most one stream controls a bot. Leaving control, `release`, and every way
//! a stream normally ends release what it pressed. In watch mode input is dropped.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiPath};
use crate::computers::viewer::{self, Event, Framebuffer, Target};
use crate::computers::{ComputerManager, ViewerLease, ViewerRefusal, ViewerTarget};

const MAX_MESSAGE: usize = 4 * 1024;
const FRAME_EVERY: Duration = Duration::from_millis(200);
const LEASE_CHECK: Duration = Duration::from_secs(1);
const PING_EVERY: Duration = Duration::from_secs(15);
const IDLE_LIMIT: Duration = Duration::from_secs(60);
const WRITE_LIMIT: Duration = Duration::from_secs(30);
const CLOSE_LIMIT: Duration = Duration::from_secs(2);
const MAX_HELD_KEYS: usize = 32;
/// Connecting to the guest (TCP, WebSocket, and RFB handshake).
const CONNECT_LIMIT: Duration = Duration::from_secs(10);
/// One write to a guest connection; a stuck guest ends the stream.
const GUEST_WRITE_LIMIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Watch,
    Control,
}

#[derive(Deserialize)]
pub(super) struct Params {
    mode: Mode,
    #[serde(default)]
    wake: bool,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum ClientMessage {
    Mode { mode: Mode },
    Pointer { x: u16, y: u16, buttons: u8 },
    Key { key: u32, down: bool },
    Release,
}

pub(super) async fn stream(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    params: Result<Query<Params>, QueryRejection>,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    super::computers::require_bot(&state, bot_id, false).await?;
    let Query(params) = params
        .map_err(|_| ApiError::bad_request("invalid_mode", "mode must be watch or control."))?;
    let computers = super::computers::manager(&state)?.clone();
    Ok(ws
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| session(computers, bot_id, params, socket)))
}

/// Why a stream ended, sent to the client before the close.
struct End {
    code: &'static str,
    message: String,
}

fn end(code: &'static str, message: impl Into<String>) -> Option<End> {
    Some(End {
        code,
        message: message.into(),
    })
}

fn error_message(code: &str, message: &str) -> Message {
    Message::Text(
        json!({ "type": "error", "code": code, "message": message })
            .to_string()
            .into(),
    )
}

async fn session(computers: Arc<ComputerManager>, bot: Uuid, params: Params, socket: WebSocket) {
    let (sink, mut incoming) = socket.split();
    let (out, out_rx) = mpsc::channel(16);
    let frame_queued = Arc::new(AtomicBool::new(false));
    let writer = tokio::spawn(write(sink, out_rx, frame_queued.clone()));
    let stream_id = Uuid::new_v4();
    let holder = format!("desktop-stream:{stream_id}");
    let ended = match computers.viewer(bot, &holder, params.wake).await {
        Ok(Ok(target)) => {
            let lease = target.lease_id;
            let mut session = Session::new(
                computers.clone(),
                bot,
                stream_id,
                target,
                out.clone(),
                frame_queued,
            );
            let ended = session.run(params.mode, &mut incoming).await;
            session.shutdown().await;
            if let Err(err) = computers.release(lease).await {
                tracing::warn!(bot = %bot, error = %err, "releasing a desktop viewer lease");
            }
            ended
        }
        Ok(Err(refusal)) => Some(refused(refusal)),
        Err(err) => {
            tracing::warn!(bot = %bot, error = %err, "desktop viewer");
            end("computer_unavailable", "The computer is not available now.")
        }
    };
    if let Some(End { code, message }) = ended {
        let _ = out.send(error_message(code, &message)).await;
    }
    drop(out);
    let _ = tokio::time::timeout(WRITE_LIMIT + CLOSE_LIMIT, writer).await;
}

fn refused(refusal: ViewerRefusal) -> End {
    let (code, message) = match refusal {
        ViewerRefusal::NotRunning => (
            "computer_not_running",
            "The computer is not running; connect with wake=true to start it.".to_owned(),
        ),
        ViewerRefusal::OwnerStopped => (
            "computer_stopped",
            "The owner stopped this computer; resume it first.".to_owned(),
        ),
        ViewerRefusal::BotInactive => ("bot_deleted", "This bot was deleted.".to_owned()),
        ViewerRefusal::NoDesktop => (
            "desktop_unavailable",
            "This computer has no desktop.".to_owned(),
        ),
        ViewerRefusal::Starting => (
            "desktop_starting",
            "The desktop is starting; try again shortly.".to_owned(),
        ),
        ViewerRefusal::Unavailable(reason) => (
            "computer_unavailable",
            format!("The computer is not available now: {reason}"),
        ),
    };
    End { code, message }
}

async fn write(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut out: mpsc::Receiver<Message>,
    frame_queued: Arc<AtomicBool>,
) {
    while let Some(message) = out.recv().await {
        let frame = matches!(message, Message::Binary(_));
        if !matches!(
            tokio::time::timeout(WRITE_LIMIT, sink.send(message)).await,
            Ok(Ok(()))
        ) {
            return;
        }
        if frame {
            frame_queued.store(false, Ordering::Release);
        }
    }
    let _ = tokio::time::timeout(CLOSE_LIMIT, sink.close()).await;
}

struct View {
    target: Target,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    events: mpsc::Receiver<Event>,
    reader: JoinHandle<Result<(), viewer::ViewerError>>,
    bridge: JoinHandle<()>,
}

struct Control {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    drain: JoinHandle<Result<(), viewer::ViewerError>>,
    bridge: JoinHandle<()>,
}

async fn close(
    mut writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    reader: JoinHandle<Result<(), viewer::ViewerError>>,
    bridge: JoinHandle<()>,
) {
    let _ = tokio::time::timeout(GUEST_WRITE_LIMIT, writer.shutdown()).await;
    drop(writer);
    if !bridge.is_finished() {
        let _ = tokio::time::timeout(CLOSE_LIMIT, bridge).await;
    }
    reader.abort();
}

struct Session {
    computers: Arc<ComputerManager>,
    bot: Uuid,
    stream_id: Uuid,
    target: ViewerTarget,
    out: mpsc::Sender<Message>,
    frame_queued: Arc<AtomicBool>,
    mode: Mode,
    /// The `desktop-control:` lease that bot input batches and browser
    /// actions defer to while this stream controls.
    control_lease: Option<Uuid>,
    /// Whether the image has the view-only server; known after the first
    /// watch connection.
    watch_server: Option<bool>,
    view: Option<View>,
    control: Option<Control>,
    fb: Framebuffer,
    sent_size: Option<(u16, u16)>,
    /// Pixels changed since the last encoded frame.
    dirty: bool,
    /// An update request is outstanding.
    requested: bool,
    /// The next request asks for the whole screen.
    full: bool,
    encoding: Option<JoinHandle<Vec<u8>>>,
    held_keys: BTreeSet<u32>,
    buttons: u8,
    pointer: (u16, u16),
}

impl Session {
    fn new(
        computers: Arc<ComputerManager>,
        bot: Uuid,
        stream_id: Uuid,
        target: ViewerTarget,
        out: mpsc::Sender<Message>,
        frame_queued: Arc<AtomicBool>,
    ) -> Self {
        Self {
            computers,
            bot,
            stream_id,
            target,
            out,
            frame_queued,
            mode: Mode::Watch,
            control_lease: None,
            watch_server: None,
            view: None,
            control: None,
            fb: Framebuffer::new(1, 1),
            sent_size: None,
            dirty: false,
            requested: false,
            full: true,
            encoding: None,
            held_keys: BTreeSet::new(),
            buttons: 0,
            pointer: (0, 0),
        }
    }

    async fn send(&self, message: Message) -> Option<End> {
        self.out.send(message).await.err().map(|_| End {
            code: "client_gone",
            message: String::new(),
        })
    }

    async fn send_mode(&self) -> Option<End> {
        self.send(Message::Text(
            json!({ "type": "mode", "mode": self.mode })
                .to_string()
                .into(),
        ))
        .await
    }

    async fn run(
        &mut self,
        mode: Mode,
        incoming: &mut futures_util::stream::SplitStream<WebSocket>,
    ) -> Option<End> {
        if let Some(end) = self.sync_view().await {
            return Some(end);
        }
        if mode == Mode::Control
            && let Some(end) = self.enter_control().await
        {
            return Some(end);
        }
        if self.mode == Mode::Watch && self.watch_server == Some(false) {
            return unsupported();
        }
        if let Some(end) = self.send_mode().await {
            return Some(end);
        }
        let mut frame_tick = tokio::time::interval(FRAME_EVERY);
        frame_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut lease_tick = tokio::time::interval(LEASE_CHECK);
        lease_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let renew_every = self.computers.config().lease_ttl / 3;
        let mut renewed = Instant::now();
        let mut pinged = Instant::now();
        let mut seen = Instant::now();
        loop {
            let ended = tokio::select! {
                message = incoming.next() => {
                    seen = Instant::now();
                    match message {
                        Some(Ok(Message::Text(text))) => self.client(&text).await,
                        Some(Ok(Message::Binary(_))) => {
                            self.send(error_message("invalid_message", "Messages are JSON text.")).await
                        }
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => None,
                        Some(Ok(Message::Close(_)) | Err(_)) | None => end("client_gone", ""),
                    }
                }
                event = next_event(&mut self.view) => match event {
                    Some(event) => self.apply(event).await,
                    None => end("desktop_disconnected", "The desktop connection closed."),
                },
                _ = control_ended(&mut self.control) => {
                    end("desktop_disconnected", "The desktop connection closed.")
                }
                png = encoded(&mut self.encoding) => {
                    match png {
                        Some(png) => {
                            self.frame_queued.store(true, Ordering::Release);
                            if let Err(mpsc::error::TrySendError::Full(_)) =
                                self.out.try_send(Message::Binary(png.into()))
                            {
                                self.frame_queued.store(false, Ordering::Release);
                                self.dirty = true;
                            }
                        }
                        None => self.dirty = true,
                    }
                    None
                }
                _ = frame_tick.tick() => self.frame_tick().await,
                _ = lease_tick.tick() => {
                    if seen.elapsed() > IDLE_LIMIT {
                        end("client_gone", "")
                    } else {
                        let renew = renewed.elapsed() >= renew_every;
                        let checked = match self.computers.viewer_lease(self.target.lease_id, renew).await {
                            Ok(ViewerLease::Live) => {
                                if renew {
                                    renewed = Instant::now();
                                    if let Some(lease) = self.control_lease {
                                        let _ = self.computers.renew(lease).await;
                                    }
                                }
                                None
                            }
                            Ok(ViewerLease::OwnerStopped) => {
                                end("computer_stopped", "The owner stopped this computer.")
                            }
                            Ok(ViewerLease::BotInactive) => end("bot_deleted", "This bot was deleted."),
                            Ok(ViewerLease::NotRunning) => {
                                end("computer_not_running", "The computer stopped running.")
                            }
                            Ok(ViewerLease::Ended) => end("lease_ended", "The desktop session ended."),
                            Err(err) => {
                                tracing::warn!(bot = %self.bot, error = %err, "checking a desktop viewer lease");
                                end("computer_unavailable", "The computer is not available now.")
                            }
                        };
                        if checked.is_none() && pinged.elapsed() >= PING_EVERY {
                            pinged = Instant::now();
                            self.send(Message::Ping(Vec::new().into())).await
                        } else {
                            checked
                        }
                    }
                }
            };
            if ended.is_some() {
                return match self
                    .computers
                    .viewer_lease(self.target.lease_id, false)
                    .await
                {
                    Ok(ViewerLease::OwnerStopped) => {
                        end("computer_stopped", "The owner stopped this computer.")
                    }
                    Ok(ViewerLease::BotInactive) => end("bot_deleted", "This bot was deleted."),
                    Ok(ViewerLease::NotRunning) => {
                        end("computer_not_running", "The computer stopped running.")
                    }
                    _ => ended.filter(|end| end.code != "client_gone"),
                };
            }
        }
    }

    async fn client(&mut self, text: &str) -> Option<End> {
        match self
            .computers
            .viewer_lease(self.target.lease_id, false)
            .await
        {
            Ok(ViewerLease::Live) => {}
            Ok(ViewerLease::OwnerStopped) => {
                return end("computer_stopped", "The owner stopped this computer.");
            }
            Ok(ViewerLease::BotInactive) => return end("bot_deleted", "This bot was deleted."),
            Ok(ViewerLease::NotRunning) => {
                return end("computer_not_running", "The computer stopped running.");
            }
            Ok(ViewerLease::Ended) => return end("lease_ended", "The desktop session ended."),
            Err(_) => return end("computer_unavailable", "The computer is not available now."),
        }
        let Ok(message) = serde_json::from_str::<ClientMessage>(text) else {
            return self
                .send(error_message(
                    "invalid_message",
                    "Unknown or malformed message.",
                ))
                .await;
        };
        match message {
            ClientMessage::Mode {
                mode: Mode::Control,
            } => {
                if self.mode == Mode::Watch
                    && let Some(end) = self.enter_control().await
                {
                    return Some(end);
                }
                self.send_mode().await
            }
            ClientMessage::Mode { mode: Mode::Watch } => {
                if self.mode == Mode::Control {
                    self.leave_control().await;
                    if let Some(end) = self.sync_view().await {
                        return Some(end);
                    }
                    if self.watch_server == Some(false)
                        && let Some(end) = self.send(unsupported_message()).await
                    {
                        return Some(end);
                    }
                }
                self.send_mode().await
            }
            ClientMessage::Pointer { x, y, buttons } => {
                let Some(control) = &mut self.control else {
                    return None;
                };
                let x = x.min(self.fb.width.saturating_sub(1));
                let y = y.min(self.fb.height.saturating_sub(1));
                self.buttons = buttons;
                self.pointer = (x, y);
                input(control, &viewer::pointer_event(buttons, x, y)).await
            }
            ClientMessage::Key { key, down } => {
                let Some(control) = &mut self.control else {
                    return None;
                };
                if down {
                    if self.held_keys.len() >= MAX_HELD_KEYS && !self.held_keys.contains(&key) {
                        return None;
                    }
                    self.held_keys.insert(key);
                } else {
                    self.held_keys.remove(&key);
                }
                input(control, &viewer::key_event(key, down)).await
            }
            ClientMessage::Release => self.release_input().await,
        }
    }

    async fn enter_control(&mut self) -> Option<End> {
        if !self.computers.claim_control(self.bot, self.stream_id).await {
            return self
                .send(error_message(
                    "control_in_use",
                    "Another window controls this desktop.",
                ))
                .await;
        }
        let holder = format!("desktop-control:{}:{}", self.bot, self.stream_id);
        let lease = match self.computers.control_lease(self.bot, &holder).await {
            Ok(Ok(lease)) => lease,
            Ok(Err(refusal)) => {
                self.computers.end_control(self.bot, self.stream_id).await;
                return Some(refused(refusal));
            }
            Err(err) => {
                tracing::warn!(bot = %self.bot, error = %err, "desktop control lease");
                self.computers.end_control(self.bot, self.stream_id).await;
                return self
                    .send(error_message(
                        "control_failed",
                        "Could not take over the desktop now.",
                    ))
                    .await;
            }
        };
        self.control_lease = Some(lease);
        match connect(&self.target, Target::Control).await {
            Ok(connection) => {
                let drain = tokio::spawn(viewer::read_events(
                    connection.reader,
                    connection.width,
                    connection.height,
                    None,
                ));
                self.control = Some(Control {
                    writer: connection.writer,
                    drain,
                    bridge: connection.bridge,
                });
                self.mode = Mode::Control;
                self.sync_view().await
            }
            Err(err) => {
                tracing::warn!(bot = %self.bot, error = %err, "desktop control connection");
                self.end_control_lease().await;
                self.computers.end_control(self.bot, self.stream_id).await;
                self.send(error_message(
                    "control_failed",
                    "Could not take over the desktop now.",
                ))
                .await
            }
        }
    }

    async fn leave_control(&mut self) {
        let _ = self.release_input().await;
        if let Some(control) = self.control.take() {
            close(control.writer, control.drain, control.bridge).await;
        }
        self.held_keys.clear();
        self.buttons = 0;
        self.mode = Mode::Watch;
        self.end_control_lease().await;
        self.computers.end_control(self.bot, self.stream_id).await;
    }

    async fn end_control_lease(&mut self) {
        if let Some(lease) = self.control_lease.take()
            && let Err(err) = self.computers.release(lease).await
        {
            tracing::warn!(bot = %self.bot, error = %err, "releasing a desktop control lease");
        }
    }

    async fn release_input(&mut self) -> Option<End> {
        let Some(control) = &mut self.control else {
            return None;
        };
        let mut bytes = Vec::new();
        for key in std::mem::take(&mut self.held_keys) {
            bytes.extend_from_slice(&viewer::key_event(key, false));
        }
        if self.buttons != 0 {
            let (x, y) = self.pointer;
            bytes.extend_from_slice(&viewer::pointer_event(0, x, y));
            self.buttons = 0;
        }
        if bytes.is_empty() {
            return None;
        }
        input(control, &bytes).await
    }

    async fn sync_view(&mut self) -> Option<End> {
        let wanted = match (self.watch_server, self.mode) {
            (Some(false), Mode::Watch) => None,
            (Some(false), Mode::Control) => Some(Target::Control),
            _ => Some(Target::Watch),
        };
        if self.view.as_ref().map(|v| v.target) == wanted {
            return None;
        }
        if let Some(view) = self.view.take() {
            close(view.writer, view.reader, view.bridge).await;
        }
        let target = wanted?;
        let connection = match connect(&self.target, target).await {
            Ok(connection) => connection,
            Err(err) => {
                tracing::warn!(bot = %self.bot, error = %err, "desktop viewer connection");
                return end("desktop_disconnected", "Could not connect to the desktop.");
            }
        };
        if target == Target::Watch && connection.name != viewer::WATCH_NAME {
            close(
                connection.writer,
                tokio::spawn(async { Ok(()) }),
                connection.bridge,
            )
            .await;
            self.watch_server = Some(false);
            return Box::pin(self.sync_view()).await;
        }
        if target == Target::Watch {
            self.watch_server = Some(true);
        }
        let (tx, events) = mpsc::channel(8);
        let reader = tokio::spawn(viewer::read_events(
            connection.reader,
            connection.width,
            connection.height,
            Some(tx),
        ));
        self.fb = Framebuffer::new(connection.width, connection.height);
        self.view = Some(View {
            target,
            writer: connection.writer,
            events,
            reader,
            bridge: connection.bridge,
        });
        self.requested = false;
        self.full = true;
        if let Some(end) = self.frame_request().await {
            return Some(end);
        }
        self.send_size().await
    }

    async fn send_size(&mut self) -> Option<End> {
        let size = (self.fb.width, self.fb.height);
        if self.sent_size == Some(size) {
            return None;
        }
        self.sent_size = Some(size);
        self.send(Message::Text(
            json!({ "type": "size", "width": size.0, "height": size.1 })
                .to_string()
                .into(),
        ))
        .await
    }

    async fn apply(&mut self, event: Event) -> Option<End> {
        self.fb.apply(&event);
        match event {
            Event::Resize { .. } => {
                if let Some(encoding) = self.encoding.take() {
                    encoding.abort();
                }
                self.full = true;
                self.dirty = true;
                self.send_size().await
            }
            Event::UpdateEnd => {
                self.requested = false;
                None
            }
            _ => {
                self.dirty = true;
                None
            }
        }
    }

    async fn frame_request(&mut self) -> Option<End> {
        let Some(view) = &mut self.view else {
            return None;
        };
        if self.requested {
            return None;
        }
        let request = viewer::update_request(!self.full, self.fb.width, self.fb.height);
        if !write_guest(&mut view.writer, &request).await {
            return end("desktop_disconnected", "The desktop connection closed.");
        }
        self.requested = true;
        self.full = false;
        None
    }

    async fn frame_tick(&mut self) -> Option<End> {
        if self.dirty && self.encoding.is_none() && !self.frame_queued.load(Ordering::Acquire) {
            let snapshot = self.fb.snapshot();
            self.encoding = Some(tokio::task::spawn_blocking(move || snapshot.png()));
            self.dirty = false;
        }
        self.frame_request().await
    }

    async fn shutdown(&mut self) {
        if self.control.is_some() {
            self.leave_control().await;
        }
        if let Some(view) = self.view.take() {
            close(view.writer, view.reader, view.bridge).await;
        }
        if let Some(encoding) = self.encoding.take() {
            encoding.abort();
        }
    }
}

fn unsupported_message() -> Message {
    error_message(
        "desktop_watch_unsupported",
        "Upgrade this computer to else-guest:3 to watch its desktop in the app.",
    )
}

fn unsupported() -> Option<End> {
    end(
        "desktop_watch_unsupported",
        "Upgrade this computer to else-guest:3 to watch its desktop in the app.",
    )
}

async fn input(control: &mut Control, bytes: &[u8]) -> Option<End> {
    if !write_guest(&mut control.writer, bytes).await {
        return end("desktop_disconnected", "The desktop connection closed.");
    }
    None
}

async fn next_event(view: &mut Option<View>) -> Option<Event> {
    match view {
        Some(view) => view.events.recv().await,
        None => std::future::pending().await,
    }
}

async fn control_ended(control: &mut Option<Control>) {
    match control {
        Some(control) => {
            if !control.bridge.is_finished() {
                let _ = (&mut control.bridge).await;
            }
        }
        None => std::future::pending().await,
    }
}

async fn encoded(task: &mut Option<JoinHandle<Vec<u8>>>) -> Option<Vec<u8>> {
    let Some(handle) = task else {
        return std::future::pending().await;
    };
    let png = handle.await.ok();
    *task = None;
    png
}

async fn connect(target: &ViewerTarget, to: Target) -> Result<viewer::Connection, String> {
    match tokio::time::timeout(CONNECT_LIMIT, async {
        loop {
            match viewer::connect(target.port, &target.password, to).await {
                Ok(connection) => return Ok(connection),
                Err(error @ viewer::ViewerError::Protocol(_)) => return Err(error),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err("timed out".into()),
    }
}

async fn write_guest(
    writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    bytes: &[u8],
) -> bool {
    matches!(
        tokio::time::timeout(GUEST_WRITE_LIMIT, writer.write_all(bytes)).await,
        Ok(Ok(()))
    )
}
