//! The sink's side of owner messages steered into its running turn
//! ([`crate::db::steers`]).

use uuid::Uuid;

use super::SinkState;
use crate::app::providers::{Fenced, Steer};

impl SinkState {
    pub(super) async fn steers(&self) -> Vec<Steer> {
        if self.fenced {
            return Vec::new();
        }
        crate::db::steers::pending(&self.pool, self.turn_id)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(turn = %self.turn_id, error = %err, "loading steered messages failed");
                Vec::new()
            })
    }

    pub(super) async fn steer_sending(&self, turn_id: Uuid) -> Result<bool, Fenced> {
        if self.fenced {
            return Err(Fenced);
        }
        crate::db::steers::sending(&self.pool, turn_id, self.turn_id)
            .await
            .map_err(|err| {
                tracing::warn!(turn = %self.turn_id, steered = %turn_id, error = %err, "recording a steered message failed");
                Fenced
            })
    }

    pub(super) async fn steer_refused(&self, turn_id: Uuid) {
        if let Err(err) = crate::db::steers::refuse(&self.pool, turn_id, self.turn_id).await {
            tracing::warn!(turn = %self.turn_id, steered = %turn_id, error = %err, "recording a refused steered message failed");
        }
    }

    /// The provider took a steered message: text after it starts a new
    /// assistant item, so the reply reads in order below the message. If
    /// its delivery cannot be recorded it stays `sending`, so its turn ends
    /// uncertain rather than running again.
    pub(super) async fn steered(&mut self, turn_id: Uuid) -> Result<(), Fenced> {
        self.end_message().await?;
        match crate::db::steers::deliver(&self.pool, turn_id, self.turn_id).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(turn = %self.turn_id, steered = %turn_id, "a steered message was no longer sending");
            }
            Err(err) => {
                tracing::warn!(turn = %self.turn_id, steered = %turn_id, error = %err, "recording a steered message failed");
            }
        }
        Ok(())
    }
}
