//! A deterministic provider for integration tests and explicitly labeled
//! development demos. It runs no model and touches no files or network.
//!
//! The reply echoes the owner's text in small chunks, including multi-byte
//! characters to exercise UTF-8 handling. Directives in the message change
//! behavior for tests:
//!
//! - `[synthetic:slow]` streams one chunk every 200 ms.
//! - `[synthetic:hang]` acknowledges, then waits until cancelled.
//! - `[synthetic:fail]` acknowledges, then fails.
//! - `[synthetic:refuse]` fails before delivery (nothing was sent).

use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::app::providers::{Availability, Profile, Provider, TurnInput, TurnOutcome, TurnSink};

pub const ID: &str = "synthetic";

pub struct Synthetic {
    chunk_delay: Duration,
}

impl Synthetic {
    pub fn new(chunk_delay: Duration) -> Self {
        Self { chunk_delay }
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
        ID
    }

    fn label(&self) -> &'static str {
        "Synthetic (test)"
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
        if sink.sending().await.is_err() || sink.running().await.is_err() {
            return TurnOutcome::Interrupted;
        }
        if input.text.contains("[synthetic:fail]") {
            let _ = sink.text("Partial output before failure. ").await;
            return TurnOutcome::Failed {
                code: "provider_failed".into(),
                message: "The synthetic provider failed.".into(),
            };
        }
        if input.text.contains("[synthetic:hang]") {
            cancel.cancelled().await;
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
