//! Messages and history arriving from WhatsApp, recorded before WhatsApp
//! hears they arrived.

use super::*;
use std::collections::HashMap;

/// The largest history chunk the receiver inflates.
const MAX_HISTORY: u64 = 128 << 20;

/// Keeps each history chunk in the device store before its receipt; the
/// listener records and then forgets it.
pub(super) struct Capture {
    backend: Arc<backend::Backend>,
    captured: Arc<Notify>,
}
impl Capture {
    pub(super) fn new(backend: Arc<backend::Backend>, captured: Arc<Notify>) -> Self {
        Self { backend, captured }
    }
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
pub(super) struct Refuse;

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
pub(super) struct Inbound {
    pub(super) events: mpsc::Sender<Envelope>,
    pub(super) stop: CancellationToken,
    pub(super) names: Mutex<HashMap<String, String>>,
}

impl Inbound {
    pub(super) async fn deliver(&self, batch: Vec<Incoming>) -> Result<(), Error> {
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

    pub(super) async fn seen(&self, client: &Client, batch: &[InboundMessage]) -> Vec<Incoming> {
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
            let chat = canonical(client, &source.chat, None).await;
            let sender = canonical(client, &source.sender, None).await;
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
        let chat = canonical(client, &wire, Some(&known)).await;
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
            let sender = canonical(client, &sender, Some(&known)).await;
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
pub(super) async fn replay_history(
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
pub(super) struct Seen<'a> {
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
type Known = HashMap<String, String>;

/// A person's phone-number JID when the device knows their LID, so their
/// chat has one ID whichever form WhatsApp addressed it by.
async fn canonical(client: &Client, jid: &Jid, known: Option<&Known>) -> Jid {
    let jid = jid.to_non_ad();
    if !jid.server.is_lid_family() {
        return jid;
    }
    if let Some(pn) = known.and_then(|known| known.get(jid.user.as_str())) {
        return Jid::pn(pn.as_str());
    }
    match client.get_lid_pn_entry(&jid).await {
        Ok(Some(entry)) => Jid::pn(entry.phone_number.as_ref()),
        _ => jid,
    }
}

pub(super) fn incoming(
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

#[cfg(test)]
mod tests {
    use super::*;

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
