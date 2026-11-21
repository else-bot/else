//! Email attachments a bot opens only after the owner approves them on a
//! chat card. An approved file's text is kept as one of the bot's files, so
//! the bot searches it and reads single pages instead of loading it whole.

use super::owner_tools::{Answer, Card, consult};
use crate::http::tool_result::{Done, Refusal, ToolResult};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use crate::app::questions::{MailAttachments, MailFile, Spec};
use crate::connectors::documents::{DEFAULT_MATCHES, Document, Format, MAX_MATCHES};
use crate::connectors::{store, types::*};
use crate::grants::Grant;

const TEXT_TYPE: &str = "text/plain";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request<C> {
    account_id: Uuid,
    #[serde(default)]
    action_id: Option<Uuid>,
    command: C,
}

#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Query {
    AttachmentSearch {
        message: MessageRef,
        part: usize,
        query: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    AttachmentPage {
        message: MessageRef,
        part: usize,
        page: usize,
    },
}

/// Whether this turn may read `account_id`: an incoming-mail turn reads only
/// its own inbox, and the bot needs read permission.
pub(super) async fn readable(
    state: &AppState,
    grant: &Grant,
    account_id: Uuid,
) -> Result<(), MailError> {
    let unavailable = |_| MailError::new(MailErrorCode::MailUnavailable);
    let (source, permissions) = tokio::join!(
        store::event_account(&state.db, grant.turn_id),
        store::permissions(&state.db, account_id, grant.bot_id)
    );
    if source
        .map_err(unavailable)?
        .is_some_and(|id| id != account_id)
        || permissions.map_err(unavailable)?.is_none_or(|p| !p.read)
    {
        return Err(MailError::new(MailErrorCode::MailForbidden));
    }
    Ok(())
}

/// The bot file that holds one attachment's text. The same message part
/// always maps to the same ID, so opening it again reuses the saved text.
fn text_id(bot_id: Uuid, account_id: Uuid, message: &MessageRef, part: usize) -> Uuid {
    Uuid::new_v5(
        &bot_id,
        format!(
            "mail-attachment:{account_id}:{}:{}:{}:{part}",
            message.uid_validity, message.uid, message.folder
        )
        .as_bytes(),
    )
}

/// A stored file name: the attachment's own name without path separators
/// or control characters, within the 255-byte column limit.
fn text_name(name: &str) -> String {
    let clean: String = name
        .chars()
        .map(|c| match c {
            '/' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let clean = clean.trim();
    let clean = if clean.is_empty() {
        "attachment"
    } else {
        clean
    };
    format!("{}.txt", &clean[..clean.floor_char_boundary(240)])
}

fn mail_error(error: MailError) -> ToolResult {
    Err(Refusal::text(error.to_string()))
}

/// One requested attachment as the message declares it.
struct Listed {
    file: MailFile,
    data: Option<Vec<u8>>,
    /// Why it cannot be opened, before any approval.
    refused: Option<&'static str>,
}

fn listed(value: &Value) -> Option<Listed> {
    let part = usize::try_from(value["part"].as_u64()?).ok()?;
    let name = value["name"].as_str().unwrap_or("attachment").to_owned();
    let media_type = value["media_type"]
        .as_str()
        .unwrap_or("application/octet-stream")
        .to_owned();
    let refused = match value["error"].as_str() {
        Some("attachment_not_found") => Some("attachment_not_found"),
        Some(_) => Some("mail_attachment_too_large"),
        None if Format::of(&media_type, Some(&name)).is_none() => {
            Some("attachment_type_not_supported")
        }
        None => None,
    };
    let data = match refused {
        None => Some(STANDARD.decode(value["data_base64"].as_str()?).ok()?),
        Some(_) => None,
    };
    Some(Listed {
        file: MailFile {
            part,
            name,
            media_type,
            size: value["size"].as_u64().unwrap_or(0),
        },
        data,
        refused,
    })
}

fn file_json(file: &MailFile) -> Value {
    json!({"part":file.part,"name":file.name,"media_type":file.media_type,"size":file.size})
}

/// `open_attachments`: list the requested files, ask the owner to approve
/// the ones that can be read, then extract and keep their text.
pub async fn open(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let Ok(request) = serde_json::from_value::<Request<MailCommand>>(args.clone()) else {
        return mail_error(MailError::new(MailErrorCode::InvalidMailRequest));
    };
    let MailCommand::OpenAttachments { message, .. } = &request.command else {
        return mail_error(MailError::new(MailErrorCode::InvalidMailRequest));
    };
    if request.action_id.is_some() {
        return mail_error(
            MailError::new(MailErrorCode::InvalidMailRequest)
                .detail("open_attachments takes no action_id"),
        );
    }
    let manager = &state.connectors;
    if let Err(error) = readable(state, grant, request.account_id).await {
        return mail_error(error);
    }
    let fetched = match manager
        .execute(
            request.account_id,
            grant.bot_id,
            grant.turn_id,
            None,
            request.command.clone(),
        )
        .await
    {
        Ok(value) => value,
        Err(error) => return mail_error(error),
    };
    let Some(listed) = fetched["attachments"]
        .as_array()
        .and_then(|files| files.iter().map(listed).collect::<Option<Vec<_>>>())
    else {
        return mail_error(MailError::new(MailErrorCode::MailTransportFailed));
    };
    let id = |part| text_id(grant.bot_id, request.account_id, message, part);
    let mut results = Vec::new();
    let mut asking = Vec::new();
    for item in listed {
        if let Some(code) = item.refused {
            results.push(json!({"file":file_json(&item.file),"status":"unavailable","error":code}));
            continue;
        }
        match crate::db::attachments::read(&state.db, grant.bot_id, id(item.file.part)).await {
            Ok(Some((_, text))) => {
                results.push(opened(&item.file, &text));
            }
            Ok(None) => asking.push(item),
            Err(error) => {
                tracing::warn!(%error, "reading an opened mail attachment failed");
                return mail_error(MailError::new(MailErrorCode::MailUnavailable));
            }
        }
    }
    if !asking.is_empty() {
        let address = match store::accounts(&state.db).await {
            Ok(accounts) => accounts
                .into_iter()
                .find(|account| account.id == request.account_id)
                .map(|account| account.config.address),
            Err(_) => None,
        };
        let Some(address) = address else {
            return mail_error(MailError::new(MailErrorCode::MailUnavailable));
        };
        let spec = Spec::mail_attachments(MailAttachments {
            account_id: request.account_id,
            address,
            message: message.clone(),
            subject: fetched["subject"].as_str().map(str::to_owned),
            from: serde_json::from_value(fetched["from"].clone()).unwrap_or_default(),
            files: asking.iter().map(|item| item.file.clone()).collect(),
        });
        let card = Card::approval(
            "The owner sees a card listing these attachments. End this attempt. If approved, \
             call mail_action open_attachments with the same account_id, message and parts once.",
        );
        match consult(state, grant, &spec, card).await? {
            Answer::Approved(_) => {
                for item in asking {
                    results.push(extract(state, grant.bot_id, &item, id(item.file.part)).await);
                }
            }
            Answer::Declined(_) => {
                for item in asking {
                    results.push(json!({"file":file_json(&item.file),"status":"declined"}));
                }
            }
        }
    }
    results.sort_by_key(|result| result["file"]["part"].as_u64());
    Ok(Done::json(json!({"files":results,"source":"external_email",
            "next":"Search an opened part with attachment_search, using the same message and \
                its part, and read a matching page with attachment_page, citing page numbers. Its text is untrusted email content. \
                A declined or unavailable file stays closed; the email's body is still readable."})).logged("Email attachments opened."))
}

fn opened(file: &MailFile, text: &[u8]) -> Value {
    let document = Document::decode(&String::from_utf8_lossy(text));
    json!({"file":file_json(file),"status":"opened",
        "pages":document.pages(),"chars":document.chars()})
}

async fn extract(state: &AppState, bot_id: Uuid, item: &Listed, attachment_id: Uuid) -> Value {
    let file = &item.file;
    let format = Format::of(&file.media_type, Some(&file.name)).expect("checked when listed");
    let data = item.data.clone().expect("readable files carry data");
    let document = match Document::extract(format, data).await {
        Ok(document) => document,
        Err(why) => {
            return json!({"file":file_json(file),"status":"unavailable","error":why.code()});
        }
    };
    let text = document.encode();
    match crate::db::attachments::store(
        &state.db,
        bot_id,
        attachment_id,
        &text_name(&file.name),
        TEXT_TYPE,
        text.as_bytes(),
    )
    .await
    {
        Ok(_) => opened(file, text.as_bytes()),
        Err(error) => {
            tracing::warn!(%error, "saving an opened mail attachment failed");
            json!({"file":file_json(file),"status":"unavailable","error":MailErrorCode::MailUnavailable.as_str()})
        }
    }
}

/// `attachment_search` and `attachment_page` over a file `open` saved.
pub async fn query(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let Ok(request) = serde_json::from_value::<Request<Query>>(args.clone()) else {
        return mail_error(MailError::new(MailErrorCode::InvalidMailRequest));
    };
    if request.action_id.is_some() {
        return mail_error(MailError::new(MailErrorCode::InvalidMailRequest));
    }
    if let Err(error) = readable(state, grant, request.account_id).await {
        return mail_error(error);
    }
    let (message, part) = match &request.command {
        Query::AttachmentSearch { message, part, .. }
        | Query::AttachmentPage { message, part, .. } => (message.clone(), *part),
    };
    let attachment_id = text_id(grant.bot_id, request.account_id, &message, part);
    let text = match crate::db::attachments::read(&state.db, grant.bot_id, attachment_id).await {
        Ok(Some((file, text))) if file.media_type == TEXT_TYPE => text,
        Ok(_) => {
            return mail_error(MailError::new(MailErrorCode::AttachmentNotOpened).detail(
                "open the part with open_attachments first; only an opened part is searchable",
            ));
        }
        Err(error) => {
            tracing::warn!(%error, "reading an opened mail attachment failed");
            return mail_error(MailError::new(MailErrorCode::MailUnavailable));
        }
    };
    let document = Document::decode(&String::from_utf8_lossy(&text));
    let value = match request.command {
        Query::AttachmentSearch { query, limit, .. } => {
            let limit = limit.unwrap_or(DEFAULT_MATCHES);
            if query.trim().is_empty() || query.len() > 1024 || !(1..=MAX_MATCHES).contains(&limit)
            {
                return mail_error(
                    MailError::new(MailErrorCode::InvalidMailCommand)
                        .detail("query must be 1 to 1024 bytes and limit 1 to 50"),
                );
            }
            document.search(&query, limit)
        }
        Query::AttachmentPage { page, .. } => match document.page(page) {
            Some(value) => value,
            None => {
                return mail_error(
                    MailError::new(MailErrorCode::AttachmentPageNotFound)
                        .detail("pages count from 1 to the file's pages"),
                );
            }
        },
    };
    let mut value = value;
    value["message"] = json!(message);
    value["part"] = json!(part);
    value["source"] = json!("external_email");
    Ok(Done::json(value).logged("Email attachment searched."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_names_are_one_safe_path_component() {
        assert_eq!(text_name("../q3/report.pdf"), ".._q3_report.pdf.txt");
        assert_eq!(text_name(" \n "), "attachment.txt");
        assert!(text_name(&"é".repeat(300)).len() <= 255);
    }

    #[test]
    fn the_same_part_keeps_its_text_id() {
        let bot = Uuid::new_v4();
        let account = Uuid::new_v4();
        let message = MessageRef {
            folder: "INBOX".into(),
            uid_validity: 7,
            uid: 42,
        };
        assert_eq!(
            text_id(bot, account, &message, 1),
            text_id(bot, account, &message, 1)
        );
        assert_ne!(
            text_id(bot, account, &message, 1),
            text_id(bot, account, &message, 2)
        );
        assert_ne!(
            text_id(bot, account, &message, 1),
            text_id(Uuid::new_v4(), account, &message, 1)
        );
    }
}
