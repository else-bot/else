//! The owner's personal WhatsApp account, linked as a device by QR code.
//! `whatsapp-rust` speaks the WhatsApp Web protocol; its keys live in the
//! connection's sealed device store ([`backend::Backend`] over [`Vault`]).

mod backend;
mod vault;

pub use vault::Vault;

use crate::socials::types::*;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use whatsapp_rust::{
    Jid, MessageId, MessageRef,
    http::{HttpClient, HttpRequest, HttpResponse},
    prelude::{Bot, BotHandle, Client, EditRequest, Event, TokioRuntime},
    transport::{Connector, TokioWebSocketTransportFactory},
    wacore::proto_helpers::{MessageBuilderExt, MessageExt, build_quote_context},
    wacore_binary::JidExt,
    waproto::whatsapp as wa,
};

/// What the pairing card shows. `revision` advances on every change, so a
/// card can wait for the next one.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Pairing {
    Starting,
    /// Scan `code` with WhatsApp → Linked devices before `expires_in` seconds.
    Code {
        code: String,
        expires_in: u64,
    },
    /// Linked; the connection now holds the device.
    Paired {
        connection: Box<Connection>,
    },
    /// No code was scanned in time. Start again for a new one.
    Expired,
    Failed {
        message: String,
    },
    Cancelled,
}
impl Pairing {
    pub fn finished(&self) -> bool {
        !matches!(self, Self::Starting | Self::Code { .. })
    }
}

/// Sends from the owner's account stay human-paced.
const SEND_SPACING: Duration = Duration::from_secs(3);
/// How long a scanned code may take to finish linking.
const LINK_TIMEOUT: Duration = Duration::from_secs(60);

/// One connection's linked device: its key store and, while the listener
/// runs, the live client every operation goes through.
pub struct Link {
    backend: Arc<backend::Backend>,
    db: SqlitePool,
    connection: Uuid,
    client: watch::Sender<Option<Arc<Client>>>,
    last_send: Mutex<Option<Instant>>,
}

impl Link {
    pub fn new(db: SqlitePool, connection: Uuid, vault: Vault) -> Self {
        Self {
            backend: Arc::new(backend::Backend::new(vault)),
            db,
            connection,
            client: watch::channel(None).0,
            last_send: Mutex::new(None),
        }
    }

    fn client(&self) -> Result<Arc<Client>, Error> {
        self.client.borrow().clone().ok_or(Error::Invalid(
            "WhatsApp is not connected yet. Retry in a moment.",
        ))
    }

    /// The author of an observed message, to address it in a group.
    async fn author(&self, chat: &str, message: &str) -> Result<Option<(String, String)>, Error> {
        Ok(sqlx::query_as(
            "SELECT author_id,body FROM social_messages WHERE connection_id=?1 AND destination=?2 AND message_id=?3",
        )
        .bind(self.connection)
        .bind(chat)
        .bind(message)
        .fetch_optional(&self.db)
        .await?)
    }

    async fn target(&self, chat: &Jid, message: &str) -> Result<OwnedTarget, Error> {
        let client = self.client()?;
        let own = own_ids(&client);
        let author = self.author(&chat.to_string(), message).await?;
        let from_me = author
            .as_ref()
            .is_none_or(|(author, _)| own.iter().any(|id| id == author));
        let sender = match author {
            Some((author, _)) if !from_me => Some(jid(&author)?),
            _ => None,
        };
        if chat.is_group() && !from_me && sender.is_none() {
            return Err(Error::Invalid(
                "That message was not observed. Use a message ID from history.",
            ));
        }
        Ok(OwnedTarget {
            chat: chat.clone(),
            id: MessageId::new(message).map_err(|_| Error::Invalid("Invalid message ID."))?,
            sender,
            from_me,
        })
    }

    /// Wait out the spacing since the last send from this account.
    async fn pace(&self) {
        let mut last = self.last_send.lock().await;
        if let Some(at) = *last {
            let wait = SEND_SPACING.saturating_sub(at.elapsed());
            tokio::time::sleep(wait).await;
        }
        *last = Some(Instant::now());
    }
}

struct OwnedTarget {
    chat: Jid,
    id: MessageId,
    sender: Option<Jid>,
    from_me: bool,
}
impl OwnedTarget {
    fn reference(&self) -> Result<MessageRef<'_>, Error> {
        MessageRef::new(
            &self.chat,
            self.id.clone(),
            self.sender.as_ref(),
            self.from_me,
        )
        .map_err(|_| Error::Invalid("Invalid message reference."))
    }
}

fn own_ids(client: &Client) -> Vec<String> {
    [client.pn(), client.lid()]
        .into_iter()
        .flatten()
        .map(|j| j.to_non_ad().to_string())
        .collect()
}

/// A chat named by its JID or, for a person, a phone number in any common
/// spelling ("+33 6 12 34 56 78").
pub fn jid(destination: &str) -> Result<Jid, Error> {
    let text = destination.trim();
    if text.contains('@') {
        return text
            .parse::<Jid>()
            .map(|j| j.to_non_ad())
            .map_err(|_| Error::Invalid("Use a WhatsApp chat ID from social_connections."));
    }
    let digits: String = text
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '.' | '(' | ')'))
        .collect();
    let digits = digits.strip_prefix('+').unwrap_or(&digits);
    if (7..=15).contains(&digits.len()) && digits.chars().all(|c| c.is_ascii_digit()) {
        Ok(Jid::pn(digits))
    } else {
        Err(Error::Invalid(
            "Use a WhatsApp chat ID from social_connections or a phone number with its country code.",
        ))
    }
}

struct Http(reqwest::Client);

#[whatsapp_rust::async_trait]
impl HttpClient for Http {
    async fn execute(&self, request: HttpRequest) -> whatsapp_rust::anyhow::Result<HttpResponse> {
        let method = reqwest::Method::from_bytes(request.method.as_bytes())?;
        let mut builder = self.0.request(method, &request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body.to_vec());
        }
        let response = builder.send().await?;
        let status = response.status().as_u16();
        Ok(HttpResponse::new(status, response.bytes().await?.to_vec()))
    }
}

/// TLS through the explicit `ring` provider, so the WhatsApp socket never
/// depends on which provider the process installed first.
fn connector() -> Result<Connector, Error> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| Error::Crypto)?
    .with_platform_verifier()
    .map_err(|_| Error::Crypto)?
    .with_no_client_auth();
    Ok(Connector::Rustls(tokio_rustls::TlsConnector::from(
        Arc::new(config),
    )))
}

fn http() -> Result<Http, Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()
        .map(Http)
        .map_err(|_| Error::Platform)
}

/// Start a client on `backend`. Its events arrive on the returned channel;
/// a full channel holds the client back rather than dropping events.
async fn start(
    backend: Arc<backend::Backend>,
) -> Result<(BotHandle, mpsc::Receiver<Arc<Event>>), Error> {
    let (tx, rx) = mpsc::channel(64);
    let bot = Bot::builder()
        .with_backend_arc(backend)
        .with_http_client(http()?)
        .with_transport_factory(TokioWebSocketTransportFactory::new().with_connector(connector()?))
        .with_runtime(TokioRuntime)
        .on_event(move |event, _client| {
            let tx = tx.clone();
            async move {
                let _ = tx.send(event).await;
            }
        })
        .build()
        .await
        .map_err(|error| {
            tracing::warn!(%error, "whatsapp client did not start");
            Error::Platform
        })?;
    Ok((bot.spawn(), rx))
}

/// Link a new device into `vault`, reporting codes on `view`.
pub async fn pair(
    db: SqlitePool,
    vault: Vault,
    view: &watch::Sender<Pairing>,
    stop: CancellationToken,
) -> Result<Identity, Error> {
    let link = Link::new(db, Uuid::nil(), vault);
    let (handle, mut events) = start(link.backend.clone()).await?;
    let mut paired: Option<Jid> = None;
    let deadline = tokio::time::sleep(Duration::from_secs(24 * 3600));
    tokio::pin!(deadline);
    let result = loop {
        let event = tokio::select! {
            _ = stop.cancelled() => break Err(Error::Inactive),
            () = &mut deadline => break Err(Error::Platform),
            event = events.recv() => match event {
                Some(event) => event,
                None => break Err(Error::Platform),
            },
        };
        match &*event {
            Event::PairingQrCode(qr) if paired.is_none() => {
                view.send_replace(Pairing::Code {
                    code: qr.code.clone(),
                    expires_in: qr.timeout.as_secs(),
                });
            }
            Event::PairingQrCodesExhausted(_) => break Err(Error::Expired),
            Event::PairSuccess(success) => {
                paired = Some(success.id.to_non_ad());
                view.send_replace(Pairing::Starting);
                deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + LINK_TIMEOUT);
            }
            Event::PairError(_) => break Err(Error::Platform),
            Event::Connected(_) if paired.is_some() => {
                let client = handle.client();
                let number = paired.take().expect("paired");
                let name = client.push_name();
                break Ok(Identity {
                    id: number.user.to_string(),
                    display_name: if name.is_empty() {
                        format!("+{}", number.user)
                    } else {
                        name
                    },
                    username: Some(format!("+{}", number.user)),
                });
            }
            Event::LoggedOut(_) => break Err(Error::Platform),
            _ => {}
        }
    };
    let _ = handle.shutdown().await;
    result
}

fn kind(chat: &Jid) -> &'static str {
    if chat.is_group() { "group" } else { "person" }
}

pub async fn destinations(link: &Link) -> Result<Vec<Destination>, Error> {
    let client = link.client()?;
    let groups = client
        .groups()
        .list_participating()
        .await
        .map_err(|_| Error::Platform)?;
    Ok(groups
        .into_iter()
        .map(|g| Destination {
            id: g.id.to_string(),
            name: g.subject.unwrap_or_default(),
            kind: "group".into(),
            can_send: true,
        })
        .collect())
}

pub async fn execute(link: &Link, command: &Command) -> Result<Value, Error> {
    let client = link.client()?;
    match command {
        Command::Chat { destination } => {
            let chat = jid(destination)?;
            if !chat.is_group() {
                return Ok(json!({"id": chat.to_string(), "kind": "person"}));
            }
            let meta = client
                .groups()
                .fetch_metadata(&chat)
                .await
                .map_err(|_| Error::Platform)?;
            Ok(json!({
                "id": meta.id.to_string(), "kind": "group", "name": meta.subject,
                "description": meta.description.as_ref().map(ToString::to_string),
                "member_count": meta.participants.len(),
            }))
        }
        Command::Members { destination, limit } => {
            let chat = jid(destination)?;
            if !chat.is_group() {
                return Err(Error::Invalid("Members are listed for groups only."));
            }
            let meta = client
                .groups()
                .fetch_metadata(&chat)
                .await
                .map_err(|_| Error::Platform)?;
            let members: Vec<Value> = meta
                .participants
                .iter()
                .take(usize::from(*limit))
                .map(|p| {
                    json!({
                        "id": p.jid.to_string(),
                        "phone": p.phone_number.as_ref().map(|j| format!("+{}", j.user)),
                        "role": format!("{:?}", p.participant_type).to_lowercase(),
                    })
                })
                .collect();
            Ok(json!({"members": members, "total": meta.participants.len()}))
        }
        Command::React {
            destination,
            message_id,
            emoji,
        } => {
            let target = link.target(&jid(destination)?, message_id).await?;
            let sent = client
                .send_reaction(&target.reference()?, emoji)
                .await
                .map_err(|_| Error::Uncertain)?;
            Ok(json!({"id": sent.message_id.as_str()}))
        }
        Command::Edit {
            destination,
            message_id,
            body,
        } => {
            let target = link.target(&jid(destination)?, message_id).await?;
            if !target.from_me {
                return Err(Error::Invalid(
                    "Only the owner's own messages can be edited.",
                ));
            }
            link.pace().await;
            let sent = client
                .edit_message(EditRequest::new(
                    target.reference()?,
                    wa::Message::text(body.clone()),
                ))
                .await
                .map_err(|_| Error::Uncertain)?;
            Ok(json!({"id": sent.message_id.as_str()}))
        }
        Command::Delete {
            destination,
            message_id,
        } => {
            let target = link.target(&jid(destination)?, message_id).await?;
            if !target.from_me {
                return Err(Error::Invalid(
                    "Only the owner's own messages can be deleted for everyone.",
                ));
            }
            client
                .revoke_message(&target.reference()?)
                .await
                .map_err(|_| Error::Uncertain)?;
            Ok(json!({"deleted": message_id}))
        }
        Command::CreateGroup { name, participants } => {
            let mut options =
                whatsapp_rust::wacore::iq::groups::GroupCreateOptions::new(name.clone());
            for participant in participants {
                options.participants.push(
                    whatsapp_rust::wacore::iq::groups::GroupParticipantOptions::new(jid(
                        participant,
                    )?),
                );
            }
            link.pace().await;
            let created = client
                .groups()
                .create_group(options)
                .await
                .map_err(|_| Error::Uncertain)?
                .metadata;
            Ok(json!({
                "id": created.id.to_string(), "name": created.subject,
                "members": created.participants.iter().map(|p| p.jid.to_string()).collect::<Vec<_>>(),
            }))
        }
        _ => Err(Error::Unsupported),
    }
}

pub async fn send(
    link: &Link,
    destination: &str,
    body: &str,
    reply: Option<&str>,
) -> Result<Value, Error> {
    let client = link.client()?;
    let chat = jid(destination)?;
    let message = match reply {
        None => wa::Message::text(body),
        Some(reply) => {
            let (author, quoted) =
                link.author(&chat.to_string(), reply)
                    .await?
                    .ok_or(Error::Invalid(
                        "The replied message was not observed. Use a message ID from history.",
                    ))?;
            wa::Message::text_with_context(
                body,
                build_quote_context(reply, author, &wa::Message::text(quoted)),
            )
        }
    };
    link.pace().await;
    let sent = client
        .send_message(chat, message)
        .await
        .map_err(|_| Error::Uncertain)?;
    Ok(json!({"id": sent.message_id.as_str()}))
}

/// Text of a message the bot can read, or `None` for media and protocol
/// messages it cannot.
fn text(message: &wa::Message) -> Option<String> {
    let base = message.get_base_message();
    base.text_content()
        .or_else(|| base.get_caption())
        .map(str::to_owned)
        .filter(|t| !t.trim().is_empty())
}

/// One message as the client reported it, live or from synced history.
struct Seen<'a> {
    chat: &'a Jid,
    sender: &'a Jid,
    id: &'a str,
    name: Option<&'a str>,
    message: &'a wa::Message,
    from_me: bool,
    replay: bool,
}

fn incoming(
    own: &[String],
    Seen {
        chat,
        sender,
        id,
        name,
        message,
        from_me,
        replay,
    }: Seen<'_>,
) -> Option<Incoming> {
    let body = text(message)?;
    let chat = chat.to_non_ad();
    let mentioned = message
        .get_base_message()
        .context_info()
        .is_some_and(|ctx| ctx.mentioned_jid.iter().any(|j| own.iter().any(|o| o == j)));
    let chat_id = chat.to_string();
    Some(Incoming {
        event_id: format!("{chat_id}/{id}"),
        destination: Destination {
            name: name
                .filter(|n| !n.is_empty())
                .map_or_else(|| chat_id.clone(), str::to_owned),
            kind: kind(&chat).into(),
            id: chat_id,
            can_send: true,
        },
        message_id: id.to_owned(),
        author_id: sender.to_non_ad().to_string(),
        body,
        mentioned,
        direct: !chat.is_group(),
        from_bot: from_me,
        replay,
    })
}

pub async fn listen(
    link: &Link,
    events: mpsc::Sender<Envelope>,
    stop: CancellationToken,
) -> Result<(), Error> {
    let (handle, mut inbox) = start(link.backend.clone()).await?;
    let client = handle.client();
    let mut names: std::collections::HashMap<String, String> = Default::default();
    let result = loop {
        let event = tokio::select! {
            _ = stop.cancelled() => break Ok(()),
            event = inbox.recv() => match event {
                Some(event) => event,
                None => break Err(Error::Platform),
            },
        };
        let mut batch = Vec::new();
        match &*event {
            Event::Connected(_) => {
                link.client.send_replace(Some(client.clone()));
            }
            Event::PairingQrCode(_) | Event::LoggedOut(_) => break Err(Error::SignedOut),
            Event::StreamReplaced(_) => break Err(Error::ReceiverConflict),
            Event::Messages(messages) => {
                let own = own_ids(&client);
                for m in messages.messages.iter() {
                    let source = &m.info.source;
                    let chat = source.chat.to_non_ad().to_string();
                    let name = if source.chat.is_group() {
                        if !names.contains_key(&chat)
                            && let Ok(Ok(meta)) = tokio::time::timeout(
                                Duration::from_secs(10),
                                client.groups().fetch_metadata(&source.chat),
                            )
                            .await
                        {
                            names.insert(chat.clone(), meta.subject.unwrap_or_default());
                        }
                        names.get(&chat).cloned()
                    } else if source.is_from_me {
                        None
                    } else {
                        Some(m.info.push_name.to_string())
                    };
                    batch.extend(incoming(
                        &own,
                        Seen {
                            chat: &source.chat,
                            sender: &source.sender,
                            id: m.info.id.as_str(),
                            name: name.as_deref(),
                            message: &m.message,
                            from_me: source.is_from_me,
                            replay: false,
                        },
                    ));
                }
            }
            Event::HistorySync(sync) => {
                let own = own_ids(&client);
                if let Some(history) = sync.get() {
                    for conversation in &history.conversations {
                        let Ok(chat) = conversation.id.parse::<Jid>() else {
                            continue;
                        };
                        let name = conversation
                            .name
                            .as_deref()
                            .or(conversation.display_name.as_deref());
                        let mut messages: Vec<_> = conversation
                            .messages
                            .iter()
                            .filter_map(|m| m.message.as_option())
                            .collect();
                        messages.sort_by_key(|m| m.message_timestamp.unwrap_or_default());
                        for info in messages {
                            let key = info.key.as_option();
                            let (Some(id), Some(message)) =
                                (key.and_then(|k| k.id.as_deref()), info.message.as_option())
                            else {
                                continue;
                            };
                            let from_me = key.and_then(|k| k.from_me).unwrap_or(false);
                            let sender = key
                                .and_then(|k| k.participant.as_deref())
                                .and_then(|p| p.parse::<Jid>().ok())
                                .or_else(|| from_me.then(|| client.pn()).flatten())
                                .unwrap_or_else(|| chat.clone());
                            batch.extend(incoming(
                                &own,
                                Seen {
                                    chat: &chat,
                                    sender: &sender,
                                    id,
                                    name,
                                    message,
                                    from_me,
                                    replay: true,
                                },
                            ));
                        }
                    }
                }
            }
            _ => {}
        }
        for incoming in batch {
            let (ack, acked) = tokio::sync::oneshot::channel();
            let envelope = Envelope {
                incoming,
                next_offset: 0,
                ack,
            };
            tokio::select! {
                _ = stop.cancelled() => break,
                sent = events.send(envelope) => if sent.is_err() { break },
            }
            tokio::select! {
                _ = stop.cancelled() => break,
                _ = acked => {},
            }
        }
    };
    link.client.send_replace(None);
    let _ = handle.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_is_named_by_its_id_or_a_phone_number_with_country_code() {
        for number in [
            "+33 6 12 34 56 78",
            "33612345678",
            "+33-6-12-34-56-78",
            "(33) 612345678",
        ] {
            assert_eq!(
                jid(number).unwrap().to_string(),
                "33612345678@s.whatsapp.net",
                "{number}"
            );
        }
        assert_eq!(
            jid("120363025246125486@g.us").unwrap().to_string(),
            "120363025246125486@g.us"
        );
        for refused in ["", "12345", "+33 6 12 AB", "not a chat"] {
            assert!(jid(refused).is_err(), "{refused}");
        }
    }
}
