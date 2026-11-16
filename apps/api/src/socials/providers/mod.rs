pub mod discord;
pub mod telegram;
pub mod whatsapp;

use crate::socials::types::*;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub enum Session {
    Telegram(teloxide_core::Bot),
    Discord {
        http: std::sync::Arc<twilight_http::Client>,
        token: Token,
    },
    WhatsApp(whatsapp::Link),
}
impl Session {
    /// The SDK session a bot token authenticates.
    pub fn token(provider: Provider, token: Token) -> Result<Self, Error> {
        match provider {
            Provider::Telegram => Ok(Self::Telegram(telegram::bot(&token))),
            Provider::Discord => Ok(Self::Discord {
                http: std::sync::Arc::new(discord::client(&token)),
                token,
            }),
            Provider::WhatsApp => Err(Error::Unsupported),
        }
    }
    /// The SDK session for connection `c`. Nothing connects until used.
    pub async fn open(
        db: &sqlx::SqlitePool,
        c: &Connection,
        credential: Credential,
    ) -> Result<Self, Error> {
        match (c.provider, credential) {
            (Provider::WhatsApp, Credential::Device(key)) => {
                let device = crate::socials::store::device(db, c.id).await?;
                Ok(Self::WhatsApp(whatsapp::Link::new(
                    db.clone(),
                    c.id,
                    whatsapp::Vault::new(db.clone(), device, &key),
                )))
            }
            (provider, Credential::Token(token)) => Self::token(provider, token),
            (_, Credential::Device(_)) => Err(Error::Crypto),
        }
    }
}

#[async_trait]
pub trait Transport: Send + Sync {
    /// The bot identity a submitted token authenticates.
    async fn identity(&self, provider: Provider, token: &Token) -> Result<Identity, Error>;
    /// Link a new device into `vault`, reporting each code on `progress`, and
    /// return the account it linked.
    async fn pair(
        &self,
        db: &sqlx::SqlitePool,
        vault: whatsapp::Vault,
        progress: &crate::socials::pairing::Progress,
        stop: CancellationToken,
    ) -> Result<Identity, Error>;
    async fn destinations(&self, session: &Session) -> Result<Vec<Destination>, Error>;
    /// Look up a chat the platform can address by public `@username`.
    async fn resolve(
        &self,
        c: &Connection,
        session: &Session,
        username: &str,
    ) -> Result<Destination, Error>;
    async fn execute(&self, session: &Session, command: &Command) -> Result<Value, Error>;
    async fn send(
        &self,
        session: &Session,
        destination: &str,
        body: &str,
        reply: Option<&str>,
    ) -> Result<Value, Error>;
    async fn listen(
        &self,
        c: &Connection,
        session: &Session,
        offset: i64,
        events: mpsc::Sender<Envelope>,
        stop: CancellationToken,
    ) -> Result<(), Error>;
}
pub struct Native;
#[async_trait]
impl Transport for Native {
    async fn identity(&self, p: Provider, t: &Token) -> Result<Identity, Error> {
        match p {
            Provider::Telegram => telegram::identity(t).await,
            Provider::Discord => discord::identity(t).await,
            Provider::WhatsApp => Err(Error::Unsupported),
        }
    }
    async fn pair(
        &self,
        db: &sqlx::SqlitePool,
        vault: whatsapp::Vault,
        progress: &crate::socials::pairing::Progress,
        stop: CancellationToken,
    ) -> Result<Identity, Error> {
        whatsapp::pair(db.clone(), vault, progress, stop).await
    }
    async fn destinations(&self, session: &Session) -> Result<Vec<Destination>, Error> {
        match session {
            Session::Telegram(_) => Ok(vec![]),
            Session::Discord { http, .. } => discord::destinations(http).await,
            Session::WhatsApp(link) => whatsapp::destinations(link).await,
        }
    }
    async fn resolve(
        &self,
        c: &Connection,
        session: &Session,
        username: &str,
    ) -> Result<Destination, Error> {
        match session {
            Session::Telegram(bot) => telegram::resolve(c, bot, username).await,
            Session::Discord { .. } => Err(Error::Invalid(
                "Use a Discord channel ID from social_connections.",
            )),
            Session::WhatsApp(_) => Err(Error::Invalid(
                "Use a WhatsApp chat ID from social_connections or a phone number.",
            )),
        }
    }
    async fn execute(&self, session: &Session, cmd: &Command) -> Result<Value, Error> {
        match session {
            Session::Telegram(bot) => telegram::execute(bot, cmd).await,
            Session::Discord { http, .. } => discord::execute(http, cmd).await,
            Session::WhatsApp(link) => whatsapp::execute(link, cmd).await,
        }
    }
    async fn send(
        &self,
        session: &Session,
        d: &str,
        b: &str,
        reply: Option<&str>,
    ) -> Result<Value, Error> {
        match session {
            Session::Telegram(bot) => telegram::send(bot, d, b, reply).await,
            Session::Discord { http, .. } => discord::send(http, d, b, reply).await,
            Session::WhatsApp(link) => whatsapp::send(link, d, b, reply).await,
        }
    }
    async fn listen(
        &self,
        c: &Connection,
        session: &Session,
        o: i64,
        e: mpsc::Sender<Envelope>,
        s: CancellationToken,
    ) -> Result<(), Error> {
        match session {
            Session::Telegram(bot) => telegram::listen(c, bot, o, e, s).await,
            Session::Discord { token, .. } => discord::listen(c, token, e, s).await,
            Session::WhatsApp(link) => whatsapp::listen(link, e, s).await,
        }
    }
}

pub(crate) fn ensure_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sdk_clients_choose_tls_before_any_mail_session() {
        let token = Token::parse("123456789:deliberately-invalid-sdk-test-token".into()).unwrap();
        let _discord = discord::client(&token);
        let _telegram = telegram::bot(&token);
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
