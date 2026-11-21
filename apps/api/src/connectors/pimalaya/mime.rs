use super::*;
use else_visual::email::TextLink;

pub(super) fn addresses(value: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|a| a.address.as_ref().map(|s| s.to_string()))
        .take(100)
        .collect()
}

/// Each address with its display name, for the owner's reader.
fn people(value: Option<&mail_parser::Address<'_>>) -> Vec<Value> {
    value
        .into_iter()
        .flat_map(|a| a.iter())
        .take(100)
        .map(|a| json!({"name":a.name.as_deref(),"address":a.address.as_deref()}))
        .collect()
}

/// Characters of text the owner's reader gets.
const VIEW_TEXT_CHARS: usize = 200_000;
/// Decoded bytes one image the HTML shows may have.
const INLINE_IMAGE_LIMIT: usize = 1024 * 1024;

/// The message as the owner's reader shows it. `html` is the message's own
/// HTML part, never one made from its text, and is left out over
/// `else_visual::email::MAX_HTML_BYTES`. Images the HTML names by `cid:`
/// travel as `inline` within that render's budget instead of as
/// attachments.
pub(super) fn view(
    parsed: &mail_parser::Message<'_>,
    message: &MessageRef,
    received_at: Option<String>,
) -> Value {
    use else_visual::email::{MAX_HTML_BYTES, MAX_INLINE_BASE64};
    let (html, html_omitted) = match html_part(parsed) {
        Some(html) if html.len() > MAX_HTML_BYTES => (None, Some("too_large")),
        html => (html, None),
    };
    let lowered = html.map(str::to_ascii_lowercase).unwrap_or_default();
    let body = parsed.body_text(0).unwrap_or_default();
    let text: String = body.chars().take(VIEW_TEXT_CHARS).collect();
    let mut budget = MAX_INLINE_BASE64;
    let mut inline = Vec::new();
    let mut attachments = Vec::new();
    for (part, file) in parsed.attachments().enumerate() {
        let media_type = media_type(file);
        let shown = file
            .content_id()
            .map(|id| id.trim_matches(['<', '>']))
            .filter(|id| {
                !id.is_empty()
                    && media_type.starts_with("image/")
                    && lowered.contains(&format!("cid:{}", id.to_ascii_lowercase()))
            });
        if let Some(id) = shown {
            let size = file.contents().len();
            let encoded = size.div_ceil(3) * 4;
            if size <= INLINE_IMAGE_LIMIT && encoded <= budget {
                budget -= encoded;
                inline.push(json!({"content_id":id,"media_type":media_type,
                    "data_base64":STANDARD.encode(file.contents())}));
            }
            continue;
        }
        attachments.push(json!({"part":part,"name":file.attachment_name(),
            "media_type":media_type,"size":file.contents().len()}));
    }
    json!({"message":message,"date":header_date(parsed.header_raw("Date")),
        "received_at":received_at,"subject":parsed.subject(),"from":people(parsed.from()),
        "to":people(parsed.to()),"cc":people(parsed.cc()),"reply_to":people(parsed.reply_to()),
        "text":text,"text_truncated":text.len() < body.len(),"html":html,
        "html_omitted":html_omitted,"inline":inline,"attachments":attachments})
}

/// The message's own HTML part, never one made from its text.
fn html_part<'a>(parsed: &'a mail_parser::Message<'_>) -> Option<&'a str> {
    parsed
        .html_body
        .iter()
        .find_map(|id| match &parsed.part(*id)?.body {
            mail_parser::PartType::Html(html) => Some(html.as_ref()),
            _ => None,
        })
}

/// The text a bot reads and the links in the message's HTML parts. The
/// HTML's text, with each link's URL after its label, wins unless the plain
/// part shows more words, URLs aside: a plain part is often only a pointer
/// to the HTML, and an HTML part is sometimes only an image and a button.
pub(super) fn bot_text(parsed: &mail_parser::Message<'_>) -> (String, Vec<TextLink>) {
    let html: Vec<&str> = parsed
        .html_body
        .iter()
        .filter_map(|id| match &parsed.part(*id)?.body {
            mail_parser::PartType::Html(html) => Some(html.as_ref()),
            _ => None,
        })
        .collect();
    if html.is_empty() {
        return (
            parsed.body_text(0).unwrap_or_default().into_owned(),
            Vec::new(),
        );
    }
    let html = else_visual::email::as_text(html);
    let plain_words = |plain: &str| {
        plain
            .split_whitespace()
            .filter(|word| !word.contains("://"))
            .count()
    };
    let plain = parsed
        .text_body
        .iter()
        .find_map(|id| match &parsed.part(*id)?.body {
            mail_parser::PartType::Text(text) => Some(text.as_ref()),
            _ => None,
        });
    match plain {
        Some(plain) if plain_words(plain) > html.words => (plain.to_owned(), html.links),
        _ => (html.text, html.links),
    }
}

/// A new message to `to` quoting the parsed one below `body`.
pub(super) fn forward(parsed: &mail_parser::Message<'_>, to: Vec<String>, body: &str) -> Draft {
    let subject = parsed.subject().unwrap_or_default();
    Draft {
        to,
        cc: Vec::new(),
        bcc: Vec::new(),
        attachments: Vec::new(),
        reply_to: None,
        message_id: None,
        subject: format!("Fwd: {subject}"),
        body: format!(
            "{body}\n\n---------- Forwarded message ----------\nFrom: {}\nSubject: {subject}\n\n{}",
            addresses(parsed.from()).join(", "),
            bot_text(parsed).0
        ),
    }
}

pub(super) struct ThreadHeaders {
    in_reply_to: String,
    references: Vec<String>,
}
pub(super) fn reply(
    parsed: &mail_parser::Message<'_>,
    own: &str,
    body: &str,
    all: bool,
) -> Result<(Draft, ThreadHeaders), MailError> {
    let mut to = addresses(parsed.reply_to().or_else(|| parsed.from()));
    let mut cc = Vec::new();
    if all {
        to.extend(addresses(parsed.to()));
        cc.extend(addresses(parsed.cc()));
    }
    let mut seen = std::collections::HashSet::new();
    seen.insert(own.to_ascii_lowercase());
    to.retain(|a| seen.insert(a.to_ascii_lowercase()));
    cc.retain(|a| seen.insert(a.to_ascii_lowercase()));
    if to.is_empty() {
        return Err(MailError::new(MailErrorCode::MailReplyHasNoRecipient));
    }
    let subject = parsed.subject().unwrap_or_default();
    Ok((
        Draft {
            to,
            cc,
            bcc: Vec::new(),
            subject: if subject.to_ascii_lowercase().starts_with("re:") {
                subject.to_owned()
            } else {
                format!("Re: {subject}")
            },
            body: body.to_owned(),
            attachments: Vec::new(),
            reply_to: None,
            message_id: None,
        },
        thread(parsed),
    ))
}

pub(super) fn thread(parsed: &mail_parser::Message<'_>) -> ThreadHeaders {
    let id = parsed
        .message_id()
        .filter(|id| safe_message_id(id))
        .unwrap_or_default()
        .to_owned();
    let mut references = parsed
        .references()
        .as_text_list()
        .unwrap_or_default()
        .iter()
        .take(30)
        .filter(|id| safe_message_id(id))
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if !id.is_empty() {
        references.push(id.clone());
    }
    ThreadHeaders {
        in_reply_to: id,
        references,
    }
}
fn safe_message_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 998
        && value
            .bytes()
            .all(|b| (33..=126).contains(&b) && !b"<>\"".contains(&b))
}

pub(super) fn smtp_address(address: &str) -> Result<SmtpMailbox<'static>, MailError> {
    let Some((local, domain)) = address.rsplit_once('@') else {
        return Err(MailError::new(MailErrorCode::InvalidEmailAddress));
    };
    if local.is_empty()
        || local.len() > 64
        || domain.is_empty()
        || address.len() > 254
        || !local
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".!#$%&'*+-/=?^_`{|}~".contains(&c))
        || !domain
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
    {
        return Err(MailError::new(MailErrorCode::InvalidEmailAddress));
    }
    Ok(SmtpMailbox {
        local_part: SmtpLocalPart(Cow::Owned(local.into())),
        domain: SmtpEhloDomain::SmtpDomain(SmtpDomain(Cow::Owned(domain.into()))),
    })
}
pub(super) fn compose(
    from: &str,
    draft: &Draft,
    reply: Option<ThreadHeaders>,
) -> Result<Vec<u8>, MailError> {
    build(from, draft, reply, None)
}

pub(super) fn compose_forward(from: &str, draft: &Draft, raw: &[u8]) -> Result<Vec<u8>, MailError> {
    build(from, draft, None, Some(raw))
}

fn build(
    from: &str,
    draft: &Draft,
    reply: Option<ThreadHeaders>,
    forwarded: Option<&[u8]>,
) -> Result<Vec<u8>, MailError> {
    smtp_address(from)?;
    let recipients = draft.to.len() + draft.cc.len() + draft.bcc.len();
    if recipients == 0
        || recipients > 100
        || draft.subject.len() > 998
        || draft.subject.contains(['\r', '\n'])
        || draft.body.len() > MESSAGE_LIMIT / 2
    {
        return Err(MailError::new(MailErrorCode::InvalidEmailMessage));
    }
    for recipient in draft.to.iter().chain(&draft.cc).chain(&draft.bcc) {
        smtp_address(recipient)?;
    }
    let mut builder = MessageBuilder::new()
        .from(from.to_owned())
        .to(draft.to.clone())
        .subject(draft.subject.clone())
        .text_body(draft.body.clone());
    if !draft.cc.is_empty() {
        builder = builder.cc(draft.cc.clone());
    }
    if let Some(id) = &draft.message_id {
        builder = builder.message_id(id.clone());
    }
    let mut attachment_bytes = 0;
    if draft.attachments.len() > 10 {
        return Err(MailError::new(MailErrorCode::MailAttachmentsTooLarge));
    }
    for attachment in &draft.attachments {
        if attachment.data_base64.len() > crate::connectors::types::MAX_ATTACHMENT_BASE64 {
            return Err(MailError::new(MailErrorCode::MailAttachmentsTooLarge));
        }
        let bytes = STANDARD
            .decode(&attachment.data_base64)
            .map_err(|_| MailError::new(MailErrorCode::InvalidMailAttachment))?;
        attachment_bytes += bytes.len();
        if attachment_bytes > crate::connectors::types::MAX_ATTACHMENT_TOTAL {
            return Err(MailError::new(MailErrorCode::MailAttachmentsTooLarge));
        }
        builder = builder.attachment(
            attachment.media_type.clone(),
            attachment.name.clone(),
            bytes,
        );
    }
    if let Some(raw) = forwarded {
        builder = builder.attachment("message/rfc822", "forwarded.eml", forward_source(raw)?);
    }
    if let Some(headers) = reply {
        if !headers.in_reply_to.is_empty() {
            builder = builder.in_reply_to(headers.in_reply_to);
        }
        if !headers.references.is_empty() {
            builder = builder.references(headers.references);
        }
    }
    let message = builder.write_to_vec().map_err(failure)?;
    if message.len() > MESSAGE_LIMIT {
        return Err(MailError::new(MailErrorCode::MailMessageTooLarge));
    }
    Ok(message)
}

pub(super) fn forward_source(raw: &[u8]) -> Result<Vec<u8>, MailError> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| MailError::new(MailErrorCode::InvalidEmailMessage))?;
    let mut output = Vec::with_capacity(raw.len());
    let mut offset = 0;
    for header in parsed.headers().iter().filter(|h| {
        matches!(
            h.name,
            mail_parser::HeaderName::Bcc | mail_parser::HeaderName::ResentBcc
        )
    }) {
        output.extend_from_slice(&raw[offset..header.offset_field as usize]);
        offset = header.offset_end as usize;
    }
    output.extend_from_slice(&raw[offset..]);
    Ok(output)
}
