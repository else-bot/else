//! Sending an approved draft through its connected email or social
//! account, as one durable action: the claim commits before any external
//! call, so a send never repeats, and a result this call cannot confirm
//! stays for the receipt or a later call to settle.

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use uuid::Uuid;

use super::{Channels, DraftFile, Platform};
use crate::connectors::types::{MailCommand, MailError, OutgoingAttachment};
use crate::db::outgoing_drafts::{
    self, Claim, Claimed, DraftError, MailSend, OutgoingDraft, Payload, SentCheck,
};
use crate::socials::types::{Error as SocialError, SendFile, SendMessage};

/// The connected accounts a send reaches, for one caller. A send must
/// record its action receipt under `action_id` before its external effect
/// and deduplicate by it, because a resumed claim calls it again with the
/// same ID. It returns an error that leaves no receipt only when nothing
/// was sent: the draft then fails.
#[async_trait]
pub trait Connected: Sync {
    /// Send through the mail connector as action `action_id`.
    async fn send_mail(
        &self,
        account_id: Uuid,
        action_id: Uuid,
        command: MailCommand,
    ) -> Result<(), MailError>;
    /// Whether the account's Sent folder holds a message with this
    /// Message-ID.
    async fn find_sent(&self, account_id: Uuid, message_id: String) -> Result<bool, MailError>;
    /// Send through the social connection as action `action_id`, if the
    /// connection is on `platform`.
    async fn send_social(
        &self,
        account_id: Uuid,
        action_id: Uuid,
        platform: Platform,
        message: SendMessage,
    ) -> Result<(), SocialError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    #[error(transparent)]
    Draft(#[from] DraftError),
    /// This call's claim failed before sending: an approved file changed.
    #[error("{0}")]
    FileChanged(FileChanged),
    /// An approved file changed after an earlier call of this request
    /// claimed the send, which may have gone out. The draft stays as it is.
    #[error("{0}")]
    FileChangedAfterClaim(FileChanged),
}

/// Send draft `draft_id`, approved for request `turn_id`, through its
/// connected account, if `channels` allow it. A repeated call in the same
/// request resumes the first call's send with its action ID, and returns
/// the draft once settled; it never sends twice.
pub async fn send_connected(
    db: &SqlitePool,
    turn_id: Uuid,
    draft_id: Uuid,
    channels: Channels,
    via: &dyn Connected,
) -> Result<OutgoingDraft, SendError> {
    let (claimed, files) =
        match outgoing_drafts::claim_connected(db, turn_id, draft_id, channels).await? {
            Claim::Settled(draft) => return Ok(draft),
            Claim::Fresh(claimed) => match approved(db, &claimed.draft).await {
                Ok(files) => (claimed, files),
                Err(changed) => {
                    outgoing_drafts::fail_unsent(db, draft_id, &changed.to_string()).await?;
                    return Err(SendError::FileChanged(changed));
                }
            },
            Claim::Resumed(claimed) => match approved(db, &claimed.draft).await {
                Ok(files) => (claimed, files),
                Err(changed) => return Err(SendError::FileChangedAfterClaim(changed)),
            },
        };
    let Claimed {
        draft,
        account_id,
        action_id,
        payload,
    } = claimed;
    Ok(match payload {
        Payload::Mail(email) => {
            match outgoing_drafts::next_mail_send(db, draft_id, &email.message_id(draft.id)).await?
            {
                MailSend::Dispatch { message_id } => {
                    let attachments = files.iter().map(mail_file).collect();
                    let command = email.send_command(attachments, Some(message_id));
                    let result = via.send_mail(account_id, action_id, command).await;
                    let refused = result.err().map(|error| error.code.as_str());
                    outgoing_drafts::after_mail_send(db, draft_id, refused).await?
                }
                MailSend::Sent(draft) => *draft,
                MailSend::CheckSent {
                    message_id,
                    earlier,
                } => {
                    let check = match message_id {
                        None => Err("an earlier send carried no Message-ID to search for".into()),
                        Some(message_id) => via
                            .find_sent(account_id, message_id)
                            .await
                            .map(|found| {
                                if found {
                                    SentCheck::Found
                                } else {
                                    SentCheck::Absent
                                }
                            })
                            .map_err(|error| error.reason()),
                    };
                    outgoing_drafts::record_sent_check(db, draft_id, action_id, &earlier, check)
                        .await?
                }
            }
        }
        Payload::Social {
            message,
            destination,
        } => {
            let send = SendMessage {
                destination,
                body: message.body,
                reply_to: message.reply_to,
                files: files.into_iter().map(social_file).collect(),
            };
            let result = via
                .send_social(account_id, action_id, message.platform, send)
                .await;
            let refused = result.err().map(|error| error.to_string());
            outgoing_drafts::after_social_send(db, draft_id, refused.as_deref()).await?
        }
    })
}

/// A draft file's approved bytes.
pub struct Approved {
    pub file: DraftFile,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the attachment {0} changed or is missing since the owner approved it; nothing was sent")]
pub struct FileChanged(pub String);

/// The approved bytes of every file `draft` carries, in order, or the name
/// of the first one missing or changed since approval.
pub async fn approved(
    db: &SqlitePool,
    draft: &OutgoingDraft,
) -> Result<Vec<Approved>, FileChanged> {
    let files = draft.content.files();
    let mut out = Vec::with_capacity(files.len());
    for file in files {
        let read = crate::db::attachments::read(db, draft.bot_id, file.id)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "reading a draft attachment failed");
                FileChanged(file.name.clone())
            })?;
        match read {
            Some((_, data)) if hex::encode(Sha256::digest(&data)) == file.sha256 => {
                out.push(Approved {
                    file: file.clone(),
                    data,
                });
            }
            _ => return Err(FileChanged(file.name.clone())),
        }
    }
    Ok(out)
}

fn mail_file(file: &Approved) -> OutgoingAttachment {
    use base64::{Engine, engine::general_purpose::STANDARD};
    OutgoingAttachment {
        name: file.file.name.clone(),
        media_type: file.file.media_type.clone(),
        data_base64: STANDARD.encode(&file.data),
    }
}

fn social_file(file: Approved) -> SendFile {
    SendFile {
        name: file.file.name,
        media_type: file.file.media_type,
        sha256: file.file.sha256,
        data: file.data.into(),
    }
}

#[cfg(test)]
mod tests;
