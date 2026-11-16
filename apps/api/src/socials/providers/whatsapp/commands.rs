//! What a bot does through a linked device: list chats, run actions, send.

use super::*;

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
