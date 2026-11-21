//! The files a draft carries. Drafting copies each named file into the
//! bot's file store under an ID derived from its bytes, so a retried tool
//! call names the same snapshot and the owner approves exactly those bytes.
//! Sending reads the snapshots back and refuses any whose hash changed.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::super::AppState;
use crate::app::messages::{MAX_ATTACHMENT_BYTES, media_type, validate_name};
use crate::app::outgoing::send::Approved;
use crate::app::outgoing::{DraftFile, EmailPart, FileSource};
use crate::connectors::types::{MailCommand, MailErrorCode};
use crate::grants::{Grant, Tool};

/// One file read for a draft, before it is stored.
struct Read {
    name: String,
    media_type: String,
    data: Vec<u8>,
}

/// Copy every source into the bot's file store, in order. The error is the
/// refusal the bot reads.
pub(super) async fn snapshot(
    state: &AppState,
    grant: &Grant,
    sources: Vec<FileSource>,
) -> Result<Vec<DraftFile>, String> {
    let mut files = Vec::with_capacity(sources.len());
    for source in sources {
        let read = match &source {
            FileSource::Path(path) => project_file(state, grant, path).await?,
            FileSource::Email(part) => email_file(state, grant, part).await?,
        };
        files.push(store(state, grant.bot_id, read).await?);
    }
    Ok(files)
}

async fn project_file(state: &AppState, grant: &Grant, path: &str) -> Result<Read, String> {
    let project = state
        .registry
        .all()
        .iter()
        .find_map(|provider| provider.project_dir(grant.bot_id))
        .ok_or("FILE_UNAVAILABLE: your project directory is unavailable")?;
    let url = path.to_owned();
    let output = tokio::task::spawn_blocking(move || {
        crate::providers::output_files::read_linked(&project, &url)
    })
    .await
    .map_err(|_| "FILE_UNAVAILABLE: reading the file failed".to_owned())?
    .map_err(|why| format!("FILE_UNAVAILABLE: {path}: {why}"))?;
    Ok(Read {
        name: output.file.name,
        media_type: output.file.media_type,
        data: output.data,
    })
}

/// An attachment of an email this turn may read. The bot never sees its
/// bytes, so no `open_attachments` approval is needed; the owner sees the
/// file on the draft card before anything leaves.
async fn email_file(state: &AppState, grant: &Grant, part: &EmailPart) -> Result<Read, String> {
    let refused = |code: &str| format!("FILE_UNAVAILABLE: email attachment {}: {code}", part.part);
    if !grant.allows(Tool::Mail) {
        return Err(refused(MailErrorCode::MailForbidden.as_str()));
    }
    let manager = &state.connectors;
    super::super::mail_attachments::readable(state, grant, part.account_id)
        .await
        .map_err(|error| refused(error.code.as_str()))?;
    let fetched = manager
        .execute(
            part.account_id,
            grant.bot_id,
            grant.turn_id,
            None,
            MailCommand::OpenAttachments {
                message: part.message.clone(),
                parts: vec![part.part],
            },
        )
        .await
        .map_err(|error| refused(error.code.as_str()))?;
    let listed = &fetched["attachments"][0];
    if let Some(code) = listed["error"].as_str() {
        return Err(refused(code));
    }
    let data = listed["data_base64"]
        .as_str()
        .and_then(|data| STANDARD.decode(data).ok())
        .ok_or_else(|| refused(MailErrorCode::MailTransportFailed.as_str()))?;
    if data.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(refused("larger than 10 MiB"));
    }
    let declared = listed["media_type"]
        .as_str()
        .and_then(|raw| media_type(raw).ok());
    Ok(Read {
        name: file_name(listed["name"].as_str()),
        media_type: sniffed(declared, &data),
        data,
    })
}

/// A name the file store accepts: the email's own name without path
/// separators or control characters.
fn file_name(raw: Option<&str>) -> String {
    let clean: String = raw
        .unwrap_or_default()
        .chars()
        .map(|c| match c {
            '/' | '\\' => '_',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let clean = clean.trim();
    let clean = &clean[..clean.floor_char_boundary(255)];
    if validate_name(clean).is_ok() {
        clean.to_owned()
    } else {
        "attachment".to_owned()
    }
}

/// Image types are kept for bytes that decode, since platforms show those
/// inline; anything else keeps the declared type.
fn sniffed(declared: Option<String>, data: &[u8]) -> String {
    match (crate::providers::output_files::image_type(data), declared) {
        (Some(image), _) => image.to_owned(),
        (None, Some(declared)) if !declared.starts_with("image/") => declared,
        _ => "application/octet-stream".to_owned(),
    }
}

async fn store(state: &AppState, bot_id: Uuid, read: Read) -> Result<DraftFile, String> {
    let sha256 = hex::encode(Sha256::digest(&read.data));
    let id = Uuid::new_v5(
        &bot_id,
        format!("draft-file:{sha256}:{}:{}", read.media_type, read.name).as_bytes(),
    );
    crate::db::attachments::store(
        &state.db,
        bot_id,
        id,
        &read.name,
        &read.media_type,
        &read.data,
    )
    .await
    .map_err(|error| {
        tracing::warn!(%error, "saving a draft attachment failed");
        "UNAVAILABLE: the attachment could not be saved".to_owned()
    })?;
    Ok(DraftFile {
        id,
        name: read.name,
        media_type: read.media_type,
        size: read.data.len() as u64,
        sha256,
    })
}

/// Where a host or computer send finds a file: the conversation staging
/// directory, which the computer mounts under `~/work`.
pub(super) fn staged_path(file: &DraftFile) -> String {
    let relative = crate::providers::attachments::relative_path(&message_file(file));
    format!("~/work/{}", relative.display())
}

fn message_file(file: &DraftFile) -> crate::app::messages::Attachment {
    crate::app::messages::Attachment {
        id: file.id,
        name: file.name.clone(),
        media_type: file.media_type.clone(),
        size: file.size,
    }
}

/// Write the approved bytes where [`staged_path`] points, replacing any
/// copy that changed, for a send the bot makes with its own tools.
pub(super) async fn stage(
    state: &AppState,
    bot_id: Uuid,
    files: &[Approved],
) -> Result<(), String> {
    if files.is_empty() {
        return Ok(());
    }
    let project = state
        .registry
        .all()
        .iter()
        .find_map(|provider| provider.project_dir(bot_id))
        .ok_or("FILE_UNAVAILABLE: your project directory is unavailable")?;
    let inputs: Vec<_> = files
        .iter()
        .map(|approved| crate::app::providers::InputFile {
            attachment: message_file(&approved.file),
            data: approved.data.clone().into(),
        })
        .collect();
    crate::providers::attachments::stage(&project, &inputs)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "staging draft attachments failed");
            "FILE_UNAVAILABLE: the attachments could not be staged".to_owned()
        })
}

/// The `attachments` argument, parsed.
pub(super) fn sources(args: &Value) -> Result<Vec<FileSource>, String> {
    crate::app::outgoing::file_sources(args).map_err(|why| format!("INVALID_DRAFT: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_email_attachment_name_becomes_one_safe_path_component() {
        assert_eq!(file_name(Some("../flyer\n.jpg")), ".._flyer .jpg");
        assert_eq!(file_name(Some("  ")), "attachment");
        assert_eq!(file_name(None), "attachment");
    }

    #[test]
    fn only_decoding_bytes_keep_an_image_type() {
        assert_eq!(
            sniffed(Some("image/png".into()), b"not a png"),
            "application/octet-stream"
        );
        assert_eq!(
            sniffed(Some("application/pdf".into()), b"%PDF-1.7"),
            "application/pdf"
        );
    }
}
