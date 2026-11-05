use crate::socials::types::*;
use serde_json::Value;
use teloxide_core::{
    Bot,
    payloads::*,
    prelude::Requester,
    types::{ChatId, InputPollOption, MessageId, ReactionType, UpdateKind},
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(super) fn bot(t: &Token) -> Bot {
    super::ensure_crypto();
    Bot::new(t.0.as_str())
}
fn chat(s: &str) -> Result<ChatId, Error> {
    s.parse::<i64>()
        .map(ChatId)
        .map_err(|_| Error::Invalid("Telegram destination must be a chat ID."))
}
fn message(s: &str) -> Result<MessageId, Error> {
    s.parse::<i32>()
        .map(MessageId)
        .map_err(|_| Error::Invalid("Invalid Telegram message ID."))
}
fn value<T: serde::Serialize>(v: T) -> Result<Value, Error> {
    serde_json::to_value(v).map_err(|_| Error::Platform)
}
pub async fn identity(t: &Token) -> Result<Identity, Error> {
    let b = bot(t);
    let me = b.get_me().await.map_err(|_| Error::Platform)?;
    let hook = b.get_webhook_info().await.map_err(|_| Error::Platform)?;
    if hook.url.is_some() {
        return Err(Error::Invalid(
            "This Telegram bot already has a webhook. Remove it in the existing integration before connecting here.",
        ));
    }
    Ok(Identity {
        id: me.id.to_string(),
        display_name: me.first_name.clone(),
        username: me.username.clone(),
    })
}
pub async fn send(t: &Token, d: &str, body: &str, reply: Option<&str>) -> Result<Value, Error> {
    if body.chars().count() > 4096 {
        return Err(Error::Invalid(
            "Telegram messages are limited to 4096 characters.",
        ));
    }
    let b = bot(t);
    let mut request = b.send_message(chat(d)?, body);
    if let Some(reply) = reply {
        request =
            request.reply_parameters(teloxide_core::types::ReplyParameters::new(message(reply)?));
    }
    value(request.await.map_err(|_| Error::Platform)?)
}
pub async fn execute(t: &Token, cmd: &Command) -> Result<Value, Error> {
    let b = bot(t);
    match cmd {
        Command::Chat { destination } => value(
            b.get_chat(chat(destination)?)
                .await
                .map_err(|_| Error::Platform)?,
        ),
        Command::Members { destination, .. } => value(
            b.get_chat_administrators(chat(destination)?)
                .await
                .map_err(|_| Error::Platform)?,
        ),
        Command::Edit {
            destination,
            message_id,
            body,
        } => value(
            b.edit_message_text(chat(destination)?, message(message_id)?, body)
                .await
                .map_err(|_| Error::Platform)?,
        ),
        Command::Delete {
            destination,
            message_id,
        } => value(
            b.delete_message(chat(destination)?, message(message_id)?)
                .await
                .map_err(|_| Error::Platform)?,
        ),
        Command::React {
            destination,
            message_id,
            emoji,
        } => value(
            b.set_message_reaction(chat(destination)?, message(message_id)?)
                .reaction(vec![ReactionType::Emoji {
                    emoji: emoji.clone(),
                }])
                .await
                .map_err(|_| Error::Platform)?,
        ),
        Command::Pin {
            destination,
            message_id,
        } => value(
            b.pin_chat_message(chat(destination)?, message(message_id)?)
                .await
                .map_err(|_| Error::Platform)?,
        ),
        Command::Unpin {
            destination,
            message_id,
        } => value(
            b.unpin_chat_message(chat(destination)?)
                .message_id(message(message_id)?)
                .await
                .map_err(|_| Error::Platform)?,
        ),
        Command::Poll {
            destination,
            question,
            options,
        } => value(
            b.send_poll(
                chat(destination)?,
                question,
                options.iter().map(InputPollOption::new),
            )
            .await
            .map_err(|_| Error::Platform)?,
        ),
        _ => Err(Error::Unsupported),
    }
}
pub async fn listen(
    c: &Connection,
    t: &Token,
    mut offset: i64,
    events: mpsc::Sender<Envelope>,
    stop: CancellationToken,
) -> Result<(), Error> {
    let b = bot(t);
    loop {
        let updates = tokio::select! {biased;_ = stop.cancelled()=>return Ok(()),r=b.get_updates().offset(i32::try_from(offset).map_err(|_|Error::Platform)?).timeout(25)=>r.map_err(|_|Error::Platform)?};
        for update in updates {
            let next = i64::from(update.id.0) + 1;
            let m = match update.kind {
                UpdateKind::Message(m) | UpdateKind::ChannelPost(m) => m,
                _ => {
                    offset = next;
                    continue;
                }
            };
            let author = m.from.as_ref();
            let body = m
                .text()
                .or_else(|| m.caption())
                .unwrap_or("")
                .chars()
                .take(16000)
                .collect::<String>();
            let mentioned = c.username.as_ref().is_some_and(|name| {
                body.to_lowercase()
                    .contains(&format!("@{}", name.to_lowercase()))
            });
            let direct = m.chat.is_private();
            let name = m
                .chat
                .title()
                .map(str::to_owned)
                .or_else(|| m.chat.first_name().map(str::to_owned))
                .unwrap_or_else(|| m.chat.id.to_string());
            let incoming = Incoming {
                event_id: update.id.0.to_string(),
                destination: Destination {
                    id: m.chat.id.to_string(),
                    name,
                    kind: if direct { "direct" } else { "group" }.into(),
                    can_send: true,
                },
                message_id: m.id.0.to_string(),
                author_id: author.map_or_else(|| "channel".into(), |a| a.id.to_string()),
                body,
                mentioned,
                direct,
                from_bot: author.is_some_and(|a| a.is_bot),
            };
            let (ack, stored) = tokio::sync::oneshot::channel();
            tokio::select! {biased;_ = stop.cancelled()=>return Ok(()),r=events.send(Envelope{incoming,next_offset:next,ack})=>r.map_err(|_|Error::Platform)?};
            tokio::select! {biased;_=stop.cancelled()=>return Ok(()),r=stored=>r.map_err(|_|Error::Platform)?};
            offset = next;
        }
    }
}
