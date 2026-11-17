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
    files: &[SendFile],
) -> Result<Value, Error> {
    let client = link.client()?;
    let chat = jid(destination)?;
    let quote = match reply {
        None => None,
        Some(reply) => {
            let (author, quoted) =
                link.author(&chat.to_string(), reply)
                    .await?
                    .ok_or(Error::Invalid(
                        "The replied message was not observed. Use a message ID from history.",
                    ))?;
            Some(build_quote_context(
                reply,
                author,
                &wa::Message::text(quoted),
            ))
        }
    };
    if files.is_empty() {
        let message = match quote {
            None => wa::Message::text(body),
            Some(context) => wa::Message::text_with_context(body, context),
        };
        link.pace().await;
        let sent = client
            .send_message(chat, message)
            .await
            .map_err(|_| Error::Uncertain)?;
        return Ok(json!({"id": sent.message_id.as_str()}));
    }
    super::super::send_parts(super::super::parts(body, files), |part, first| {
        let client = client.clone();
        let chat = chat.clone();
        let quote = quote.clone().filter(|_| first);
        async move {
            let message = match part {
                super::super::Part::Text(text) => match quote {
                    None => wa::Message::text(text),
                    Some(context) => wa::Message::text_with_context(text, context),
                },
                super::super::Part::File(file, caption) => {
                    media_message(&client, file, caption, quote).await?
                }
            };
            link.pace().await;
            let sent = client
                .send_message(chat, message)
                .await
                .map_err(|_| Error::Uncertain)?;
            Ok(json!({"id": sent.message_id.as_str()}))
        }
    })
    .await
}

/// An image with its caption, or any other file as a document. Nothing
/// reached the chat when the upload fails.
async fn media_message(
    client: &Client,
    file: &SendFile,
    caption: Option<&str>,
    quote: Option<wa::ContextInfo>,
) -> Result<wa::Message, Error> {
    use whatsapp_rust::buffa::MessageField;
    use whatsapp_rust::download::MediaType;
    let kind = if file.is_image() {
        MediaType::Image
    } else {
        MediaType::Document
    };
    let upload = client
        .upload(file.data.to_vec(), kind, Default::default())
        .await
        .map_err(|_| Error::Platform)?;
    let context = quote.map_or_else(MessageField::none, MessageField::some);
    let mut message = wa::Message::default();
    if file.is_image() {
        let mut image = wa::message::ImageMessage::default();
        image.url = Some(upload.url);
        image.direct_path = Some(upload.direct_path);
        image.media_key = Some(upload.media_key.to_vec());
        image.file_sha256 = Some(upload.file_sha256.to_vec());
        image.file_enc_sha256 = Some(upload.file_enc_sha256.to_vec());
        image.file_length = Some(upload.file_length);
        image.media_key_timestamp = Some(upload.media_key_timestamp);
        image.mimetype = Some(file.media_type.clone());
        image.caption = caption.map(str::to_owned);
        image.context_info = context;
        message.image_message = MessageField::some(image);
    } else {
        let mut document = wa::message::DocumentMessage::default();
        document.url = Some(upload.url);
        document.direct_path = Some(upload.direct_path);
        document.media_key = Some(upload.media_key.to_vec());
        document.file_sha256 = Some(upload.file_sha256.to_vec());
        document.file_enc_sha256 = Some(upload.file_enc_sha256.to_vec());
        document.file_length = Some(upload.file_length);
        document.media_key_timestamp = Some(upload.media_key_timestamp);
        document.mimetype = Some(file.media_type.clone());
        document.file_name = Some(file.name.clone());
        document.title = Some(file.name.clone());
        document.caption = caption.map(str::to_owned);
        document.context_info = context;
        message.document_message = MessageField::some(document);
    }
    Ok(message)
}
