//! A deterministic provider for integration tests and explicitly labeled
//! development demos. It runs no model and touches no files or network.
//!
//! The reply echoes the owner's text in small chunks, including multi-byte
//! characters to exercise UTF-8 handling. Directives in the message change
//! behavior for tests:
//!
//! - `[synthetic:slow]` streams one chunk every 200 ms.
//! - `[synthetic:hang]` acknowledges, then waits until cancelled and takes
//!   200 ms to drain, like a provider confirming an interrupt.
//! - `[synthetic:fail]` acknowledges, then fails.
//! - `[synthetic:ignore-cancel]` acknowledges and never stops on its own.
//! - `[synthetic:stall-send]` marks delivery as started, then waits until
//!   cancelled without acknowledging (a crash here leaves delivery unknown).
//! - `[synthetic:refuse]` fails before delivery (nothing was sent).
//!
//! Like a host adapter it keeps a native session per conversation (resumed
//! while its fingerprint matches, unless the test forgot it), sends the
//! shared packet (missed history, handoff checkpoint, the owner's message),
//! and records every packet so tests can check what a session received.
//! Its checkpoint is deterministic text, or an error when a test asks.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::app::providers::{
    Availability, CheckpointRequest, Profile, Provider, SessionState, TurnInput, TurnOutcome,
    TurnSink,
};

pub const ID: &str = "synthetic";
const FINGERPRINT: &str = "synthetic-v1";

pub struct Synthetic {
    id: &'static str,
    label: &'static str,
    chunk_delay: Duration,
    packets: Arc<Mutex<Vec<String>>>,
    checkpoint_error: Mutex<Option<String>>,
    forgotten: Mutex<Vec<String>>,
}

impl Synthetic {
    pub fn new(chunk_delay: Duration) -> Self {
        Self::named(ID, "Synthetic (test)", chunk_delay)
    }

    /// Another synthetic provider, for switching tests.
    pub fn named(id: &'static str, label: &'static str, chunk_delay: Duration) -> Self {
        Self {
            id,
            label,
            chunk_delay,
            packets: Arc::default(),
            checkpoint_error: Mutex::new(None),
            forgotten: Mutex::new(Vec::new()),
        }
    }

    /// Every packet this provider was sent, oldest first.
    pub fn packets(&self) -> Arc<Mutex<Vec<String>>> {
        self.packets.clone()
    }

    /// Make checkpoints fail with `reason` (or succeed again with `None`).
    pub fn fail_checkpoints(&self, reason: Option<&str>) {
        *self.checkpoint_error.lock().unwrap() = reason.map(str::to_owned);
    }

    /// Lose every native session so far, as if its files were removed.
    pub fn forget_sessions(&self, native_ids: &[String]) {
        self.forgotten.lock().unwrap().extend_from_slice(native_ids);
    }
}

/// Split into chunks of at most `size` characters (never inside a char).
fn chunks(text: &str, size: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    chars.chunks(size).map(|c| c.iter().collect()).collect()
}

pub fn reply_for(input: &TurnInput) -> String {
    format!(
        "Synthetic reply ✓ from {} (turn {}): {}",
        input.bot_name, input.turn_no, input.text
    )
}

#[async_trait]
impl Provider for Synthetic {
    fn id(&self) -> &'static str {
        self.id
    }

    fn label(&self) -> &'static str {
        self.label
    }

    fn availability(&self) -> Availability {
        Availability::Available
    }

    fn uses_profile(&self) -> bool {
        false
    }

    fn validate_profile(&self, _: &Profile) -> Result<(), String> {
        Err("the synthetic provider has no execution profile".to_owned())
    }

    async fn checkpoint(
        &self,
        request: CheckpointRequest,
        _: &CancellationToken,
    ) -> Result<String, String> {
        if let Some(reason) = self.checkpoint_error.lock().unwrap().clone() {
            return Err(reason);
        }
        let lines = request
            .prompt
            .lines()
            .filter(|l| l.starts_with("[turn "))
            .count();
        super::bounded_checkpoint(
            format!(
                "Synthetic checkpoint from {} covering {lines} recorded messages.",
                self.id
            ),
            request.max_bytes,
        )
    }

    async fn run(
        &self,
        input: TurnInput,
        sink: &dyn TurnSink,
        cancel: CancellationToken,
    ) -> TurnOutcome {
        if input.text.contains("[synthetic:refuse]") {
            return TurnOutcome::Failed {
                code: "provider_refused".into(),
                message: "The synthetic provider refused the message.".into(),
            };
        }
        // Resume the recorded session unless it is gone or incompatible.
        let resumed = input.session.as_ref().filter(|s| {
            s.fingerprint == FINGERPRINT && !self.forgotten.lock().unwrap().contains(&s.native_id)
        });
        let mut state = match resumed {
            Some(session) => session.clone(),
            None => SessionState {
                native_id: uuid::Uuid::new_v4().to_string(),
                fingerprint: FINGERPRINT.into(),
                considered_through_turn: 0,
                memory_revision: None,
            },
        };
        if sink.session(&state).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let (packet, selection) =
            super::prompt::packet_with_selection(&input, state.considered_through_turn);
        if sink.packet(&selection, &packet).await.is_err() {
            return TurnOutcome::Interrupted;
        }
        if sink.sending().await.is_err() {
            return TurnOutcome::Interrupted;
        }
        self.packets.lock().unwrap().push(packet);
        if input.text.contains("[synthetic:stall-send]") {
            cancel.cancelled().await;
            return TurnOutcome::Uncertain {
                message: "Stopped before the synthetic provider acknowledged.".into(),
            };
        }
        if sink.running().await.is_err() {
            return TurnOutcome::Interrupted;
        }
        state.considered_through_turn = input.turn_no;
        let _ = sink.session(&state).await;
        if input.text.contains("[synthetic:fail]") {
            let _ = sink.text("Partial output before failure. ").await;
            return TurnOutcome::Failed {
                code: "provider_failed".into(),
                message: "The synthetic provider failed.".into(),
            };
        }
        if input.text.contains("[synthetic:ignore-cancel]") {
            std::future::pending::<()>().await;
        }
        if input.text.contains("[synthetic:hang]") {
            cancel.cancelled().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            return TurnOutcome::Interrupted;
        }
        let delay = if input.text.contains("[synthetic:slow]") {
            Duration::from_millis(200)
        } else {
            self.chunk_delay
        };
        for chunk in chunks(&reply_for(&input), 7) {
            tokio::select! {
                _ = cancel.cancelled() => return TurnOutcome::Interrupted,
                _ = tokio::time::sleep(delay) => {}
            }
            if sink.text(&chunk).await.is_err() {
                return TurnOutcome::Interrupted;
            }
        }
        TurnOutcome::Completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_respect_char_boundaries() {
        let parts = chunks("ab✓cdé", 2);
        assert_eq!(parts, vec!["ab", "✓c", "dé"]);
        assert_eq!(parts.concat(), "ab✓cdé");
    }
}
