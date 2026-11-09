use std::sync::Arc;

use uuid::Uuid;

use crate::api::{Client, ClientError, EmailConsent, McpConnectionStatus, McpConsent, McpState};
use crate::email::oauth;
use crate::model::{APPROVE, DENY, Question, QuestionAnswer};

#[derive(Clone)]
pub(super) struct Flow {
    pub client: Arc<Client>,
    pub bot: Uuid,
    pub question: Uuid,
    pub consent: Consent,
}

/// The account a connection card signs in to.
#[derive(Clone)]
pub(super) enum Consent {
    Mcp(McpConsent),
    Email(EmailConsent),
}

impl Consent {
    pub fn of(question: &Question) -> Option<Self> {
        question
            .mcp_connection()
            .cloned()
            .map(Self::Mcp)
            .or_else(|| question.email_connection().cloned().map(Self::Email))
    }
}

/// A browser sign-in in progress for one card.
pub(super) enum SignIn {
    Mcp(crate::mcp_oauth::SignIn),
    Email(oauth::SignIn),
}

fn withdrawn() -> ClientError {
    ClientError::Protocol("The connection request was withdrawn.".into())
}

impl SignIn {
    pub fn url(&self) -> &str {
        match self {
            Self::Mcp(signin) => &signin.authorization.authorization_url,
            Self::Email(signin) => &signin.authorization.authorization_url,
        }
    }

    /// Waits for the browser callback, then redeems it unless `exchanging`
    /// reports that the card is no longer pending.
    pub async fn complete(&mut self, exchanging: impl FnOnce() -> bool) -> Result<(), ClientError> {
        match self {
            Self::Mcp(signin) => {
                let callback = signin.wait().await?;
                if !exchanging() {
                    return Err(withdrawn());
                }
                signin.finish(callback).await
            }
            Self::Email(signin) => {
                let callback = signin.wait().await.map_err(ClientError::Protocol)?;
                if !exchanging() {
                    return Err(withdrawn());
                }
                signin.finish(callback).await.map(|_| ())
            }
        }
    }
}

#[derive(Debug, PartialEq)]
pub(super) enum Resolution {
    Disconnected,
    ConfirmationRequired(String),
    Recorded,
    /// The card's own cancel, recorded by the server.
    Cancelled,
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
    pub async fn sign_in(&self) -> Result<SignIn, ClientError> {
        match &self.consent {
            Consent::Mcp(consent) => {
                crate::mcp_oauth::SignIn::start(self.client.clone(), self.bot, consent.clone())
                    .await
                    .map(SignIn::Mcp)
            }
            Consent::Email(consent) => {
                let provider = oauth::Provider::for_email(consent.provider).ok_or_else(|| {
                    ClientError::Protocol(format!(
                        "{} needs an app password. Add it in Settings, Email.",
                        consent.provider.label()
                    ))
                })?;
                oauth::SignIn::start(self.client.clone(), provider, consent.address.clone())
                    .await
                    .map(SignIn::Email)
            }
        }
    }

    pub fn ready_label(&self) -> &'static str {
        match &self.consent {
            Consent::Mcp(_) => "Connect in browser",
            Consent::Email(consent) => match oauth::Provider::for_email(consent.provider) {
                Some(oauth::Provider::Microsoft) => "Sign in with Microsoft",
                _ => "Sign in with Google",
            },
        }
    }

    fn account(
        consent: &McpConsent,
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
        if status.provider == consent.provider
            && consent
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
        let consent = match &self.consent {
            Consent::Mcp(consent) => consent,
            // The API approves an inbox card only once an account is connected.
            Consent::Email(_) => return self.confirm().await,
        };
        let status = self
            .client
            .clone()
            .mcp_connection(self.bot, consent.id.clone())
            .await;
        match Self::account(consent, status)? {
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
            Err(error) if error.code() == Some("inbox_not_connected") => {
                Ok(Resolution::Disconnected)
            }
            Err(error) => Ok(Resolution::ConfirmationRequired(error.to_string())),
        }
    }

    pub async fn cancel(&self) -> Result<Resolution, ClientError> {
        if let Consent::Mcp(consent) = &self.consent {
            let status = self
                .client
                .clone()
                .cancel_pending_mcp(self.bot, consent.id.clone())
                .await;
            if Self::account(consent, status)? == Account::Covers {
                return Ok(Resolution::ConfirmationRequired("Sign-in already completed. Finish confirming the connection to resume this request.".into()));
            }
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
