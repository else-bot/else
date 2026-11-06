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

/// How the MCP's shared account relates to this card's consent.
#[derive(PartialEq)]
enum Account {
    Disconnected,
    /// Same provider, with at least the scopes the card asked for.
    Covers,
    /// Another provider or fewer scopes: it cannot approve this card.
    Differs,
}

impl Flow {
    fn account(
        &self,
        status: Result<McpConnectionStatus, ClientError>,
    ) -> Result<Account, ClientError> {
        let status = match status {
            Ok(status) => status,
            Err(ClientError::Api { status: 404, .. }) => return Ok(Account::Disconnected),
            Err(error) => return Err(error),
        };
        if status.status != McpState::Connected {
            return Ok(Account::Disconnected);
        }
        if status.provider == self.consent.provider
            && self
                .consent
                .scopes
                .iter()
                .all(|scope| status.scopes.contains(scope))
        {
            Ok(Account::Covers)
        } else {
            Ok(Account::Differs)
        }
    }

    pub async fn recover(&self) -> Result<Resolution, ClientError> {
        let status = self
            .client
            .clone()
            .mcp_connection(self.bot, self.consent.id.clone())
            .await;
        match self.account(status)? {
            Account::Covers => self.confirm().await,
            Account::Disconnected => Ok(Resolution::Disconnected),
            Account::Differs => Err(ClientError::Protocol("The connected account's permissions differ from this card. Ask the bot for a new connection request.".into())),
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
        if self.account(status)? == Account::Covers {
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
