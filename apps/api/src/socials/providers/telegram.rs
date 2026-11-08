use crate::socials::types::*;
use serde_json::Value;
use std::time::Duration;
use teloxide_core::{
    ApiError, Bot, RequestError,
    payloads::*,
    prelude::Requester,
    types::{
        AllowedUpdate, ChatId, ChatMemberKind, ChatPermissions, InputPollOption, MessageId,
        ReactionType, Recipient, Restricted, UpdateKind, UserId,
    },
};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How long Telegram holds an idle `getUpdates` long poll.
const POLL_SECONDS: u32 = 25;

pub(super) fn bot(t: &Token) -> Bot {
    super::ensure_crypto();
    // teloxide 0.13 ignores the long-poll timeout hint, so its default
    // 17-second client timeout would end every idle poll with an error.
    let client = teloxide_core::net::default_reqwest_settings()
        .timeout(Duration::from_secs(u64::from(POLL_SECONDS) + 15))
        .build()
        .expect("the Telegram HTTP client builds");
    Bot::with_client(t.0.as_str(), client)
}

/// Telegram delivers a bot's updates to one receiver: a webhook or another
/// poller takes them all, so no chat ever reaches this connection.
fn poll_error(error: RequestError) -> Error {
    match error {
        RequestError::Api(ApiError::CantGetUpdates | ApiError::TerminatedByOtherGetUpdates) => {
            Error::ReceiverConflict
        }
        // teloxide 0.13 matches only an older, shorter webhook message.
        RequestError::Api(ApiError::Unknown(text)) if text.contains("webhook is active") => {
            Error::ReceiverConflict
        }
        _ => Error::Platform,
    }
}
fn chat(s: &str) -> Result<ChatId, Error> {
    s.parse::<i64>().map(ChatId).map_err(|_| {
        Error::Invalid("Telegram destination must be a chat ID or a public @username.")
    })
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
/// Telegram addresses a public channel or supergroup by `@username`.
/// `can_send` reports whether this bot may post there now.
pub async fn resolve(c: &Connection, b: &Bot, username: &str) -> Result<Destination, Error> {
    let chat = b
        .get_chat(Recipient::ChannelUsername(username.to_owned()))
        .await
        .map_err(|error| match error {
            RequestError::Api(ApiError::ChatNotFound) => Error::Invalid(
                "Telegram has no public channel or group with that @username. Private chats need their chat ID from social_connections.",
            ),
            _ => Error::Platform,
        })?;
    let me = c
        .remote_id
        .parse()
        .map(UserId)
        .map_err(|_| Error::Platform)?;
    let can_send = match b.get_chat_member(chat.id, me).await.map(|m| m.kind) {
        Ok(member) if chat.is_channel() => member.can_post_messages(),
        Ok(ChatMemberKind::Restricted(Restricted {
            is_member,
            can_send_messages,
            ..
        })) => is_member && can_send_messages,
        Ok(ChatMemberKind::Member(_)) => chat
            .permissions()
            .is_none_or(|p| p.contains(ChatPermissions::SEND_MESSAGES)),
        Ok(member) => member.is_privileged(),
        Err(_) => false,
    };
    Ok(Destination {
        id: chat.id.to_string(),
        name: chat.title().unwrap_or(username).to_owned(),
        kind: if chat.is_private() { "direct" } else { "group" }.into(),
        can_send,
    })
}
pub async fn send(b: &Bot, d: &str, body: &str, reply: Option<&str>) -> Result<Value, Error> {
    if body.chars().count() > 4096 {
        return Err(Error::Invalid(
            "Telegram messages are limited to 4096 characters.",
        ));
    }
    let mut request = b.send_message(chat(d)?, body);
    if let Some(reply) = reply {
        request =
            request.reply_parameters(teloxide_core::types::ReplyParameters::new(message(reply)?));
    }
    value(request.await.map_err(|_| Error::Platform)?)
}
pub async fn execute(b: &Bot, cmd: &Command) -> Result<Value, Error> {
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
    b: &Bot,
    mut offset: i64,
    events: mpsc::Sender<Envelope>,
    stop: CancellationToken,
) -> Result<(), Error> {
    loop {
        let updates = tokio::select! {biased;_ = stop.cancelled()=>return Ok(()),r=b.get_updates().offset(i32::try_from(offset).map_err(|_|Error::Platform)?).timeout(POLL_SECONDS).allowed_updates([AllowedUpdate::Message,AllowedUpdate::ChannelPost])=>r.map_err(poll_error)?};
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize)]
    struct Response {
        data: ApiError,
    }

    #[test]
    fn telegram_refusing_the_poll_to_another_receiver_is_a_conflict() {
        for text in [
            "Conflict: can't use getUpdates method while webhook is active; use deleteWebhook to delete the webhook first",
            "Conflict: terminated by other getUpdates request; make sure that only one bot instance is running",
        ] {
            let refused: Response =
                serde_json::from_value(serde_json::json!({ "data": text })).unwrap();
            assert!(
                matches!(
                    poll_error(RequestError::Api(refused.data)),
                    Error::ReceiverConflict
                ),
                "{text}"
            );
        }
        assert!(matches!(
            poll_error(RequestError::Api(ApiError::InvalidToken)),
            Error::Platform
        ));
    }
}
