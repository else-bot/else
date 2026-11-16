//! The owner's personal WhatsApp account, linked as a device by QR code.
//! `whatsapp-rust` speaks the WhatsApp Web protocol; its keys live in the
//! connection's sealed device store ([`backend::Backend`] over [`Vault`]).

mod backend;
mod client;
mod commands;
mod inbound;
mod pair;
mod vault;

pub use commands::{destinations, execute, send};
pub use pair::pair;
pub use vault::Vault;

use client::start;
use inbound::{Inbound, replay_history};

use crate::socials::{pairing::Progress, types::*};
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

/// Sends from the owner's account stay human-paced.
const SEND_SPACING: Duration = Duration::from_secs(3);
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

fn kind(chat: &Jid) -> &'static str {
    if chat.is_group() { "group" } else { "person" }
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
}
