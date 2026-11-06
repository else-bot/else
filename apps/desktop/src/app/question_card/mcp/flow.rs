use std::sync::Arc;

use uuid::Uuid;

use crate::api::{Client, ClientError, McpConnectionStatus, McpConsent, McpState};
use crate::model::{APPROVE, DENY, QuestionAnswer};

#[derive(Clone)]
pub(super) struct Flow {
    pub client: Arc<Client>,
    pub bot: Uuid,
    pub question: Uuid,
    pub consent: McpConsent,
}

#[derive(Debug, PartialEq)]
pub(super) enum Resolution {
    Disconnected,
    ConfirmationRequired(String),
    Recorded,
}

impl Flow {
    fn connected(
        &self,
        status: Result<McpConnectionStatus, ClientError>,
    ) -> Result<bool, ClientError> {
        let status = match status {
            Ok(status) => status,
            Err(ClientError::Api { status: 404, .. }) => return Ok(false),
            Err(error) => return Err(error),
        };
        if status.status != McpState::Connected {
            return Ok(false);
        }
        if status.provider != self.consent.provider || status.scopes != self.consent.scopes {
            return Err(ClientError::Protocol("The connected account's permissions differ from this card. Ask the bot for a new connection request.".into()));
        }
        Ok(true)
    }

    pub async fn recover(&self) -> Result<Resolution, ClientError> {
        let status = self
            .client
            .clone()
            .mcp_connection(self.bot, self.consent.id.clone())
            .await;
        if self.connected(status)? {
            self.confirm().await
        } else {
            Ok(Resolution::Disconnected)
        }
    }

    pub async fn confirm(&self) -> Result<Resolution, ClientError> {
        match self
            .client
            .clone()
            .answer_question(
                self.bot,
                self.question,
                QuestionAnswer::Single(APPROVE.into()),
            )
            .await
        {
            Ok(()) => Ok(Resolution::Recorded),
            Err(ClientError::Unauthorized) => Err(ClientError::Unauthorized),
            Err(error) => Ok(Resolution::ConfirmationRequired(error.to_string())),
        }
    }

    pub async fn cancel(&self) -> Result<Resolution, ClientError> {
        let status = self
            .client
            .clone()
            .cancel_pending_mcp(self.bot, self.consent.id.clone())
            .await;
        if self.connected(status)? {
            return Ok(Resolution::ConfirmationRequired("Sign-in already completed. Finish confirming the connection to resume this request.".into()));
        }
        self.client
            .clone()
            .answer_question(self.bot, self.question, QuestionAnswer::Single(DENY.into()))
            .await?;
        Ok(Resolution::Recorded)
    }
}

#[cfg(test)]
mod tests;
