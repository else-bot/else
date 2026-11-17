//! How an attempt gives the provider the owner messages that joined its
//! turn while it runs.

use crate::app::providers::{Fenced, Stop, TurnSink};

use super::{Channel, Protocol};

/// Why [`Channel::send`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    /// The provider answered that it took nothing.
    Refused(String),
    /// Cut off, timed out, or the transport closed: it may have taken it.
    Unknown(String),
}

/// Whether the attempt still gives the provider joined messages. It closes
/// after a refusal, an unknown delivery or a failed staging, so a later
/// message never overtakes one the provider may not have.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum Steering {
    #[default]
    Open,
    Closed,
}

impl Steering {
    /// Give the provider each owner message joined to the turn and not
    /// given yet, oldest first, staging its files and recording it as
    /// sending first. A stop request ends the batch at once, even
    /// mid-write. A message the provider refused waits for the reply's next
    /// attempt; one whose write failed or was cut off keeps an unknown
    /// delivery, so its turn ends uncertain and is never resent.
    pub(super) async fn give<C, P>(
        &mut self,
        channel: &mut C,
        protocol: &mut P,
        sink: &dyn TurnSink,
        stop: &Stop,
    ) where
        C: Channel,
        P: Protocol<Message = C::Message>,
    {
        if *self == Self::Closed {
            return;
        }
        for steer in sink.steers().await {
            let Some(input) = protocol.steer_input(&steer) else {
                return;
            };
            if stop.is_requested() {
                return;
            }
            if !steer.files.is_empty() {
                let Some(project) = channel.project() else {
                    return;
                };
                if let Err(err) = crate::providers::attachments::stage(project, &steer.files).await
                {
                    tracing::warn!(steered = %steer.turn_id, error = %err, "staging a steered message's files failed");
                    *self = Self::Closed;
                    return;
                }
            }
            match sink.steer_sending(steer.turn_id).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(Fenced) => return,
            }
            let sent = tokio::select! {
                sent = channel.send(&input) => sent,
                () = stop.requested() => Err(SendError::Unknown("stopped while writing".into())),
            };
            match sent {
                Ok(()) => protocol.steer_sent(&steer),
                Err(SendError::Refused(why)) => {
                    tracing::info!(steered = %steer.turn_id, %why, "the provider refused a steered message");
                    sink.steer_refused(steer.turn_id).await;
                    *self = Self::Closed;
                    return;
                }
                Err(SendError::Unknown(why)) => {
                    tracing::warn!(steered = %steer.turn_id, %why, "writing a steered message failed");
                    *self = Self::Closed;
                    return;
                }
            }
        }
    }
}
