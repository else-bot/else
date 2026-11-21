//! Control mode: the claim and lease that make a stream the bot's one
//! controller, and the input-only RFB connection its input travels on.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use axum::extract::ws::Message;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{CLOSE_LIMIT, End, disconnected, error_message, refused, release_abandoned, stopping};
use crate::computers::ComputerManager;
use crate::computers::viewer::{self, Event};

const MAX_HELD_KEYS: usize = 32;
const GUEST_WRITE_LIMIT: Duration = Duration::from_secs(5);

/// Input the stream pressed is released over the link before it closes, so
/// the guest never keeps a key or button down after control ends.
pub(super) struct Controlling {
    pub(super) claim: ControlClaim,
    pub(super) link: Control,
    held_keys: BTreeSet<u32>,
    buttons: u8,
    pointer: (u16, u16),
}

impl Controlling {
    pub(super) fn new(claim: ControlClaim, link: Control) -> Self {
        Self {
            claim,
            link,
            held_keys: BTreeSet::new(),
            buttons: 0,
            pointer: (0, 0),
        }
    }

    pub(super) async fn pointer(&mut self, x: u16, y: u16, buttons: u8) -> Result<(), End> {
        self.buttons = buttons;
        self.pointer = (x, y);
        self.link.input(&viewer::pointer_event(buttons, x, y)).await
    }

    pub(super) async fn key(&mut self, key: u32, down: bool) -> Result<(), End> {
        if down {
            if self.held_keys.len() >= MAX_HELD_KEYS && !self.held_keys.contains(&key) {
                return Ok(());
            }
            self.held_keys.insert(key);
        } else {
            self.held_keys.remove(&key);
        }
        self.link.input(&viewer::key_event(key, down)).await
    }

    pub(super) async fn release_input(&mut self) -> Result<(), End> {
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
            return Ok(());
        }
        self.link.input(&bytes).await
    }

    pub(super) async fn end(mut self, computers: &ComputerManager) {
        let _ = self.release_input().await;
        close(self.link.writer, self.link.reader, self.link.bridge).await;
        self.claim.release(computers).await;
    }
}

/// A bot's one control claim and the lease that makes the bot's input
/// batches defer to the owner. An async drop cannot await, so the holder
/// gives both up with [`ControlClaim::release`].
pub(super) struct ControlClaim {
    bot: Uuid,
    stream: Uuid,
    pub(super) lease: Uuid,
}

impl ControlClaim {
    pub(super) async fn take(
        computers: &ComputerManager,
        db: &sqlx::SqlitePool,
        bot: Uuid,
        stream: Uuid,
        stop: &CancellationToken,
    ) -> Result<Result<Self, Message>, End> {
        let claim = computers.claim_control(bot, stream);
        let Some(claimed) = stop.run_until_cancelled(claim).await else {
            // The actor may still grant the queued claim; this runs after it.
            computers.end_control(bot, stream).await;
            return Err(stopping());
        };
        if !claimed {
            return Ok(Err(error_message(
                "control_in_use",
                "Another window controls this desktop.",
            )));
        }
        let holder = format!("desktop-control:{bot}:{stream}");
        let attach = computers.control_lease(bot, &holder);
        let failed = match stop.run_until_cancelled(attach).await {
            Some(Ok(Ok(lease))) => return Ok(Ok(Self { bot, stream, lease })),
            None => {
                release_abandoned(computers, db, &holder).await;
                Err(stopping())
            }
            Some(Ok(Err(refusal))) => Err(refused(refusal)),
            Some(Err(err)) => {
                tracing::warn!(bot = %bot, error = %err, "desktop control lease");
                Ok(Err(control_failed()))
            }
        };
        computers.end_control(bot, stream).await;
        failed
    }

    pub(super) async fn release(self, computers: &ComputerManager) {
        if let Err(err) = computers.release(self.lease).await {
            tracing::warn!(bot = %self.bot, error = %err, "releasing a desktop control lease");
        }
        computers.end_control(self.bot, self.stream).await;
    }
}

pub(super) fn control_failed() -> Message {
    error_message("control_failed", "Could not take over the desktop now.")
}

/// The input-only connection to the guest's controlling VNC server.
pub(super) struct Control {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    events: mpsc::Receiver<Event>,
    reader: JoinHandle<Result<(), viewer::ViewerError>>,
    bridge: JoinHandle<()>,
    pub(super) connected: Instant,
    pub(super) clipboard_utf8: bool,
}

impl Control {
    pub(super) fn open(connection: viewer::Connection) -> Self {
        let (tx, events) = mpsc::channel(4);
        let reader = tokio::spawn(viewer::read_events(
            connection.reader,
            connection.width,
            connection.height,
            Some(tx),
        ));
        Self {
            writer: connection.writer,
            events,
            reader,
            bridge: connection.bridge,
            connected: Instant::now(),
            clipboard_utf8: false,
        }
    }

    pub(super) async fn input(&mut self, bytes: &[u8]) -> Result<(), End> {
        if !write_guest(&mut self.writer, bytes).await {
            return Err(disconnected());
        }
        Ok(())
    }

    /// The next event the connection announces on its own (it requests no
    /// screen updates), or `None` once the connection closed.
    pub(super) async fn update(&mut self) -> Option<Event> {
        if self.bridge.is_finished() {
            return None;
        }
        tokio::select! {
            Some(event) = self.events.recv() => Some(event),
            _ = &mut self.bridge => None,
        }
    }

    pub(super) async fn wait_for_utf8(&mut self) {
        while let Some(event) = self.events.recv().await {
            if matches!(event, Event::ClipboardUtf8) {
                self.clipboard_utf8 = true;
                return;
            }
        }
        std::future::pending().await
    }
}

pub(super) async fn close(
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

pub(super) async fn write_guest(
    writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    bytes: &[u8],
) -> bool {
    matches!(
        tokio::time::timeout(GUEST_WRITE_LIMIT, writer.write_all(bytes)).await,
        Ok(Ok(()))
    )
}
