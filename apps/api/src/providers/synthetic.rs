//! A deterministic provider for integration tests and explicitly labeled
//! development demos. It runs no model and touches no files or network.
//!
//! The reply echoes the owner's text in small chunks, including multi-byte
//! characters to exercise UTF-8 handling. Directives in the message change
//! behavior for tests, from the request or any owner message that joined it:
//!
//! - `[synthetic:hang]` acknowledges, then waits until cancelled and takes
//!   200 ms to drain, like a provider confirming an interrupt.
//! - `[synthetic:fail]` acknowledges, then fails.
//! - `[synthetic:ignore-cancel]` acknowledges and stops only when the stop escalates.
//! - `[synthetic:stall-send]` marks delivery as started, then waits until
//!   cancelled without acknowledging (a crash here leaves delivery unknown).
//! - `[synthetic:refuse]` fails before delivery (nothing was sent).
//! - `[synthetic:policy]` fails before delivery with a policy violation.
//! - `[synthetic:messages]` follows the reply with a second message.
//!
//! Like a host adapter it keeps a native session per conversation (resumed
//! while its fingerprint matches), prepares the turn's packet through the
//! same session code without saved memory, and records every packet so
//! tests can check what a session received. Its checkpoint is deterministic
//! text.

use std::collections::VecDeque;
use std::time::Duration;

use crate::actor::Actor;
use crate::app::providers::FailureCode;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::cli::session::{Opened, PreparedTurn};
use crate::app::providers::{
    Availability, CheckpointRequest, Profile, Provider, Stop, TurnInput, TurnOutcome, TurnSink,
};

pub const ID: &str = "synthetic";
const FINGERPRINT: &str = "synthetic-v1";
pub const SECOND_MESSAGE: &str = "Second message.";

pub struct Synthetic {
    id: &'static str,
    label: &'static str,
    chunk_delay: Duration,
    packets: Actor<VecDeque<String>>,
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
            packets: Actor::default(),
        }
    }

    /// The last 256 packets this provider was sent, oldest first.
    pub async fn packets(&self) -> Vec<String> {
        self.packets
            .call(|packets| packets.iter().cloned().collect())
            .await
    }
}

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

    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome {
        let cancel = stop.token();
        let directives = std::iter::once(input.text.as_str())
            .chain(input.steered.iter().map(|s| s.message.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        if directives.contains("[synthetic:policy]") {
            return TurnOutcome::Failed {
                code: FailureCode::PolicyViolation,
                message: "The synthetic provider attempted a forbidden tool.".into(),
            };
        }
        if directives.contains("[synthetic:refuse]") {
            return TurnOutcome::Failed {
                code: FailureCode::ProviderRefused,
                message: "The synthetic provider refused the message.".into(),
            };
        }
        let resumed = input
            .session
            .clone()
            .filter(|s| s.fingerprint == FINGERPRINT);
        // Tests assert packets without saved memory.
        let input = TurnInput {
            memory: None,
            ..input
        };
        let opened = match &resumed {
            Some(session) => Opened::Resumed(session),
            None => Opened::Fresh(uuid::Uuid::new_v4().to_string()),
        };
        let mut turn = PreparedTurn::new(&input, FINGERPRINT.into(), opened);
        if turn.record(sink).await.is_err() || sink.sending().await.is_err() {
            return TurnOutcome::Interrupted;
        }
        let packet = turn.packet.clone();
        self.packets
            .call(move |packets| {
                if packets.len() == 256 {
                    packets.pop_front();
                }
                packets.push_back(packet);
            })
            .await;
        if directives.contains("[synthetic:stall-send]") {
            cancel.cancelled().await;
            return TurnOutcome::Uncertain {
                message: "Stopped before the synthetic provider acknowledged.".into(),
            };
        }
        if turn
            .accept(sink, &serde_json::Value::Null, "")
            .await
            .is_err()
        {
            return TurnOutcome::Interrupted;
        }
        if directives.contains("[synthetic:fail]") {
            let _ = sink.text("Partial output before failure. ").await;
            return TurnOutcome::Failed {
                code: FailureCode::ProviderFailed,
                message: "The synthetic provider failed.".into(),
            };
        }
        if directives.contains("[synthetic:ignore-cancel]") {
            stop.escalated().await;
            return TurnOutcome::Uncertain {
                message: "The synthetic provider did not stop after being interrupted.".into(),
            };
        }
        if directives.contains("[synthetic:hang]") {
            cancel.cancelled().await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            return TurnOutcome::Interrupted;
        }
        let delay = self.chunk_delay;
        for chunk in chunks(&reply_for(&input), 7) {
            tokio::select! {
                _ = cancel.cancelled() => return TurnOutcome::Interrupted,
                _ = tokio::time::sleep(delay) => {}
            }
            if sink.text(&chunk).await.is_err() {
                return TurnOutcome::Interrupted;
            }
        }
        if directives.contains("[synthetic:usage]") {
            for output in [10, 20] {
                sink.usage(crate::app::providers::Usage {
                    input: 1000,
                    cached_input: 900,
                    cache_write: 50,
                    output,
                })
                .await;
            }
            let resets_at = chrono::Utc::now() + chrono::Duration::hours(2);
            sink.limits(vec![crate::app::providers::LimitWindow {
                minutes: crate::app::providers::LimitWindow::FIVE_HOURS,
                used_basis_points: 1200,
                resets_at,
            }])
            .await;
        }
        if directives.contains("[synthetic:messages]")
            && (sink.end_message().await.is_err() || sink.text(SECOND_MESSAGE).await.is_err())
        {
            return TurnOutcome::Interrupted;
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
