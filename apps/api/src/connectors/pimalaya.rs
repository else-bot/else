mod mime;
mod session;

use mime::{ThreadHeaders, addresses, compose, compose_forward, reply, smtp_address, thread, view};
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
use io_smtp::rfc5321::data::{SmtpData, SmtpDataError};
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

/// The largest whole message read. Larger ones still list in search.
const MESSAGE_LIMIT: usize = 64 * 1024 * 1024;
/// The largest IMAP literal one response may carry, which bounds a fetch chunk.
const LITERAL_LIMIT: usize = 2 * 1024 * 1024;
/// Bytes of a message fetched per command, so each response stays under
/// `LITERAL_LIMIT` and `RESPONSE_LIMIT`.
const FETCH_CHUNK: u32 = 1024 * 1024;
/// The largest decoded attachment handed out after the owner approves it.
const ATTACHMENT_LIMIT: usize = 32 * 1024 * 1024;
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
        self.fetch_message(message).await.map(|(bytes, _)| bytes)
    }
    /// The raw message and when the server received it.
    async fn fetch_message(
        &mut self,
        message: &MessageRef,
    ) -> Result<(Vec<u8>, Option<String>), MailError> {
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
                    MessageDataItemName::InternalDate,
                ]),
                opts.clone(),
            )
            .await
            .map_err(failure)?;
        let meta_items = || meta.values().flat_map(|row| row.as_ref().iter());
        let size = meta_items()
            .find_map(|i| {
                if let MessageDataItem::Rfc822Size(n) = i {
                    Some(*n as usize)
                } else {
                    None
                }
            })
            .ok_or_else(|| MailError::new("message_not_found"))?;
        let received_at = meta_items().find_map(internal_date).map(rfc3339);
        if size > MESSAGE_LIMIT {
            return Err(MailError::new("mail_message_too_large").detail(
                "the message is over 64 MiB, so neither its body nor its attachments can be read",
            ));
        }
        let mut bytes = Vec::with_capacity(size);
        loop {
            let data = self
                .fetch(
                    sequence(message.uid)?,
                    items(vec![
                        MessageDataItemName::Uid,
                        MessageDataItemName::BodyExt {
                            section: None,
                            partial: Some((
                                u32::try_from(bytes.len()).map_err(invalid)?,
                                NonZeroU32::new(FETCH_CHUNK).expect("nonzero chunk"),
                            )),
                            peek: true,
                        },
                    ]),
                    opts.clone(),
                )
                .await
                .map_err(failure)?;
            let chunk = body_chunk(&data, message.uid)
                .ok_or_else(|| MailError::new("message_not_found"))?;
            // Servers may count RFC822.SIZE slightly differently, so only a
            // chunk that stops dead short of it is treated as a cut-off read.
            if chunk.is_empty() && bytes.len() < size {
                return Err(MailError::new("mail_message_incomplete")
                    .detail("the server stopped sending the message; try again"));
            }
            if bytes.len() + chunk.len() > MESSAGE_LIMIT {
                return Err(MailError::new("mail_message_too_large")
                    .detail("the message is over 64 MiB, so neither its body nor its attachments can be read"));
            }
            bytes.extend_from_slice(chunk);
            if chunk.len() < FETCH_CHUNK as usize {
                return Ok((bytes, received_at));
            }
        }
    }
    async fn search_messages(
        &mut self,
        folder: &str,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Value, MailError> {
        if limit == 0 || limit > 100 {
            return Err(MailError::new("invalid_mail_limit"));
        }
        let selected = self
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
            SearchKey::Text(query.to_owned().try_into().map_err(invalid)?)
        } else {
            SearchKey::All
        };
        let mut found = self
            .search(
                Vec1::try_from(vec![key]).expect("one search key"),
                ImapMessageSearchOptions { uid: true },
            )
            .await
            .map_err(failure)?;
        found.sort_unstable();
        found.dedup();
        let truncated = found.len() > limit;
        let chosen: Vec<_> = found.into_iter().rev().take(limit).collect();
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
        let rows = self
            .fetch(
                set,
                items(vec![
                    MessageDataItemName::Uid,
                    MessageDataItemName::Envelope,
                    MessageDataItemName::Flags,
                    MessageDataItemName::InternalDate,
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
            let received = row.as_ref().iter().find_map(internal_date);
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
            let mut value = json!({"message":MessageRef{folder:folder.to_owned(),uid_validity:validity,uid},"date":null,"received_at":received.map(rfc3339)});
            for item in row.as_ref().iter() {
                match item {
                    MessageDataItem::Envelope(e) => {
                        value["subject"] = json!(nstring(&e.subject));
                        value["date"] = json!(header_date(nstring(&e.date).as_deref()));
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
                                    io_imap::types::flag::FlagFetch::Recent => "\\Recent".into(),
                                })
                                .collect::<Vec<_>>()
                        )
                    }
                    _ => (),
                }
            }
            messages.push((received, uid, value));
        }
        messages.sort_unstable_by_key(|(received, uid, _)| std::cmp::Reverse((*received, *uid)));
        let messages: Vec<_> = messages.into_iter().map(|(_, _, value)| value).collect();
        Ok(json!({"messages":messages,"truncated":truncated,"source":"external_email"}))
    }
    async fn read_message(&mut self, message: &MessageRef) -> Result<Value, MailError> {
        let (bytes, received_at) = self.fetch_message(message).await?;
        let parsed = MessageParser::default()
            .parse(&bytes)
            .ok_or_else(|| MailError::new("invalid_email_message"))?;
        let body = parsed.body_text(0).unwrap_or_default();
        let text: String = body.chars().take(32000).collect();
        Ok(
            json!({"message":message,"source":"external_email","date":header_date(parsed.header_raw("Date")),"received_at":received_at,"subject":parsed.subject(),"from":addresses(parsed.from()),"to":addresses(parsed.to()),"cc":addresses(parsed.cc()),"reply_to":addresses(parsed.reply_to()),"body":text,"truncated":body.chars().count()>32000,"attachments":parsed.attachments().enumerate().map(|(part,a)|json!({"part":part,"name":a.attachment_name(),"media_type":media_type(a),"size":a.contents().len()})).collect::<Vec<_>>()}),
        )
    }
    async fn view_message(&mut self, message: &MessageRef) -> Result<Value, MailError> {
        let (bytes, received_at) = self.fetch_message(message).await?;
        let parsed = MessageParser::default()
            .parse(&bytes)
            .ok_or_else(|| MailError::new("invalid_email_message"))?;
        Ok(view(&parsed, message, received_at))
    }
    /// Each requested attachment's name, type and size, with its bytes
    /// unless it is missing or over `ATTACHMENT_LIMIT`.
    async fn open_attachments(
        &mut self,
        message: &MessageRef,
        parts: &[usize],
    ) -> Result<Value, MailError> {
        let bytes = self.body(message).await?;
        let parsed = MessageParser::default()
            .parse(&bytes)
            .ok_or_else(|| MailError::new("invalid_email_message"))?;
        let found: Vec<_> = parsed.attachments().collect();
        let files = parts
            .iter()
            .map(|&part| {
                let Some(file) = found.get(part) else {
                    return json!({"part":part,"error":"attachment_not_found"});
                };
                let mut value = json!({"part":part,"name":file.attachment_name(),
                    "media_type":media_type(file),"size":file.contents().len()});
                if file.contents().len() > ATTACHMENT_LIMIT {
                    value["error"] = json!("mail_attachment_too_large");
                } else {
                    value["data_base64"] = json!(STANDARD.encode(file.contents()));
                }
                value
            })
            .collect::<Vec<_>>();
        Ok(json!({"attachments":files,"subject":parsed.subject(),
            "from":addresses(parsed.from()),"source":"external_email"}))
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
    async fn verify(&self, config: &MailConfig, auth: &Auth) -> Result<(), MailError> {
        tokio::try_join!(
            Imap::open(&config.imap, auth),
            Smtp::open(&config.smtp, auth)
        )
        .map(|_| ())
    }
    async fn execute(
        &self,
        config: &MailConfig,
        auth: &Auth,
        command: &MailCommand,
    ) -> Result<Value, MailError> {
        let mut phase = SendPhase::Prepare;
        let result = timeout(
            Duration::from_secs(120),
            execute(config, auth, command, &mut phase),
        )
        .await
        .unwrap_or_else(|_| Err(MailError::new("mail_timeout")));
        if command.sends() {
            result.map_err(|error| error.at(phase))
        } else {
            result
        }
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

/// Runs one command. A send advances `phase` as it goes, so a caller that
/// times it out still knows how far it got.
async fn execute(
    config: &MailConfig,
    auth: &Auth,
    command: &MailCommand,
    phase: &mut SendPhase,
) -> Result<Value, MailError> {
    if let MailCommand::Send { draft } = command
        && draft.reply_to.is_none()
    {
        return send(config, auth, draft, None, phase).await;
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
            client
                .search_messages(folder, query.as_deref(), *limit)
                .await
        }
        MailCommand::Read { message } => client.read_message(message).await,
        MailCommand::View { message } => client.view_message(message).await,
        MailCommand::OpenAttachments { message, parts } => {
            client.open_attachments(message, parts).await
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
            send(config, auth, draft, thread, phase).await
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
            send(config, auth, &draft, Some(headers), phase).await
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
                message_id: None,
                subject: format!("Fwd: {}", parsed.subject().unwrap_or_default()),
                body: format!(
                    "{body}\n\n---------- Forwarded message ----------\nFrom: {}\nSubject: {}\n\n{}",
                    addresses(parsed.from()).join(", "),
                    parsed.subject().unwrap_or_default(),
                    parsed.body_text(0).unwrap_or_default()
                ),
            };
            let bytes = compose_forward(&config.address, &draft, &raw)?;
            send_prepared(config, auth, &draft, bytes, phase).await
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
        MailCommand::FindSent { message_id } => {
            client
                .examine(mailbox(&config.sent_folder)?, Default::default())
                .await
                .map_err(failure)?;
            let key = SearchKey::Header(
                "Message-ID".try_into().map_err(invalid)?,
                message_id.clone().try_into().map_err(invalid)?,
            );
            let found = client
                .search(
                    Vec1::try_from(vec![key]).expect("one search key"),
                    ImapMessageSearchOptions { uid: true },
                )
                .await
                .map_err(failure)?;
            Ok(json!({"found": !found.is_empty(), "folder": config.sent_folder}))
        }
    }
}

/// The body bytes one partial fetch returned for `uid`. A part the server
/// returns as NIL, or past the end of the message, is empty.
fn body_chunk<'a>(
    rows: &'a std::collections::BTreeMap<NonZeroU32, Vec1<MessageDataItem<'static>>>,
    uid: u32,
) -> Option<&'a [u8]> {
    let row = rows.values().find(|row| {
        row.as_ref()
            .iter()
            .any(|i| matches!(i, MessageDataItem::Uid(id) if id.get() == uid))
    })?;
    row.as_ref().iter().find_map(|item| match item {
        MessageDataItem::BodyExt { data, .. } => {
            Some(data.0.as_ref().map_or(&[][..], |data| data.as_ref()))
        }
        _ => None,
    })
}

fn mailbox_name(value: &Mailbox<'_>) -> String {
    match value {
        Mailbox::Inbox => "INBOX".into(),
        Mailbox::Other(value) => String::from_utf8_lossy(value.inner().as_ref()).into_owned(),
    }
}

/// A part's `type/subtype`, lowercased; `application/octet-stream` when unstated.
fn media_type(part: &mail_parser::MessagePart<'_>) -> String {
    part.content_type().map_or_else(
        || "application/octet-stream".to_owned(),
        |ct| match ct.subtype() {
            Some(sub) => format!("{}/{}", ct.ctype(), sub).to_ascii_lowercase(),
            None => ct.ctype().to_ascii_lowercase(),
        },
    )
}

/// The sender's `Date:` header as RFC 3339, or its trimmed text when it is not a real date.
fn header_date(raw: Option<&str>) -> Option<String> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    Some(
        mail_parser::DateTime::parse_rfc822(raw)
            .filter(mail_parser::DateTime::is_valid)
            .map(|date| date.to_rfc3339())
            .filter(|date| chrono::DateTime::parse_from_rfc3339(date).is_ok())
            .unwrap_or_else(|| raw.chars().take(256).collect()),
    )
}
fn internal_date(item: &MessageDataItem<'_>) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    match item {
        MessageDataItem::InternalDate(at) => Some(*at.as_ref()),
        _ => None,
    }
}
fn rfc3339(at: chrono::DateTime<chrono::FixedOffset>) -> String {
    at.with_timezone(&chrono::Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
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
    phase: &mut SendPhase,
) -> Result<Value, MailError> {
    let message = compose(&config.address, draft, reply)?;
    send_prepared(config, auth, draft, message, phase).await
}

async fn send_prepared(
    config: &MailConfig,
    auth: &Auth,
    draft: &Draft,
    message: Vec<u8>,
    phase: &mut SendPhase,
) -> Result<Value, MailError> {
    let from = SmtpReversePath::SmtpMailbox(smtp_address(&config.address)?);
    let recipients = draft
        .to
        .iter()
        .chain(&draft.cc)
        .chain(&draft.bcc)
        .map(|a| smtp_address(a).map(SmtpForwardPath))
        .collect::<Result<Vec<_>, _>>()?;
    *phase = SendPhase::Connect;
    let mut client = Smtp::open(&config.smtp, auth).await?;
    client
        .transmit(from, recipients, message.clone(), phase)
        .await
        .map_err(smtp_failure)?;
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

/// Error text with no line breaks or control bytes, bounded. Only for
/// errors that never quote a message: TLS and I/O.
fn safe_text(error: &impl std::fmt::Display) -> String {
    error
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_graphic() { c } else { ' ' })
        .take(160)
        .collect()
}

/// A connection failure, with its kind as the cause.
fn io_failure(error: io::Error) -> MailError {
    let (code, cause) = match error.kind() {
        io::ErrorKind::TimedOut => ("mail_timeout", "the mail server stopped responding"),
        io::ErrorKind::ConnectionRefused => (
            "mail_transport_failed",
            "the mail server refused the connection",
        ),
        io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::BrokenPipe => (
            "mail_transport_failed",
            "the mail server closed the connection",
        ),
        io::ErrorKind::NotFound
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkUnreachable => (
            "mail_transport_failed",
            "the mail server could not be reached",
        ),
        _ => (
            "mail_transport_failed",
            "the connection to the mail server failed",
        ),
    };
    MailError::new(code).cause(cause)
}

/// An SMTP failure. A refusal keeps only the server's reply code: its
/// text can quote the message.
fn smtp_failure(error: SmtpClientError) -> MailError {
    use io_smtp::rfc5321::{mail::SmtpMailError, rcpt::SmtpRcptError};
    let refused = |what: &str, code: u16| {
        MailError::new("mail_rejected")
            .cause(format!("the mail server refused {what} with code {code}"))
    };
    match error {
        SmtpClientError::Io(error) => io_failure(error),
        SmtpClientError::Mail(SmtpMailError::Rejected { code, .. }) => refused("the sender", code),
        SmtpClientError::Rcpt(SmtpRcptError::Rejected { code, .. }) => refused("a recipient", code),
        SmtpClientError::Data(SmtpDataError::CommandRejected { code, .. }) => {
            refused("to take the message", code)
        }
        SmtpClientError::Data(SmtpDataError::BodyRejected { code, .. }) => {
            refused("the message", code)
        }
        _ => MailError::new("mail_transport_failed")
            .cause("the mail server's reply was not understood"),
    }
}

#[cfg(test)]
mod tests;
