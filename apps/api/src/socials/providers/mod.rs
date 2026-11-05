pub mod discord;
pub mod telegram;

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
}
impl Session {
    pub fn new(provider: Provider, token: Token) -> Self {
        match provider {
            Provider::Telegram => Self::Telegram(telegram::bot(&token)),
            Provider::Discord => Self::Discord {
                http: std::sync::Arc::new(discord::client(&token)),
                token,
            },
        }
    }
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn identity(&self, provider: Provider, token: &Token) -> Result<Identity, Error>;
    async fn destinations(&self, session: &Session) -> Result<Vec<Destination>, Error>;
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
        }
    }
    async fn destinations(&self, session: &Session) -> Result<Vec<Destination>, Error> {
        match session {
            Session::Telegram(_) => Ok(vec![]),
            Session::Discord { http, .. } => discord::destinations(http).await,
        }
    }
    async fn execute(&self, session: &Session, cmd: &Command) -> Result<Value, Error> {
        match session {
            Session::Telegram(bot) => telegram::execute(bot, cmd).await,
            Session::Discord { http, .. } => discord::execute(http, cmd).await,
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
