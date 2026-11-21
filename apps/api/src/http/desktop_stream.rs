//! The owner's in-app desktop viewer: one WebSocket per viewer that carries
//! PNG frames out and, in control mode, pointer and key input in.
//!
//! ```text
//! GET /v1/bots/{id}/desktop/stream?mode=watch|control[&wake=true][&cursor=native]
//! server → {"type":"size","width":W,"height":H}   first, and on every resize
//! server → {"type":"mode","mode":"watch"|"control"}  initial mode and each ack
//! server → {"type":"cursor","style":CSS_NAME}   with cursor=native, in control
//! server → binary PNG of the whole screen, only when it changed
//! server → {"type":"error","code":C,"message":M}   fatal ones close the stream
//! client → {"type":"mode","mode":"watch"|"control"}
//! client → {"type":"pointer","x":X,"y":Y,"buttons":MASK}   RFB button mask
//! client → {"type":"key","key":KEYSYM,"down":BOOL}          X11 keysym
//! client → {"type":"release"}   release every held key and button
//! client → {"type":"clipboard","text":TEXT}   set the guest's clipboard
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
//! a stream ends release what it pressed. In watch mode input is dropped.
//! Clipboard text is input too: in control it becomes the guest's CLIPBOARD
//! and PRIMARY selections before any later key arrives, as UTF-8 where the
//! guest's x11vnc takes ExtendedClipboard and as Latin-1 otherwise. Text too
//! long, or outside Latin-1 for an older guest, is refused with a notice.
//! With `cursor=native`, frames leave out the cursor while the owner controls,
//! and the stream names the guest's cursor shape whenever it changes, so the
//! app shows it as its own native cursor; pointer moves then need no frames.
//! The stream asks for the next screen update as soon as one arrives and
//! encodes a frame once the previous one is on the socket, so the frame rate
//! follows the guest and the owner's connection instead of a timer.
//! API shutdown signals streams with `server_stopping` and waits for input,
//! control, and lease cleanup within the configured shutdown grace period.

mod control;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::rejection::QueryRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::response::Response;
use else_wire::computers::{DesktopMode, ServerEvent};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::{TaskTrackerToken, TaskTrackerWaitFuture};
use uuid::Uuid;

use self::control::{Control, ControlClaim, Controlling, close, control_failed, write_guest};
use super::AppState;
use super::error::{ApiError, ApiPath};
use crate::computers::cursors;
use crate::computers::viewer::{self, Event, Framebuffer, Target};
use crate::computers::{ComputerManager, ViewerLease, ViewerRefusal, ViewerTarget};

/// Room for the longest clipboard text even when JSON escapes all of it.
const MAX_MESSAGE: usize = 8 * viewer::MAX_CLIPBOARD_BYTES;
/// How long a paste outside Latin-1 waits, from taking control, for the guest
/// to offer UTF-8 before it counts as an older guest.
const CLIPBOARD_CAPS_WAIT: Duration = Duration::from_secs(1);
const LEASE_CHECK: Duration = Duration::from_secs(1);
const PING_EVERY: Duration = Duration::from_secs(15);
const IDLE_LIMIT: Duration = Duration::from_secs(60);
const WRITE_LIMIT: Duration = Duration::from_secs(30);
const CLOSE_LIMIT: Duration = Duration::from_secs(2);
const CONNECT_LIMIT: Duration = Duration::from_secs(10);
const STOPPING: &str = "else.bot is stopping; reconnect in a moment.";

#[derive(Default)]
pub(crate) struct Streams {
    stop: CancellationToken,
    tasks: TaskTracker,
}

struct Admission {
    stop: CancellationToken,
    _tracked: TaskTrackerToken,
}

impl Streams {
    fn admit(&self) -> Option<Admission> {
        let tracked = self.tasks.token();
        (!self.tasks.is_closed()).then(|| Admission {
            stop: self.stop.clone(),
            _tracked: tracked,
        })
    }

    pub(crate) fn stop(&self) -> TaskTrackerWaitFuture<'_> {
        self.stop.cancel();
        self.tasks.close();
        self.tasks.wait()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum CursorMode {
    #[default]
    Painted,
    Native,
}

#[derive(Deserialize)]
pub(super) struct Params {
    mode: DesktopMode,
    #[serde(default)]
    wake: bool,
    #[serde(default)]
    cursor: CursorMode,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum ClientMessage {
    Mode { mode: DesktopMode },
    Pointer { x: u16, y: u16, buttons: u8 },
    Key { key: u32, down: bool },
    Release,
    Clipboard { text: String },
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
    let Some(admission) = state.desktop_streams.admit() else {
        return Err(ApiError::unavailable("server_stopping", STOPPING));
    };
    let db = state.db.clone();
    Ok(ws
        .max_message_size(MAX_MESSAGE)
        .on_upgrade(move |socket| session(computers, db, bot_id, params, socket, admission)))
}

struct End {
    code: &'static str,
    message: String,
}

fn end(code: &'static str, message: impl Into<String>) -> End {
    End {
        code,
        message: message.into(),
    }
}

fn stopping() -> End {
    end("server_stopping", STOPPING)
}

fn disconnected() -> End {
    end("desktop_disconnected", "The desktop connection closed.")
}

fn unavailable() -> End {
    end("computer_unavailable", "The computer is not available now.")
}

fn lease_end(lease: ViewerLease) -> Option<End> {
    match lease {
        ViewerLease::Live => None,
        ViewerLease::OwnerStopped => {
            Some(end("computer_stopped", "The owner stopped this computer."))
        }
        ViewerLease::BotInactive => Some(end("bot_deleted", "This bot was deleted.")),
        ViewerLease::NotRunning => {
            Some(end("computer_not_running", "The computer stopped running."))
        }
        ViewerLease::Ended => Some(end("lease_ended", "The desktop session ended.")),
    }
}

fn event_message(event: ServerEvent) -> Message {
    Message::Text(json!(event).to_string().into())
}

fn error_message(code: &str, message: &str) -> Message {
    event_message(ServerEvent::Error {
        code: code.to_owned(),
        message: message.to_owned(),
    })
}

async fn session(
    computers: Arc<ComputerManager>,
    db: sqlx::SqlitePool,
    bot: Uuid,
    params: Params,
    socket: WebSocket,
    admission: Admission,
) {
    let stop = &admission.stop;
    let (sink, mut incoming) = socket.split();
    let (out, out_rx) = mpsc::channel(16);
    let frame_slot = Arc::new(FrameSlot::default());
    let writer = tokio::spawn(write(sink, out_rx, frame_slot.clone()));
    let stream_id = Uuid::new_v4();
    let holder = format!("desktop-stream:{stream_id}");
    let viewer = tokio::select! {
        biased;
        _ = stop.cancelled() => None,
        viewer = computers.viewer(bot, &holder, params.wake) => Some(viewer),
    };
    let ended = match viewer {
        None => {
            release_abandoned(&computers, &db, &holder).await;
            Some(stopping())
        }
        Some(Ok(Ok(target))) => {
            let lease = target.lease_id;
            let mut session = Session::new(
                computers.clone(),
                db.clone(),
                bot,
                stream_id,
                target,
                out.clone(),
                frame_slot,
                params.cursor,
                stop.clone(),
            );
            let Err(ended) = session.run(params.mode, &mut incoming).await;
            session.shutdown().await;
            let ended = session.reason(ended).await;
            if let Err(err) = computers.release(lease).await {
                tracing::warn!(bot = %bot, error = %err, "releasing a desktop viewer lease");
            }
            ended
        }
        Some(Ok(Err(refusal))) => Some(refused(refusal)),
        Some(Err(err)) => {
            tracing::warn!(bot = %bot, error = %err, "desktop viewer");
            Some(unavailable())
        }
    };
    if let Some(End { code, message }) = ended
        && let Err(mpsc::error::TrySendError::Full(message)) =
            out.try_send(error_message(code, &message))
    {
        let _ = stop.run_until_cancelled(out.send(message)).await;
    }
    drop(out);
    let mut writer = writer;
    let flush = tokio::time::timeout(WRITE_LIMIT + CLOSE_LIMIT, &mut writer);
    let stopped = async {
        stop.cancelled().await;
        tokio::time::sleep(CLOSE_LIMIT).await;
    };
    tokio::select! {
        _ = flush => {}
        _ = stopped => {}
    }
    writer.abort();
}

async fn release_abandoned(computers: &ComputerManager, db: &sqlx::SqlitePool, holder: &str) {
    let leases = async {
        // A cancelled SQLite COMMIT may still run on its worker. Take the
        // writer before reading so that commit cannot appear after cleanup.
        let mut tx = db.begin_with("BEGIN IMMEDIATE").await?;
        let leases: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM computer_leases WHERE holder = ?1 AND released_at IS NULL",
        )
        .bind(holder)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(leases)
    }
    .await;
    let leases = match leases {
        Ok(leases) => leases,
        Err(err) => {
            tracing::warn!(error = %err, "finding an abandoned desktop lease");
            return;
        }
    };
    for lease in leases {
        if let Err(err) = computers.release(lease).await {
            tracing::warn!(error = %err, "releasing an abandoned desktop lease");
        }
    }
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

/// At most one frame waits for the socket, and a frame dropped because the
/// queue was full is encoded again only after the writer makes progress.
#[derive(Default)]
struct FrameSlot {
    queued: AtomicBool,
    written: Notify,
}

async fn write(
    mut sink: futures_util::stream::SplitSink<WebSocket, Message>,
    mut out: mpsc::Receiver<Message>,
    slot: Arc<FrameSlot>,
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
            slot.queued.store(false, Ordering::Release);
        }
        slot.written.notify_one();
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

enum Mode {
    Watch,
    Control(Controlling),
}

impl Mode {
    fn wire(&self) -> DesktopMode {
        match self {
            Mode::Watch => DesktopMode::Watch,
            Mode::Control(_) => DesktopMode::Control,
        }
    }
}

struct Session {
    computers: Arc<ComputerManager>,
    db: sqlx::SqlitePool,
    bot: Uuid,
    stream_id: Uuid,
    target: ViewerTarget,
    out: mpsc::Sender<Message>,
    frame_slot: Arc<FrameSlot>,
    cursor: CursorMode,
    cursor_shapes: bool,
    cursor_style: Option<&'static str>,
    cursor_unsent: bool,
    mode: Mode,
    /// Set once the guest's watch server turned out to be an older image's
    /// controlling server, which only control mode may use.
    watch_unsupported: bool,
    view: Option<View>,
    fb: Framebuffer,
    sent_size: Option<(u16, u16)>,
    dirty: bool,
    requested: bool,
    full: bool,
    encoding: Option<JoinHandle<Vec<u8>>>,
    stop: CancellationToken,
}

impl Session {
    #[allow(clippy::too_many_arguments)]
    fn new(
        computers: Arc<ComputerManager>,
        db: sqlx::SqlitePool,
        bot: Uuid,
        stream_id: Uuid,
        target: ViewerTarget,
        out: mpsc::Sender<Message>,
        frame_slot: Arc<FrameSlot>,
        cursor: CursorMode,
        stop: CancellationToken,
    ) -> Self {
        Self {
            computers,
            db,
            bot,
            stream_id,
            target,
            out,
            frame_slot,
            cursor,
            cursor_shapes: false,
            cursor_style: None,
            cursor_unsent: false,
            mode: Mode::Watch,
            watch_unsupported: false,
            view: None,
            fb: Framebuffer::new(1, 1),
            sent_size: None,
            dirty: false,
            requested: false,
            full: true,
            encoding: None,
            stop,
        }
    }

    async fn send(&self, message: Message) -> Result<(), End> {
        match self.stop.run_until_cancelled(self.out.send(message)).await {
            Some(Ok(())) => Ok(()),
            Some(Err(_)) => Err(end("client_gone", "")),
            None => Err(stopping()),
        }
    }

    async fn lease_state(&self, renew: bool) -> Option<Result<ViewerLease, String>> {
        self.stop
            .run_until_cancelled(self.computers.viewer_lease(self.target.lease_id, renew))
            .await
            .map(|state| state.map_err(|err| err.to_string()))
    }

    async fn check_lease(&self, renew: bool) -> Result<(), End> {
        match self.lease_state(renew).await {
            None => Err(stopping()),
            Some(Ok(lease)) => lease_end(lease).map_or(Ok(()), Err),
            Some(Err(err)) => {
                tracing::warn!(bot = %self.bot, error = %err, "checking a desktop viewer lease");
                Err(unavailable())
            }
        }
    }

    async fn reason(&self, ended: End) -> Option<End> {
        if self.stop.is_cancelled() {
            return Some(stopping());
        }
        match self.lease_state(false).await {
            None => Some(stopping()),
            Some(Ok(
                lease @ (ViewerLease::OwnerStopped
                | ViewerLease::BotInactive
                | ViewerLease::NotRunning),
            )) => lease_end(lease),
            _ => Some(ended).filter(|end| end.code != "client_gone"),
        }
    }

    async fn send_mode(&self) -> Result<(), End> {
        self.send(event_message(ServerEvent::Mode {
            mode: self.mode.wire(),
        }))
        .await
    }

    async fn run(
        &mut self,
        mode: DesktopMode,
        incoming: &mut futures_util::stream::SplitStream<WebSocket>,
    ) -> Result<Infallible, End> {
        self.sync_view().await?;
        if mode == DesktopMode::Control {
            self.enter_control().await?;
        }
        if matches!(self.mode, Mode::Watch) && self.watch_unsupported {
            return Err(unsupported());
        }
        self.send_mode().await?;
        let mut lease_tick = tokio::time::interval(LEASE_CHECK);
        lease_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let renew_every = self.computers.config().lease_ttl / 3;
        let mut renewed = Instant::now();
        let mut pinged = Instant::now();
        let mut seen = Instant::now();
        let stop = self.stop.clone();
        let frame_slot = self.frame_slot.clone();
        loop {
            tokio::select! {
                _ = stop.cancelled() => return Err(stopping()),
                message = incoming.next() => {
                    seen = Instant::now();
                    match message {
                        Some(Ok(Message::Text(text))) => self.client(&text).await?,
                        Some(Ok(Message::Binary(_))) => {
                            self.send(error_message("invalid_message", "Messages are JSON text.")).await?;
                        }
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                        Some(Ok(Message::Close(_)) | Err(_)) | None => return Err(end("client_gone", "")),
                    }
                }
                event = next_event(&mut self.view) => match event {
                    Some(event) => self.apply(event).await?,
                    None => return Err(disconnected()),
                },
                update = control_update(&mut self.mode) => match update {
                    Some(Event::ClipboardUtf8) => {
                        if let Mode::Control(controlling) = &mut self.mode {
                            controlling.link.clipboard_utf8 = true;
                        }
                    }
                    Some(_) => {}
                    None => return Err(disconnected()),
                },
                png = encoded(&mut self.encoding) => {
                    match png {
                        Some(png) => {
                            self.frame_slot.queued.store(true, Ordering::Release);
                            if let Err(mpsc::error::TrySendError::Full(_)) =
                                self.out.try_send(Message::Binary(png.into()))
                            {
                                self.frame_slot.queued.store(false, Ordering::Release);
                                self.dirty = true;
                            }
                        }
                        None => self.dirty = true,
                    }
                }
                _ = frame_slot.written.notified() => {
                    self.send_cursor();
                    self.encode();
                }
                _ = lease_tick.tick() => {
                    if seen.elapsed() > IDLE_LIMIT {
                        return Err(end("client_gone", ""));
                    }
                    let renew = renewed.elapsed() >= renew_every;
                    self.check_lease(renew).await?;
                    if renew {
                        renewed = Instant::now();
                        if let Mode::Control(controlling) = &self.mode {
                            let renewal = self.computers.renew(controlling.claim.lease);
                            let _ = self.stop.run_until_cancelled(renewal).await;
                        }
                    }
                    if pinged.elapsed() >= PING_EVERY {
                        pinged = Instant::now();
                        self.send(Message::Ping(Vec::new().into())).await?;
                    }
                }
            }
        }
    }

    async fn client(&mut self, text: &str) -> Result<(), End> {
        self.check_lease(false).await?;
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
                mode: DesktopMode::Control,
            } => {
                if matches!(self.mode, Mode::Watch) {
                    self.enter_control().await?;
                }
                self.send_mode().await
            }
            ClientMessage::Mode {
                mode: DesktopMode::Watch,
            } => {
                if matches!(self.mode, Mode::Control(_)) {
                    self.leave_control().await;
                    self.sync_view().await?;
                    self.sync_cursor().await?;
                    if self.watch_unsupported {
                        self.send(unsupported_message()).await?;
                    }
                }
                self.send_mode().await
            }
            ClientMessage::Pointer { x, y, buttons } => {
                let Mode::Control(controlling) = &mut self.mode else {
                    return Ok(());
                };
                let x = x.min(self.fb.width.saturating_sub(1));
                let y = y.min(self.fb.height.saturating_sub(1));
                controlling.pointer(x, y, buttons).await
            }
            ClientMessage::Key { key, down } => {
                let Mode::Control(controlling) = &mut self.mode else {
                    return Ok(());
                };
                controlling.key(key, down).await
            }
            ClientMessage::Release => {
                let Mode::Control(controlling) = &mut self.mode else {
                    return Ok(());
                };
                controlling.release_input().await
            }
            ClientMessage::Clipboard { text } => self.paste(&text).await,
        }
    }

    async fn paste(&mut self, text: &str) -> Result<(), End> {
        let Mode::Control(controlling) = &mut self.mode else {
            return Ok(());
        };
        let text = text.replace("\r\n", "\n");
        if text.len() > viewer::MAX_CLIPBOARD_BYTES {
            return self
                .send(error_message(
                    "clipboard_too_large",
                    "The clipboard holds more text than the desktop takes (256 KB).",
                ))
                .await;
        }
        let link = &mut controlling.link;
        let latin1 = viewer::clipboard_latin1(&text);
        if latin1.is_none() && !link.clipboard_utf8 {
            let waiting = tokio::time::timeout_at(
                (link.connected + CLIPBOARD_CAPS_WAIT).into(),
                link.wait_for_utf8(),
            );
            if self.stop.run_until_cancelled(waiting).await.is_none() {
                return Err(stopping());
            }
        }
        let message = if link.clipboard_utf8 {
            viewer::clipboard_utf8(&text)
        } else if let Some(latin1) = latin1 {
            latin1
        } else {
            return self
                .send(error_message(
                    "clipboard_unsupported",
                    "This text has characters this computer cannot paste. Upgrade the computer to paste it.",
                ))
                .await;
        };
        link.input(&message).await
    }

    async fn enter_control(&mut self) -> Result<(), End> {
        let taken = ControlClaim::take(
            &self.computers,
            &self.db,
            self.bot,
            self.stream_id,
            &self.stop,
        )
        .await?;
        let claim = match taken {
            Ok(claim) => claim,
            Err(notice) => return self.send(notice).await,
        };
        let link = match connect(&self.target, Target::Control, &self.stop).await {
            Ok(connection) => Control::open(connection),
            Err(err) => {
                tracing::warn!(bot = %self.bot, error = %err, "desktop control connection");
                claim.release(&self.computers).await;
                return self.send(control_failed()).await;
            }
        };
        self.mode = Mode::Control(Controlling::new(claim, link));
        self.sync_view().await?;
        self.sync_cursor().await
    }

    async fn leave_control(&mut self) {
        if let Mode::Control(controlling) = std::mem::replace(&mut self.mode, Mode::Watch) {
            controlling.end(&self.computers).await;
        }
    }

    async fn sync_view(&mut self) -> Result<(), End> {
        let wanted = match (self.watch_unsupported, &self.mode) {
            (true, Mode::Watch) => None,
            (true, Mode::Control(_)) => Some(Target::Control),
            (false, _) => Some(Target::Watch),
        };
        if self.view.as_ref().map(|v| v.target) == wanted {
            return Ok(());
        }
        if let Some(view) = self.view.take() {
            close(view.writer, view.reader, view.bridge).await;
        }
        let Some(target) = wanted else {
            return Ok(());
        };
        let connection = match connect(&self.target, target, &self.stop).await {
            Ok(connection) => connection,
            Err(err) => {
                tracing::warn!(bot = %self.bot, error = %err, "desktop viewer connection");
                return Err(end(
                    "desktop_disconnected",
                    "Could not connect to the desktop.",
                ));
            }
        };
        if target == Target::Watch && connection.name != viewer::WATCH_NAME {
            close(
                connection.writer,
                tokio::spawn(async { Ok(()) }),
                connection.bridge,
            )
            .await;
            self.watch_unsupported = true;
            return Box::pin(self.sync_view()).await;
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
        self.cursor_shapes = false;
        self.cursor_style = None;
        self.cursor_unsent = false;
        self.sync_cursor().await?;
        self.frame_request().await?;
        self.send_size().await
    }

    /// Asks the view for cursor shapes exactly while a native-cursor owner
    /// controls; otherwise the server paints the cursor into the frames.
    async fn sync_cursor(&mut self) -> Result<(), End> {
        let wanted = self.cursor == CursorMode::Native && matches!(self.mode, Mode::Control(_));
        let Some(view) = &mut self.view else {
            return Ok(());
        };
        if wanted == self.cursor_shapes {
            return Ok(());
        }
        let mut encodings = viewer::set_encodings(wanted, view.target == Target::Control);
        if !wanted {
            // The server repaints the cursor into the pixels only where the
            // screen changes next, so refresh all of it now.
            encodings.extend_from_slice(&viewer::update_request(
                false,
                self.fb.width,
                self.fb.height,
            ));
        }
        let written = write_guest(&mut view.writer, &encodings);
        match self.stop.run_until_cancelled(written).await {
            None => return Err(stopping()),
            Some(false) => return Err(disconnected()),
            Some(true) => {}
        }
        self.cursor_shapes = wanted;
        self.cursor_style = None;
        self.cursor_unsent = false;
        Ok(())
    }

    /// Sends the latest cursor style without waiting on a busy socket; the
    /// writer's next progress retries it, and newer styles replace it.
    fn send_cursor(&mut self) {
        let Some(style) = self.cursor_style.filter(|_| self.cursor_unsent) else {
            return;
        };
        let message = event_message(ServerEvent::Cursor {
            style: style.to_owned(),
        });
        if !matches!(
            self.out.try_send(message),
            Err(mpsc::error::TrySendError::Full(_))
        ) {
            self.cursor_unsent = false;
        }
    }

    async fn send_size(&mut self) -> Result<(), End> {
        let size = (self.fb.width, self.fb.height);
        if self.sent_size == Some(size) {
            return Ok(());
        }
        self.sent_size = Some(size);
        self.send(event_message(ServerEvent::Size {
            width: size.0,
            height: size.1,
        }))
        .await
    }

    async fn apply(&mut self, event: Event) -> Result<(), End> {
        self.fb.apply(&event);
        match event {
            Event::Cursor {
                hotspot_x,
                hotspot_y,
                width,
                height,
                mask,
            } => {
                let style = cursors::style((hotspot_x, hotspot_y), (width, height), &mask);
                if !self.cursor_shapes || self.cursor_style == Some(style) {
                    return Ok(());
                }
                self.cursor_style = Some(style);
                self.cursor_unsent = true;
                self.send_cursor();
                Ok(())
            }
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
                self.encode();
                self.frame_request().await
            }
            _ => {
                self.dirty = true;
                Ok(())
            }
        }
    }

    async fn frame_request(&mut self) -> Result<(), End> {
        let Some(view) = &mut self.view else {
            return Ok(());
        };
        if self.requested {
            return Ok(());
        }
        let request = viewer::update_request(!self.full, self.fb.width, self.fb.height);
        let written = write_guest(&mut view.writer, &request);
        match self.stop.run_until_cancelled(written).await {
            None => return Err(stopping()),
            Some(false) => return Err(disconnected()),
            Some(true) => {}
        }
        self.requested = true;
        self.full = false;
        Ok(())
    }

    fn encode(&mut self) {
        if self.dirty && self.encoding.is_none() && !self.frame_slot.queued.load(Ordering::Acquire)
        {
            let snapshot = self.fb.snapshot();
            self.encoding = Some(tokio::task::spawn_blocking(move || snapshot.png()));
            self.dirty = false;
        }
    }

    async fn shutdown(&mut self) {
        self.leave_control().await;
        if let Some(view) = self.view.take() {
            close(view.writer, view.reader, view.bridge).await;
        }
        if let Some(encoding) = self.encoding.take() {
            encoding.abort();
        }
    }
}

const UNSUPPORTED: &str = "Upgrade this computer to else-guest:3 to watch its desktop in the app.";

fn unsupported_message() -> Message {
    error_message("desktop_watch_unsupported", UNSUPPORTED)
}

fn unsupported() -> End {
    end("desktop_watch_unsupported", UNSUPPORTED)
}

async fn next_event(view: &mut Option<View>) -> Option<Event> {
    match view {
        Some(view) => view.events.recv().await,
        None => std::future::pending().await,
    }
}

async fn control_update(mode: &mut Mode) -> Option<Event> {
    match mode {
        Mode::Control(controlling) => controlling.link.update().await,
        Mode::Watch => std::future::pending().await,
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

async fn connect(
    target: &ViewerTarget,
    to: Target,
    stop: &CancellationToken,
) -> Result<viewer::Connection, String> {
    let connecting = tokio::time::timeout(CONNECT_LIMIT, async {
        loop {
            match viewer::connect(target.port, &target.password, to).await {
                Ok(connection) => return Ok(connection),
                Err(error @ viewer::ViewerError::Protocol(_)) => return Err(error),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    });
    let connected = tokio::select! {
        biased;
        _ = stop.cancelled() => return Err("the API is stopping".into()),
        connected = connecting => connected,
    };
    match connected {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_) => Err("timed out".into()),
    }
}
