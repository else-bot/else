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

/// How a bot named a chat: by the ID incoming messages carry, or by a
/// public username only the platform can turn into one.
pub enum Named {
    Id(String),
    Username(String),
}
impl Named {
    pub fn parse(provider: Provider, destination: &str) -> Result<Self, Error> {
        match provider {
            Provider::WhatsApp => Ok(Self::Id(whatsapp::jid(destination)?.to_string())),
            Provider::Telegram | Provider::Discord => Ok(match public_username(destination)? {
                Some(username) => Self::Username(username),
                None => Self::Id(destination.to_owned()),
            }),
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
    /// Send `body` with `files`, which drafting already checked against the
    /// platform's limits. A send of several parts that fails after its first
    /// part went out is `Error::Uncertain`.
    async fn send(
        &self,
        session: &Session,
        destination: &str,
        body: &str,
        reply: Option<&str>,
        files: &[SendFile],
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
/// Telegram and WhatsApp take a caption of at most this many characters.
pub const MAX_CAPTION: usize = 1024;

/// The parts a message with files goes out as, in order: the body as the
/// first file's caption when it fits, otherwise as its own text first.
pub fn parts<'a>(body: &'a str, files: &'a [SendFile]) -> Vec<Part<'a>> {
    let captioned = body.chars().count() <= MAX_CAPTION;
    let mut parts = Vec::with_capacity(files.len() + 1);
    if !captioned {
        parts.push(Part::Text(body));
    }
    for (index, file) in files.iter().enumerate() {
        let caption = (captioned && index == 0).then_some(body);
        parts.push(Part::File(file, caption));
    }
    parts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part<'a> {
    Text(&'a str),
    File(&'a SendFile, Option<&'a str>),
}

impl std::fmt::Debug for SendFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendFile")
            .field("name", &self.name)
            .finish()
    }
}

impl PartialEq for SendFile {
    fn eq(&self, other: &Self) -> bool {
        self.sha256 == other.sha256 && self.name == other.name
    }
}

impl Eq for SendFile {}

/// The first part's receipt, once every part went out. A failure after the
/// first part means some of the message reached the chat.
pub async fn send_parts<'a, F, Fut>(parts: Vec<Part<'a>>, mut send: F) -> Result<Value, Error>
where
    F: FnMut(Part<'a>, bool) -> Fut,
    Fut: std::future::Future<Output = Result<Value, Error>>,
{
    let count = parts.len();
    let mut first = None;
    for (index, part) in parts.into_iter().enumerate() {
        match send(part, index == 0).await {
            Ok(value) => {
                first.get_or_insert(value);
            }
            Err(_) if index > 0 => return Err(Error::Uncertain),
            Err(error) => return Err(error),
        }
    }
    let mut receipt = first.unwrap_or(Value::Null);
    if count > 1
        && let Some(object) = receipt.as_object_mut()
    {
        object.insert("parts".into(), count.into());
    }
    Ok(receipt)
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
        files: &[SendFile],
    ) -> Result<Value, Error> {
        match session {
            Session::Telegram(bot) => telegram::send(bot, d, b, reply, files).await,
            Session::Discord { http, .. } => discord::send(http, d, b, reply, files).await,
            Session::WhatsApp(link) => whatsapp::send(link, d, b, reply, files).await,
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

    fn file(name: &str) -> SendFile {
        SendFile {
            name: name.into(),
            media_type: "image/png".into(),
            sha256: name.into(),
            data: std::sync::Arc::from(&b"x"[..]),
        }
    }

    #[test]
    fn a_short_body_captions_the_first_file_and_a_long_one_goes_first() {
        let files = [file("a"), file("b")];
        assert_eq!(
            parts("Flyer", &files),
            [
                Part::File(&files[0], Some("Flyer")),
                Part::File(&files[1], None)
            ]
        );
        let long = "x".repeat(MAX_CAPTION + 1);
        assert_eq!(
            parts(&long, &files[..1]),
            [Part::Text(&long), Part::File(&files[0], None)]
        );
    }

    #[tokio::test]
    async fn a_failure_after_the_first_part_is_uncertain() {
        let files = [file("a"), file("b")];
        let first = send_parts(parts("Hi", &files), |_, first| async move {
            if first {
                Err(Error::Platform)
            } else {
                Ok(Value::Null)
            }
        })
        .await;
        assert!(matches!(first, Err(Error::Platform)), "nothing went out");
        let later = send_parts(parts("Hi", &files), |_, first| async move {
            if first {
                Ok(serde_json::json!({"id": "1"}))
            } else {
                Err(Error::Platform)
            }
        })
        .await;
        assert!(
            matches!(later, Err(Error::Uncertain)),
            "the first part went out"
        );
        let all = send_parts(parts("Hi", &files), |_, _| async {
            Ok(serde_json::json!({"id": "1"}))
        })
        .await
        .unwrap();
        assert_eq!(all, serde_json::json!({"id": "1", "parts": 2}));
    }
}
