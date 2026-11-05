use crate::socials::types::*;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use twilight_gateway::{EventTypeFlags, Shard, ShardId, StreamExt};
use twilight_http::request::channel::reaction::RequestReactionType;
use twilight_model::{
    gateway::{Intents, event::Event},
    id::Id,
};

pub(super) fn client(t: &Token) -> twilight_http::Client {
    super::ensure_crypto();
    twilight_http::Client::new(t.0.to_string())
}
fn id<T>(s: &str) -> Result<Id<T>, Error> {
    s.parse::<u64>()
        .ok()
        .and_then(Id::new_checked)
        .ok_or(Error::Invalid("Invalid Discord ID."))
}
macro_rules! model {
    ($call:expr) => {
        serde_json::to_value(
            $call
                .await
                .map_err(|_| Error::Platform)?
                .model()
                .await
                .map_err(|_| Error::Platform)?,
        )
        .map_err(|_| Error::Platform)
    };
}
pub async fn identity(t: &Token) -> Result<Identity, Error> {
    let c = client(t);
    let me = c
        .current_user()
        .await
        .map_err(|_| Error::Platform)?
        .model()
        .await
        .map_err(|_| Error::Platform)?;
    if !me.bot {
        return Err(Error::Invalid("Use a Discord bot token, not a user token."));
    }
    Ok(Identity {
        id: me.id.to_string(),
        display_name: me.name.clone(),
        username: Some(me.name),
    })
}
pub async fn destinations(c: &twilight_http::Client) -> Result<Vec<Destination>, Error> {
    use futures_util::{StreamExt, stream};
    use twilight_model::{channel::ChannelType, guild::Permissions};
    use twilight_util::permission_calculator::PermissionCalculator;
    let me = c
        .current_user()
        .await
        .map_err(|_| Error::Platform)?
        .model()
        .await
        .map_err(|_| Error::Platform)?;
    let guilds = c
        .current_user_guilds()
        .limit(200)
        .await
        .map_err(|_| Error::Platform)?
        .model()
        .await
        .map_err(|_| Error::Platform)?;
    let lists = stream::iter(guilds.into_iter().map(|g| {
        let c = &c;
        let me = me.id;
        async move {
            let (guild, member, channels) = tokio::join!(
                c.guild(g.id),
                c.guild_member(g.id, me),
                c.guild_channels(g.id)
            );
            let guild = guild
                .map_err(|_| Error::Platform)?
                .model()
                .await
                .map_err(|_| Error::Platform)?;
            let member = member
                .map_err(|_| Error::Platform)?
                .model()
                .await
                .map_err(|_| Error::Platform)?;
            let channels = channels
                .map_err(|_| Error::Platform)?
                .model()
                .await
                .map_err(|_| Error::Platform)?;
            let everyone = guild
                .roles
                .iter()
                .find(|r| r.id.get() == g.id.get())
                .map_or(Permissions::empty(), |r| r.permissions);
            let roles = guild
                .roles
                .iter()
                .filter(|r| member.roles.contains(&r.id))
                .map(|r| (r.id, r.permissions))
                .collect::<Vec<_>>();
            Ok::<_, Error>(
                channels
                    .into_iter()
                    .filter_map(|x| {
                        if !matches!(
                            x.kind,
                            ChannelType::GuildText
                                | ChannelType::GuildAnnouncement
                                | ChannelType::PublicThread
                                | ChannelType::PrivateThread
                        ) {
                            return None;
                        }
                        let permissions = PermissionCalculator::new(g.id, me, everyone, &roles)
                            .owner_id(guild.owner_id)
                            .in_channel(
                                x.kind,
                                x.permission_overwrites.as_deref().unwrap_or_default(),
                            );
                        if !permissions.contains(Permissions::VIEW_CHANNEL) {
                            return None;
                        }
                        Some(Destination {
                            id: x.id.to_string(),
                            name: format!("{} / #{}", g.name, x.name.unwrap_or_default()),
                            kind: "channel".into(),
                            can_send: permissions.contains(Permissions::SEND_MESSAGES),
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        }
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;
    let mut result = Vec::new();
    for list in lists {
        result.extend(list?);
        if result.len() > 200 {
            result.truncate(200);
            break;
        }
    }
    Ok(result)
}

pub async fn send(
    c: &twilight_http::Client,
    d: &str,
    body: &str,
    reply: Option<&str>,
) -> Result<Value, Error> {
    if body.chars().count() > 2000 {
        return Err(Error::Invalid(
            "Discord messages are limited to 2000 characters.",
        ));
    }
    let mut request = c.create_message(id(d)?).content(body);
    if let Some(reply) = reply {
        request = request.reply(id(reply)?);
    }
    model!(request)
}
pub async fn execute(c: &twilight_http::Client, cmd: &Command) -> Result<Value, Error> {
    match cmd {
        Command::History {
            destination,
            before,
            limit,
        } => {
            let r = c.channel_messages(id(destination)?).limit(*limit);
            if let Some(before) = before {
                model!(r.before(id(before)?))
            } else {
                model!(r)
            }
        }
        Command::Message {
            destination,
            message_id,
        } => model!(c.message(id(destination)?, id(message_id)?)),
        Command::Chat { destination } => model!(c.channel(id(destination)?)),
        Command::Members { .. } => Err(Error::Unsupported),
        Command::Edit {
            destination,
            message_id,
            body,
        } => model!(
            c.update_message(id(destination)?, id(message_id)?)
                .content(Some(body))
        ),
        Command::Delete {
            destination,
            message_id,
        } => {
            c.delete_message(id(destination)?, id(message_id)?)
                .await
                .map_err(|_| Error::Platform)?;
            Ok(json!({"deleted":true}))
        }
        Command::React {
            destination,
            message_id,
            emoji,
        } => {
            c.create_reaction(
                id(destination)?,
                id(message_id)?,
                &RequestReactionType::Unicode { name: emoji },
            )
            .await
            .map_err(|_| Error::Platform)?;
            Ok(json!({"reacted":true}))
        }
        Command::Pin {
            destination,
            message_id,
        } => {
            c.create_pin(id(destination)?, id(message_id)?)
                .await
                .map_err(|_| Error::Platform)?;
            Ok(json!({"pinned":true}))
        }
        Command::Unpin {
            destination,
            message_id,
        } => {
            c.delete_pin(id(destination)?, id(message_id)?)
                .await
                .map_err(|_| Error::Platform)?;
            Ok(json!({"unpinned":true}))
        }
        Command::CreateThread {
            destination,
            message_id,
            name,
        } => model!(c.create_thread_from_message(id(destination)?, id(message_id)?, name)),
        _ => Err(Error::Unsupported),
    }
}
pub async fn listen(
    c: &Connection,
    t: &Token,
    events: mpsc::Sender<Envelope>,
    stop: CancellationToken,
) -> Result<(), Error> {
    super::ensure_crypto();
    let mut shard = Shard::new(
        ShardId::ONE,
        t.0.to_string(),
        Intents::GUILD_MESSAGES
            | Intents::DIRECT_MESSAGES
            | if c.message_content {
                Intents::MESSAGE_CONTENT
            } else {
                Intents::empty()
            },
    );
    loop {
        let event = tokio::select! {biased;_=stop.cancelled()=>return Ok(()),r=shard.next_event(EventTypeFlags::MESSAGE_CREATE)=>r.ok_or(Error::Platform)?.map_err(|_|Error::Platform)?};
        if let Event::MessageCreate(m) = event {
            let incoming = Incoming {
                event_id: m.id.to_string(),
                destination: Destination {
                    id: m.channel_id.to_string(),
                    name: m.channel_id.to_string(),
                    kind: if m.guild_id.is_some() {
                        "channel"
                    } else {
                        "direct"
                    }
                    .into(),
                    can_send: true,
                },
                message_id: m.id.to_string(),
                author_id: m.author.id.to_string(),
                body: m.content.chars().take(16000).collect(),
                mentioned: m.mentions.iter().any(|u| u.id.to_string() == c.remote_id),
                direct: m.guild_id.is_none(),
                from_bot: m.author.bot,
            };
            let (ack, stored) = tokio::sync::oneshot::channel();
            tokio::select! {biased;_=stop.cancelled()=>return Ok(()),r=events.send(Envelope{incoming,next_offset:0,ack})=>r.map_err(|_|Error::Platform)?};
            tokio::select! {biased;_=stop.cancelled()=>return Ok(()),r=stored=>r.map_err(|_|Error::Platform)?};
        }
    }
}
