mod mime;
mod session;

use mime::{ThreadHeaders, addresses, compose, compose_forward, reply, smtp_address, thread};
use session::{Imap, Smtp};

use std::borrow::Cow;
use std::io;
use std::num::NonZeroU32;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use io_imap::client::{ImapClientAsync, ImapClientError};
use io_imap::codec::{fragmentizer::Fragmentizer, imap_types::core::TagGenerator};
use io_imap::coroutine::{ImapCoroutine, ImapCoroutineState, ImapYield};
use io_imap::rfc2177::idle::{ImapIdle, ImapIdleOptions, ImapIdleYield};
use io_imap::rfc3501::{
    append::ImapMessageAppendOptions, fetch::ImapMessageFetchOptions,
    search::ImapMessageSearchOptions, store::ImapMessageStoreOptions,
};
use io_imap::session::{
    ImapSessionOpen, ImapSessionOpenOptions, ImapSessionOpenYield, ImapSessionTransport,
};
use io_imap::types::{
    core::Vec1,
    fetch::{MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName},
    flag::{Flag, StoreType},
    mailbox::{ListMailbox, Mailbox},
    response::Capability,
    search::SearchKey,
    sequence::SequenceSet,
};
use io_sasl::{mechanism::Sasl, rfc4616::plain::SaslPlainCreds, xoauth2::SaslXoauth2Creds};
use io_smtp::client::{SmtpClientAsync, SmtpClientError};
use io_smtp::coroutine::{SmtpCoroutine, SmtpCoroutineState, SmtpYield};
use io_smtp::rfc5321::{
    SmtpDomain, SmtpEhloDomain, SmtpForwardPath, SmtpLocalPart, SmtpMailbox, SmtpReversePath,
};
use io_smtp::session::{
    SmtpSessionOpen, SmtpSessionOpenOptions, SmtpSessionOpenYield, SmtpSessionTransport,
};
use mail_builder::MessageBuilder;
use mail_parser::{MessageParser, MimeHeaders};
use rustls::{ClientConfig, pki_types::ServerName};
use rustls_platform_verifier::ConfigVerifierExt;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::{TlsConnector, client::TlsStream};
use tokio_util::sync::CancellationToken;

use super::types::*;

const MESSAGE_LIMIT: usize = 2 * 1024 * 1024;
const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(45);
const IDLE_TIMEOUT: Duration = Duration::from_secs(29 * 60);

pub struct Pimalaya;

fn failure(_: impl std::fmt::Debug) -> MailError {
    MailError::new("mail_transport_failed")
}
fn invalid(_: impl std::fmt::Debug) -> MailError {
    MailError::new("invalid_mail_request")
}
fn mailbox(s: &str) -> Result<Mailbox<'static>, MailError> {
    Mailbox::try_from(s.to_owned()).map_err(invalid)
}
fn sequence(uid: u32) -> Result<SequenceSet, MailError> {
    NonZeroU32::new(uid)
        .map(SequenceSet::from)
        .ok_or_else(|| MailError::new("invalid_message_uid"))
}
fn items(names: Vec<MessageDataItemName<'static>>) -> MacroOrMessageDataItemNames<'static> {
    MacroOrMessageDataItemNames::MessageDataItemNames(names)
}
impl Imap {
    async fn select_ref(&mut self, message: &MessageRef) -> Result<(), MailError> {
        let data = self
            .select(mailbox(&message.folder)?, Default::default())
            .await
            .map_err(failure)?;
        if message.uid == 0 || data.uid_validity.map(NonZeroU32::get) != Some(message.uid_validity)
        {
            return Err(MailError::new("stale_message_reference"));
        }
        Ok(())
    }
    async fn body(&mut self, message: &MessageRef) -> Result<Vec<u8>, MailError> {
        self.select_ref(message).await?;
        let opts = ImapMessageFetchOptions {
            uid: true,
            ..Default::default()
        };
        let meta = self
            .fetch(
                sequence(message.uid)?,
                items(vec![
                    MessageDataItemName::Uid,
                    MessageDataItemName::Rfc822Size,
                ]),
                opts.clone(),
            )
            .await
            .map_err(failure)?;
        let size = meta
            .values()
            .flat_map(|row| row.as_ref().iter())
            .find_map(|i| {
                if let MessageDataItem::Rfc822Size(n) = i {
                    Some(*n as usize)
                } else {
                    None
                }
            })
            .ok_or_else(|| MailError::new("message_not_found"))?;
        if size > MESSAGE_LIMIT {
            return Err(MailError::new("mail_message_too_large"));
        }
        let data = self
            .fetch(
                sequence(message.uid)?,
                items(vec![
                    MessageDataItemName::Uid,
                    MessageDataItemName::BodyExt {
                        section: None,
                        partial: None,
                        peek: true,
                    },
                ]),
                opts,
            )
            .await
            .map_err(failure)?;
        for row in data.values() {
            let uid = row.as_ref().iter().find_map(|i| {
                if let MessageDataItem::Uid(uid) = i {
                    Some(uid.get())
                } else {
                    None
                }
            });
            if uid != Some(message.uid) {
                continue;
            }
            for item in row.as_ref().iter() {
                if let MessageDataItem::BodyExt { data, .. } = item
                    && let Some(data) = &data.0
                {
                    let bytes = data.as_ref();
                    if bytes.len() > MESSAGE_LIMIT {
                        return Err(MailError::new("mail_message_too_large"));
                    }
                    return Ok(bytes.to_vec());
                }
            }
        }
        Err(MailError::new("message_not_found"))
    }
    async fn flag(
        &mut self,
        message: &MessageRef,
        flag: Flag<'static>,
        add: bool,
    ) -> Result<Value, MailError> {
        self.select_ref(message).await?;
        self.store(
            sequence(message.uid)?,
            if add {
                StoreType::Add
            } else {
                StoreType::Remove
            },
            vec![flag],
            ImapMessageStoreOptions { uid: true },
        )
        .await
        .map_err(failure)?;
        Ok(json!({"message":message,"updated":true}))
    }
    async fn move_message(
        &mut self,
        message: &MessageRef,
        folder: &str,
    ) -> Result<Value, MailError> {
        self.select_ref(message).await?;
        let result = if self.has("MOVE") {
            self.r#move(
                sequence(message.uid)?,
                mailbox(folder)?,
                io_imap::rfc6851::r#move::ImapMessageMoveOptions { uid: true },
            )
            .await
            .map_err(failure)?
        } else if self.has("UIDPLUS") {
            let copied = self
                .copy(
                    sequence(message.uid)?,
                    mailbox(folder)?,
                    io_imap::rfc3501::copy::ImapMessageCopyOptions { uid: true },
                )
                .await
                .map_err(failure)?;
            self.flag(message, Flag::Deleted, true).await?;
            self.uid_expunge(sequence(message.uid)?)
                .await
                .map_err(failure)?;
            copied
        } else {
            return Err(MailError::new("mail_move_not_supported"));
        };
        let moved = result.as_ref().and_then(|(validity, _, uids)| {
            uids.first().map(|uid| MessageRef {
                folder: folder.into(),
                uid_validity: *validity,
                uid: *uid,
            })
        });
        Ok(json!({"moved":true,"destination":folder,"message":moved}))
    }
}
#[async_trait::async_trait]
impl MailTransport for Pimalaya {
    async fn execute(
        &self,
        config: &MailConfig,
        auth: &Auth,
        command: &MailCommand,
    ) -> Result<Value, MailError> {
        timeout(Duration::from_secs(120), execute(config, auth, command))
            .await
            .map_err(|_| MailError::new("mail_timeout"))?
    }
    async fn scan(
        &self,
        config: &MailConfig,
        auth: &Auth,
        cursor: Option<&Cursor>,
    ) -> Result<Scan, MailError> {
        let mut client = Imap::open(&config.imap, auth).await?;
        scan(&mut client, cursor).await
    }
    async fn wait(
        &self,
        config: &MailConfig,
        auth: &Auth,
        cursor: &Cursor,
        stop: CancellationToken,
    ) -> Result<(), MailError> {
        wait_for_mail(&config.imap, auth, cursor, stop, &session::tls_connector()?).await
    }
}

async fn wait_for_mail(
    endpoint: &Endpoint,
    auth: &Auth,
    cursor: &Cursor,
    stop: CancellationToken,
    connector: &TlsConnector,
) -> Result<(), MailError> {
    tokio::select! {
        _=stop.cancelled()=>Ok(()),
        result=async {
            let mut client=timeout(COMMAND_TIMEOUT, Imap::connect(endpoint,auth,connector)).await.map_err(|_|MailError::new("mail_timeout"))??;
            if !client.has("IDLE") {return Err(MailError::new("mail_idle_not_supported"));}
            let selected = client.examine(mailbox("INBOX")?,Default::default()).await.map_err(failure)?;
            let validity = selected.uid_validity.ok_or_else(|| MailError::new("mail_missing_uid_validity"))?.get();
            let next = selected.uid_next.ok_or_else(|| MailError::new("mail_missing_uid_next"))?.get();
            if validity != cursor.uid_validity || next.saturating_sub(1) > cursor.last_uid { return Ok(()); }
            client.idle().await
        }=>result,
    }
}

impl Imap {
    async fn idle(&mut self) -> Result<(), MailError> {
        let mut coroutine = ImapIdle::new(
            Arc::new(AtomicBool::new(false)),
            ImapIdleOptions {
                timeout: Some(IDLE_TIMEOUT),
            },
        );
        let mut arg: Option<Vec<u8>> = None;
        let mut bytes = [0; 16384];
        let mut budget = RESPONSE_LIMIT;
        let idle = async {
            loop {
                let state = coroutine.resume(&mut self.fragmentizer, arg.as_deref());
                arg = None;
                match state {
                    ImapCoroutineState::Complete(result) => return result.map_err(failure),
                    ImapCoroutineState::Yielded(ImapIdleYield::Event(_)) => return Ok(()),
                    ImapCoroutineState::Yielded(ImapIdleYield::WantsTag) => {
                        arg = Some(self.tags.generate().as_ref().as_bytes().to_vec())
                    }
                    ImapCoroutineState::Yielded(ImapIdleYield::WantsWrite(bytes)) => {
                        self.socket.write(&bytes).await.map_err(failure)?
                    }
                    ImapCoroutineState::Yielded(ImapIdleYield::WantsRead) => {
                        let n = self.socket.read(&mut bytes).await.map_err(failure)?;
                        budget = budget
                            .checked_sub(n)
                            .ok_or_else(|| MailError::new("mail_response_too_large"))?;
                        arg = Some(bytes[..n].to_vec());
                    }
                }
            }
        };
        timeout(IDLE_TIMEOUT, idle).await.unwrap_or(Ok(()))
    }
}

async fn scan(client: &mut Imap, cursor: Option<&Cursor>) -> Result<Scan, MailError> {
    let data = client
        .examine(mailbox("INBOX")?, Default::default())
        .await
        .map_err(failure)?;
    let uid_validity = data
        .uid_validity
        .ok_or_else(|| MailError::new("mail_missing_uid_validity"))?
        .get();
    let last = data
        .uid_next
        .ok_or_else(|| MailError::new("mail_missing_uid_next"))?
        .get()
        - 1;
    let Some(previous) = cursor.filter(|c| c.uid_validity == uid_validity) else {
        return Ok(Scan {
            cursor: Cursor {
                uid_validity,
                last_uid: last,
            },
            arrivals: Vec::new(),
            more: false,
        });
    };
    if previous.last_uid >= last {
        return Ok(Scan {
            cursor: previous.clone(),
            arrivals: Vec::new(),
            more: false,
        });
    }
    let end = previous.last_uid.saturating_add(100).min(last);
    let range = SequenceSet::from(
        NonZeroU32::new(previous.last_uid + 1).unwrap()..=NonZeroU32::new(end).unwrap(),
    );
    let criteria = Vec1::try_from(vec![SearchKey::Uid(range)]).expect("one search key");
    let found = client
        .search(criteria, ImapMessageSearchOptions { uid: true })
        .await
        .map_err(failure)?;
    let arrivals = found
        .into_iter()
        .filter(|uid| uid.get() > previous.last_uid && uid.get() <= end)
        .map(|uid| Arrival {
            message: MessageRef {
                folder: "INBOX".into(),
                uid_validity,
                uid: uid.get(),
            },
        })
        .collect();
    Ok(Scan {
        cursor: Cursor {
            uid_validity,
            last_uid: end,
        },
        arrivals,
        more: end < last,
    })
}

async fn execute(
    config: &MailConfig,
    auth: &Auth,
    command: &MailCommand,
) -> Result<Value, MailError> {
    if let MailCommand::Send { draft } = command
        && draft.reply_to.is_none()
    {
        return send(config, auth, draft, None).await;
    }
    let mut client = Imap::open(&config.imap, auth).await?;
    match command {
        MailCommand::Folders => {
            let list = client
                .list(mailbox("")?, ListMailbox::try_from("*").map_err(invalid)?)
                .await
                .map_err(failure)?;
            Ok(
                json!({"folders":list.into_iter().take(500).map(|(name,separator,attributes)|json!({"name":mailbox_name(&name),"separator":separator.map(|c|c.inner().to_string()),"attributes":attributes.iter().map(ToString::to_string).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
            )
        }
        MailCommand::Search {
            folder,
            query,
            limit,
        } => {
            if *limit == 0 || *limit > 100 {
                return Err(MailError::new("invalid_mail_limit"));
            }
            let selected = client
                .examine(mailbox(folder)?, Default::default())
                .await
                .map_err(failure)?;
            let validity = selected
                .uid_validity
                .ok_or_else(|| MailError::new("mail_missing_uid_validity"))?
                .get();
            let key = if let Some(query) = query {
                if query.len() > 2048 {
                    return Err(MailError::new("mail_query_too_large"));
                }
                SearchKey::Text(query.clone().try_into().map_err(invalid)?)
            } else {
                SearchKey::All
            };
            let mut found = client
                .search(
                    Vec1::try_from(vec![key]).expect("one search key"),
                    ImapMessageSearchOptions { uid: true },
                )
                .await
                .map_err(failure)?;
            found.sort_unstable();
            found.dedup();
            let truncated = found.len() > *limit;
            let chosen: Vec<_> = found.into_iter().rev().take(*limit).collect();
            if chosen.is_empty() {
                return Ok(json!({"messages":[],"truncated":false}));
            }
            let set = SequenceSet::try_from(
                chosen
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
                    .as_str(),
            )
            .map_err(invalid)?;
            let rows = client
                .fetch(
                    set,
                    items(vec![
                        MessageDataItemName::Uid,
                        MessageDataItemName::Envelope,
                        MessageDataItemName::Flags,
                    ]),
                    ImapMessageFetchOptions {
                        uid: true,
                        ..Default::default()
                    },
                )
                .await
                .map_err(failure)?;
            let mut messages = Vec::new();
            for row in rows.values() {
                let Some(uid) = row.as_ref().iter().find_map(|i| {
                    if let MessageDataItem::Uid(id) = i {
                        Some(id.get())
                    } else {
                        None
                    }
                }) else {
                    continue;
                };
                if !chosen.iter().any(|id| id.get() == uid) {
                    continue;
                }
                let mut value =
                    json!({"message":MessageRef{folder:folder.clone(),uid_validity:validity,uid}});
                for item in row.as_ref().iter() {
                    match item {
                        MessageDataItem::Envelope(e) => {
                            value["subject"] = json!(nstring(&e.subject));
                            value["from"] = json!(
                                e.from
                                    .iter()
                                    .map(|a| format!(
                                        "{}@{}",
                                        nstring(&a.mailbox).unwrap_or_default(),
                                        nstring(&a.host).unwrap_or_default()
                                    ))
                                    .collect::<Vec<_>>()
                            );
                        }
                        MessageDataItem::Flags(flags) => {
                            value["flags"] = json!(
                                flags
                                    .iter()
                                    .map(|f| match f {
                                        io_imap::types::flag::FlagFetch::Flag(f) => f.to_string(),
                                        io_imap::types::flag::FlagFetch::Recent =>
                                            "\\Recent".into(),
                                    })
                                    .collect::<Vec<_>>()
                            )
                        }
                        _ => (),
                    }
                }
                messages.push(value);
            }
            Ok(json!({"messages":messages,"truncated":truncated,"source":"external_email"}))
        }
        MailCommand::Read { message } => {
            let bytes = client.body(message).await?;
            let parsed = MessageParser::default()
                .parse(&bytes)
                .ok_or_else(|| MailError::new("invalid_email_message"))?;
            let body = parsed.body_text(0).unwrap_or_default();
            let text: String = body.chars().take(32000).collect();
            Ok(
                json!({"message":message,"source":"external_email","subject":parsed.subject(),"from":addresses(parsed.from()),"to":addresses(parsed.to()),"cc":addresses(parsed.cc()),"reply_to":addresses(parsed.reply_to()),"body":text,"truncated":body.chars().count()>32000,"attachments":parsed.attachments().enumerate().map(|(part,a)|json!({"part":part,"name":a.attachment_name(),"size":a.contents().len()})).collect::<Vec<_>>()}),
            )
        }
        MailCommand::Attachment { message, part } => {
            let bytes = client.body(message).await?;
            let parsed = MessageParser::default()
                .parse(&bytes)
                .ok_or_else(|| MailError::new("invalid_email_message"))?;
            let attachment = parsed
                .attachments()
                .nth(*part)
                .ok_or_else(|| MailError::new("attachment_not_found"))?;
            if attachment.contents().len() > 256 * 1024 {
                return Err(MailError::new("mail_attachment_too_large"));
            }
            Ok(
                json!({"name":attachment.attachment_name(),"data_base64":STANDARD.encode(attachment.contents()),"source":"external_email"}),
            )
        }
        MailCommand::SetRead { message, read } => client.flag(message, Flag::Seen, *read).await,
        MailCommand::SetFlagged { message, flagged } => {
            client.flag(message, Flag::Flagged, *flagged).await
        }
        MailCommand::Move { message, folder } => client.move_message(message, folder).await,
        MailCommand::Archive { message } => {
            client.move_message(message, &config.archive_folder).await
        }
        MailCommand::Trash { message } => client.move_message(message, &config.trash_folder).await,
        MailCommand::Delete { message } => {
            if !client.has("UIDPLUS") {
                return Err(MailError::new("mail_uid_expunge_not_supported"));
            }
            client.flag(message, Flag::Deleted, true).await?;
            client
                .uid_expunge(sequence(message.uid)?)
                .await
                .map_err(failure)?;
            Ok(json!({"message":message,"deleted":true}))
        }
        MailCommand::Send { draft } => {
            let thread = match &draft.reply_to {
                Some(message) => {
                    let raw = client.body(message).await?;
                    let parsed = MessageParser::default()
                        .parse(&raw)
                        .ok_or_else(|| MailError::new("invalid_email_message"))?;
                    Some(thread(&parsed))
                }
                None => None,
            };
            send(config, auth, draft, thread).await
        }
        MailCommand::Reply {
            message,
            body,
            all,
            send: SendNow,
        } => {
            let raw = client.body(message).await?;
            let parsed = MessageParser::default()
                .parse(&raw)
                .ok_or_else(|| MailError::new("invalid_email_message"))?;
            let (draft, headers) = reply(&parsed, &config.address, body, *all)?;
            send(config, auth, &draft, Some(headers)).await
        }
        MailCommand::Forward {
            message,
            to,
            body,
            send: SendNow,
        } => {
            let raw = client.body(message).await?;
            let parsed = MessageParser::default()
                .parse(&raw)
                .ok_or_else(|| MailError::new("invalid_email_message"))?;
            let draft = Draft {
                to: to.clone(),
                cc: Vec::new(),
                bcc: Vec::new(),
                attachments: Vec::new(),
                reply_to: None,
                subject: format!("Fwd: {}", parsed.subject().unwrap_or_default()),
                body: format!(
                    "{body}\n\n---------- Forwarded message ----------\nFrom: {}\nSubject: {}\n\n{}",
                    addresses(parsed.from()).join(", "),
                    parsed.subject().unwrap_or_default(),
                    parsed.body_text(0).unwrap_or_default()
                ),
            };
            let bytes = compose_forward(&config.address, &draft, &raw)?;
            send_prepared(config, auth, &draft, bytes).await
        }
        MailCommand::CreateFolder { folder } => {
            client.create(mailbox(folder)?).await.map_err(failure)?;
            Ok(json!({"created":true}))
        }
        MailCommand::RenameFolder { folder, name } => {
            client
                .rename(mailbox(folder)?, mailbox(name)?)
                .await
                .map_err(failure)?;
            Ok(json!({"renamed":true}))
        }
        MailCommand::DeleteFolder { folder } => {
            client.delete(mailbox(folder)?).await.map_err(failure)?;
            Ok(json!({"deleted":true}))
        }
    }
}

fn mailbox_name(value: &Mailbox<'_>) -> String {
    match value {
        Mailbox::Inbox => "INBOX".into(),
        Mailbox::Other(value) => String::from_utf8_lossy(value.inner().as_ref()).into_owned(),
    }
}

fn nstring(value: &io_imap::types::core::NString<'_>) -> Option<String> {
    value.0.as_ref().map(|s| {
        String::from_utf8_lossy(s.as_ref())
            .chars()
            .take(1024)
            .collect()
    })
}
async fn send(
    config: &MailConfig,
    auth: &Auth,
    draft: &Draft,
    reply: Option<ThreadHeaders>,
) -> Result<Value, MailError> {
    let message = compose(&config.address, draft, reply)?;
    send_prepared(config, auth, draft, message).await
}

async fn send_prepared(
    config: &MailConfig,
    auth: &Auth,
    draft: &Draft,
    message: Vec<u8>,
) -> Result<Value, MailError> {
    let from = SmtpReversePath::SmtpMailbox(smtp_address(&config.address)?);
    let recipients = draft
        .to
        .iter()
        .chain(&draft.cc)
        .chain(&draft.bcc)
        .map(|a| smtp_address(a).map(SmtpForwardPath))
        .collect::<Result<Vec<_>, _>>()?;
    let mut client = Smtp::open(&config.smtp, auth).await?;
    client
        .send(from, recipients, message.clone(), Default::default())
        .await
        .map_err(failure)?;
    let sent_copy_saved = if matches!(config.provider, Provider::Gmail | Provider::Microsoft) {
        None
    } else {
        Some(
            async {
                let mut imap = Imap::open(&config.imap, auth).await?;
                imap.append(
                    mailbox(&config.sent_folder)?,
                    &message,
                    ImapMessageAppendOptions {
                        flags: vec![Flag::Seen],
                        ..Default::default()
                    },
                )
                .await
                .map_err(failure)?;
                Ok::<_, MailError>(())
            }
            .await
            .is_ok(),
        )
    };
    Ok(json!({"sent":true, "sent_copy_saved":sent_copy_saved}))
}

#[cfg(test)]
mod tests;
