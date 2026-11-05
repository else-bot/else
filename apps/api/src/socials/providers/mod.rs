pub mod discord;
pub mod telegram;

use crate::socials::types::*;
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[async_trait]
pub trait Transport: Send + Sync {
    async fn identity(&self, provider: Provider, token: &Token) -> Result<Identity, Error>;
    async fn destinations(&self, c: &Connection, token: &Token) -> Result<Vec<Destination>, Error>;
    async fn execute(
        &self,
        c: &Connection,
        token: &Token,
        command: &Command,
    ) -> Result<Value, Error>;
    async fn send(
        &self,
        c: &Connection,
        token: &Token,
        destination: &str,
        body: &str,
        reply: Option<&str>,
    ) -> Result<Value, Error>;
    async fn listen(
        &self,
        c: &Connection,
        token: &Token,
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
    async fn destinations(&self, c: &Connection, t: &Token) -> Result<Vec<Destination>, Error> {
        match c.provider {
            Provider::Telegram => Ok(vec![]),
            Provider::Discord => discord::destinations(t).await,
        }
    }
    async fn execute(&self, c: &Connection, t: &Token, cmd: &Command) -> Result<Value, Error> {
        match c.provider {
            Provider::Telegram => telegram::execute(t, cmd).await,
            Provider::Discord => discord::execute(t, cmd).await,
        }
    }
    async fn send(
        &self,
        c: &Connection,
        t: &Token,
        d: &str,
        b: &str,
        reply: Option<&str>,
    ) -> Result<Value, Error> {
        match c.provider {
            Provider::Telegram => telegram::send(t, d, b, reply).await,
            Provider::Discord => discord::send(t, d, b, reply).await,
        }
    }
    async fn listen(
        &self,
        c: &Connection,
        t: &Token,
        o: i64,
        e: mpsc::Sender<Envelope>,
        s: CancellationToken,
    ) -> Result<(), Error> {
        match c.provider {
            Provider::Telegram => telegram::listen(c, t, o, e, s).await,
            Provider::Discord => discord::listen(c, t, e, s).await,
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
