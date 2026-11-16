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
use tokio::sync::{Mutex, Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use whatsapp_rust::{
    HistorySyncCaptureHook, InboundDurabilityHook, Jid, MessageId, MessageRef,
    http::{HttpClient, HttpRequest, HttpResponse},
    prelude::{Bot, BotHandle, Client, EditRequest, Event, TokioRuntime},
    transport::{Connector, TokioWebSocketTransportFactory},
    wacore::proto_helpers::{MessageBuilderExt, MessageExt, build_quote_context},
    wacore::types::events::InboundMessage,
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
/// How long to wait for WhatsApp to send a code, or the next one.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const UNREACHABLE: &str = "WhatsApp could not be reached. Check this computer's internet connection, then show a new code.";

/// Where a pairing is: waiting for WhatsApp, showing a code, or linking the
/// phone that scanned it.
enum Stage {
    Connecting,
    Showing,
    Linking(Jid),
}

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

/// The largest history chunk the receiver inflates.
const MAX_HISTORY: u64 = 128 << 20;

/// Start a client on `backend`. Messages reach `inbound` before WhatsApp
/// hears they arrived, and history chunks are kept in the device store
/// before their receipt, waking `captured`. The returned events are
/// observations only: the client may drop them under load.
async fn start(
    backend: Arc<backend::Backend>,
    inbound: Arc<dyn InboundDurabilityHook>,
    captured: Arc<Notify>,
) -> Result<(BotHandle, mpsc::Receiver<Arc<Event>>), Error> {
    let (tx, rx) = mpsc::channel(64);
    let capture = Arc::new(Capture {
        backend: backend.clone(),
        captured,
    });
    let bot = Bot::builder()
        .with_backend_arc(backend)
        .with_http_client(http()?)
        .with_transport_factory(TokioWebSocketTransportFactory::new().with_connector(connector()?))
        .with_runtime(TokioRuntime)
        .with_inbound_durability_hook_arc(inbound)
        .with_history_sync_capture_hook_arc(capture)
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

/// Keeps each history chunk in the device store before its receipt; the
/// listener records and then forgets it.
struct Capture {
    backend: Arc<backend::Backend>,
    captured: Arc<Notify>,
}

#[whatsapp_rust::async_trait]
impl HistorySyncCaptureHook for Capture {
    async fn on_history_sync(
        &self,
        _client: Arc<Client>,
        message_id: &str,
        _sync_type: Option<wa::message::HistorySyncType>,
        compressed: &[u8],
    ) -> whatsapp_rust::anyhow::Result<()> {
        self.backend
            .capture_history(message_id, compressed)
            .await
            .map_err(whatsapp_rust::anyhow::Error::new)?;
        self.captured.notify_one();
        Ok(())
    }
}

/// Refuses messages while a device links, so WhatsApp keeps them unacked
/// and the client redelivers them to the connection's listener.
struct Refuse;

#[whatsapp_rust::async_trait]
impl InboundDurabilityHook for Refuse {
    async fn on_messages(
        &self,
        _client: Arc<Client>,
        _batch: &[InboundMessage],
    ) -> whatsapp_rust::anyhow::Result<()> {
        Err(whatsapp_rust::anyhow::anyhow!(
            "the device is still linking"
        ))
    }
}

/// Hands messages to the connection's receiver and returns once each is
/// recorded, so WhatsApp hears a message arrived only after it is stored.
struct Inbound {
    events: mpsc::Sender<Envelope>,
    stop: CancellationToken,
    names: Mutex<std::collections::HashMap<String, String>>,
}

impl Inbound {
    async fn deliver(&self, batch: Vec<Incoming>) -> Result<(), Error> {
        for incoming in batch {
            let (ack, acked) = tokio::sync::oneshot::channel();
            let envelope = Envelope {
                incoming,
                next_offset: 0,
                ack,
            };
            tokio::select! {
                _ = self.stop.cancelled() => return Err(Error::Inactive),
                sent = self.events.send(envelope) => sent.map_err(|_| Error::Inactive)?,
            }
            tokio::select! {
                _ = self.stop.cancelled() => return Err(Error::Inactive),
                acked = acked => acked.map_err(|_| Error::Inactive)?,
            }
        }
        Ok(())
    }

    /// A group's subject, looked up once per chat.
    async fn group_name(&self, client: &Client, chat: &Jid) -> Option<String> {
        let key = chat.to_non_ad().to_string();
        if let Some(name) = self.names.lock().await.get(&key) {
            return Some(name.clone());
        }
        let meta = tokio::time::timeout(
            Duration::from_secs(10),
            client.groups().fetch_metadata(chat),
        )
        .await
        .ok()?
        .ok()?;
        let name = meta.subject.unwrap_or_default();
        self.names.lock().await.insert(key, name.clone());
        Some(name)
    }
}

#[whatsapp_rust::async_trait]
impl InboundDurabilityHook for Inbound {
    async fn on_messages(
        &self,
        client: Arc<Client>,
        batch: &[InboundMessage],
    ) -> whatsapp_rust::anyhow::Result<()> {
        let seen = self.seen(&client, batch).await;
        self.deliver(seen)
            .await
            .map_err(whatsapp_rust::anyhow::Error::new)
    }
}

impl Inbound {
    async fn seen(&self, client: &Client, batch: &[InboundMessage]) -> Vec<Incoming> {
        let own = own_ids(client);
        let mut seen = Vec::new();
        for m in batch {
            let source = &m.info.source;
            let name = if source.chat.is_group() {
                self.group_name(client, &source.chat).await
            } else if source.is_from_me {
                None
            } else {
                Some(m.info.push_name.to_string())
            };
            let chat = canonical(client, &source.chat, &Known::new()).await;
            let sender = canonical(client, &source.sender, &Known::new()).await;
            seen.extend(incoming(
                &own,
                Seen {
                    wire: &source.chat,
                    chat: &chat,
                    sender: &sender,
                    id: m.info.id.as_str(),
                    name: name.as_deref(),
                    message: &m.message,
                    from_me: source.is_from_me,
                    replay: false,
                },
            ));
        }
        seen
    }
}

/// The messages of one history chunk, oldest first in each chat.
async fn history(own: &[String], client: &Client, history: &wa::HistorySync) -> Vec<Incoming> {
    // The client learns this chunk's mappings only after capture, so read
    // them from the chunk itself.
    let known: Known = history
        .phone_number_to_lid_mappings
        .iter()
        .filter_map(|m| {
            let lid = m.lid_jid.as_deref()?.parse::<Jid>().ok()?;
            let pn = m.pn_jid.as_deref()?.parse::<Jid>().ok()?;
            Some((lid.user.to_string(), pn.user.to_string()))
        })
        .collect();
    let mut batch = Vec::new();
    for conversation in &history.conversations {
        let Ok(wire) = conversation.id.parse::<Jid>() else {
            continue;
        };
        let chat = canonical(client, &wire, &known).await;
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
                .unwrap_or_else(|| wire.clone());
            let sender = canonical(client, &sender, &known).await;
            batch.extend(incoming(
                own,
                Seen {
                    wire: &wire,
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
    batch
}

/// Record every captured history chunk, then forget it. A chunk that does
/// not decode is dropped, since it never will.
async fn replay_history(
    backend: &backend::Backend,
    client: &Client,
    inbound: &Inbound,
) -> Result<(), Error> {
    for (id, compressed) in backend.history().await? {
        let decoded = tokio::task::spawn_blocking(move || {
            whatsapp_rust::wacore_binary::zlib_pool::decompress_zlib_pooled(
                &compressed,
                MAX_HISTORY,
            )
            .ok()
            .and_then(|raw| whatsapp_rust::waproto::codec::history_sync_decode(&raw).ok())
        })
        .await
        .ok()
        .flatten();
        match decoded {
            Some(chunk) => {
                inbound
                    .deliver(history(&own_ids(client), client, &chunk).await)
                    .await?
            }
            None => tracing::warn!("dropped a WhatsApp history chunk that did not decode"),
        }
        backend.forget_history(&id).await?;
    }
    Ok(())
}

/// Link a new device into `vault`, reporting codes on `view`.
pub async fn pair(
    db: SqlitePool,
    vault: Vault,
    view: &watch::Sender<Pairing>,
    stop: CancellationToken,
) -> Result<Identity, Error> {
    let link = Link::new(db, Uuid::nil(), vault);
    let (handle, mut events) = start(
        link.backend.clone(),
        Arc::new(Refuse),
        Arc::new(Notify::new()),
    )
    .await?;
    let mut stage = Stage::Connecting;
    let deadline = tokio::time::sleep(CONNECT_TIMEOUT);
    tokio::pin!(deadline);
    let result = loop {
        let event = tokio::select! {
            _ = stop.cancelled() => break Err(Error::Inactive),
            () = &mut deadline => break Err(match stage {
                Stage::Connecting => Error::Invalid(UNREACHABLE),
                Stage::Showing => Error::Expired,
                Stage::Linking(_) => Error::Platform,
            }),
            event = events.recv() => match event {
                Some(event) => event,
                None => break Err(Error::Platform),
            },
        };
        match &*event {
            Event::PairingQrCode(qr) if !matches!(stage, Stage::Linking(_)) => {
                stage = Stage::Showing;
                view.send_replace(Pairing::Code {
                    code: qr.code.clone(),
                    expires_in: qr.timeout.as_secs(),
                });
                deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + qr.timeout + CONNECT_TIMEOUT);
            }
            Event::PairingQrCodesExhausted(_) => break Err(Error::Expired),
            Event::PairSuccess(success) => {
                stage = Stage::Linking(success.id.to_non_ad());
                view.send_replace(Pairing::Starting);
                deadline
                    .as_mut()
                    .reset(tokio::time::Instant::now() + LINK_TIMEOUT);
            }
            Event::PairError(_) => break Err(Error::Platform),
            Event::Connected(_) => {
                let Stage::Linking(number) = &stage else {
                    continue;
                };
                let name = handle.client().push_name();
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
    /// The chat as WhatsApp addressed it, which keys deduplication.
    wire: &'a Jid,
    chat: &'a Jid,
    sender: &'a Jid,
    id: &'a str,
    name: Option<&'a str>,
    message: &'a wa::Message,
    from_me: bool,
    replay: bool,
}

/// LID users mapped to phone-number users.
type Known = std::collections::HashMap<String, String>;

/// A person's phone-number JID when the device knows their LID, so their
/// chat has one ID whichever form WhatsApp addressed it by.
async fn canonical(client: &Client, jid: &Jid, known: &Known) -> Jid {
    let jid = jid.to_non_ad();
    if !jid.server.is_lid_family() {
        return jid;
    }
    if let Some(pn) = known.get(jid.user.as_str()) {
        return Jid::pn(pn.as_str());
    }
    match client.get_lid_pn_entry(&jid).await {
        Ok(Some(entry)) => Jid::pn(entry.phone_number.as_ref()),
        _ => jid,
    }
}

fn incoming(
    own: &[String],
    Seen {
        wire,
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
        event_id: format!("{}/{id}", wire.to_non_ad()),
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
    let inbound = Arc::new(Inbound {
        events,
        stop: stop.child_token(),
        names: Mutex::default(),
    });
    let captured = Arc::new(Notify::new());
    let (handle, mut observed) =
        start(link.backend.clone(), inbound.clone(), captured.clone()).await?;
    let client = handle.client();
    // History captured before a restart, or while the device linked.
    captured.notify_one();
    let result = loop {
        let event = tokio::select! {
            _ = stop.cancelled() => break Ok(()),
            () = captured.notified() => {
                if let Err(error) = replay_history(&link.backend, &client, &inbound).await {
                    break if stop.is_cancelled() { Ok(()) } else { Err(error) };
                }
                continue;
            }
            event = observed.recv() => match event {
                Some(event) => event,
                None => break Err(Error::Platform),
            },
        };
        match &*event {
            Event::Connected(_) => {
                link.client.send_replace(Some(client.clone()));
            }
            Event::PairingQrCode(_) | Event::LoggedOut(_) => break Err(Error::SignedOut),
            Event::StreamReplaced(_) => break Err(Error::ReceiverConflict),
            // Messages the phone recovered skip the durability hook; this
            // event is their only delivery.
            Event::Messages(batch) if !batch.hook_committed => {
                let seen = inbound.seen(&client, &batch.messages).await;
                if let Err(error) = inbound.deliver(seen).await {
                    break if stop.is_cancelled() {
                        Ok(())
                    } else {
                        Err(error)
                    };
                }
            }
            _ => {}
        }
    };
    inbound.stop.cancel();
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

    #[test]
    fn a_lid_chat_is_recorded_under_the_persons_number_but_deduplicated_by_its_wire_id() {
        let wire: Jid = "100000012345678@lid".parse().unwrap();
        let person = jid("33612345678").unwrap();
        let message = wa::Message::text("hi");
        let seen = incoming(
            &[],
            Seen {
                wire: &wire,
                chat: &person,
                sender: &person,
                id: "3EB0ABC",
                name: Some("Ada"),
                message: &message,
                from_me: false,
                replay: false,
            },
        )
        .unwrap();
        assert_eq!(seen.destination.id, "33612345678@s.whatsapp.net");
        assert_eq!(seen.author_id, "33612345678@s.whatsapp.net");
        assert_eq!(seen.event_id, "100000012345678@lid/3EB0ABC");
        assert!(seen.direct);
    }
}
