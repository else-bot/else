//! One RFB connection to the guest, for viewing or for control input.

use std::time::Duration;

use tokio::io::{AsyncWriteExt, DuplexStream, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::computers::viewer::{self, Event};

pub(super) const CLOSE_LIMIT: Duration = Duration::from_secs(2);
const WRITE_LIMIT: Duration = Duration::from_secs(5);

/// The connection's write half, the events its reader announces, and the
/// bridge that ends when the guest's WebSocket closes.
pub(super) struct GuestLink {
    writer: WriteHalf<DuplexStream>,
    events: mpsc::Receiver<Event>,
    reader: JoinHandle<Result<(), viewer::ViewerError>>,
    bridge: JoinHandle<()>,
}

impl GuestLink {
    /// Start reading `connection`, holding up to `queue` unread events.
    pub(super) fn open(connection: viewer::Connection, queue: usize) -> Self {
        let (tx, events) = mpsc::channel(queue);
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
        }
    }

    /// False when the write failed or timed out.
    pub(super) async fn write(&mut self, bytes: &[u8]) -> bool {
        matches!(
            tokio::time::timeout(WRITE_LIMIT, self.writer.write_all(bytes)).await,
            Ok(Ok(()))
        )
    }

    /// The next event, or `None` once the reader stopped.
    pub(super) async fn event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// The next event, or `None` once the connection closed. Events still
    /// queued are dropped, unless one is ready when the close is seen.
    pub(super) async fn update(&mut self) -> Option<Event> {
        if self.bridge.is_finished() {
            return None;
        }
        tokio::select! {
            Some(event) = self.events.recv() => Some(event),
            _ = &mut self.bridge => None,
        }
    }

    /// Shut the connection, then stop the reader.
    pub(super) async fn close(self) {
        shut(self.writer, self.bridge).await;
        self.reader.abort();
    }
}

/// Close a connection that was never read, without starting a reader.
pub(super) async fn discard(connection: viewer::Connection) {
    shut(connection.writer, connection.bridge).await;
}

/// Shut the write half, then give the bridge up to [`CLOSE_LIMIT`] to
/// finish the close; a bridge still running after that is left to end.
async fn shut(mut writer: WriteHalf<DuplexStream>, bridge: JoinHandle<()>) {
    let _ = tokio::time::timeout(WRITE_LIMIT, writer.shutdown()).await;
    drop(writer);
    if !bridge.is_finished() {
        let _ = tokio::time::timeout(CLOSE_LIMIT, bridge).await;
    }
}
