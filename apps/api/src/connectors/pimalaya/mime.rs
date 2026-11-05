use super::*;

pub(super) fn addresses(value: Option<&mail_parser::Address<'_>>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|a| a.iter())
        .filter_map(|a| a.address.as_ref().map(|s| s.to_string()))
        .take(100)
        .collect()
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
        return Err(MailError::new("mail_reply_has_no_recipient"));
    }
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
        },
        ThreadHeaders {
            in_reply_to: id,
            references,
        },
    ))
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
        return Err(MailError::new("invalid_email_address"));
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
        return Err(MailError::new("invalid_email_address"));
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
    build(from, draft, reply, false, None)
}

pub(super) fn compose_draft(
    from: &str,
    draft: &Draft,
    reply: Option<ThreadHeaders>,
) -> Result<Vec<u8>, MailError> {
    build(from, draft, reply, true, None)
}

pub(super) fn compose_forward(
    from: &str,
    draft: &Draft,
    raw: &[u8],
    is_draft: bool,
) -> Result<Vec<u8>, MailError> {
    build(from, draft, None, is_draft, Some(raw))
}

fn build(
    from: &str,
    draft: &Draft,
    reply: Option<ThreadHeaders>,
    is_draft: bool,
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
        return Err(MailError::new("invalid_email_message"));
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
    if is_draft && !draft.bcc.is_empty() {
        builder = builder.bcc(draft.bcc.clone());
    }
    let mut attachment_bytes = 0;
    if draft.attachments.len() > 10 {
        return Err(MailError::new("mail_attachments_too_large"));
    }
    for attachment in &draft.attachments {
        if attachment.data_base64.len() > 1_398_104 {
            return Err(MailError::new("mail_attachments_too_large"));
        }
        let bytes = STANDARD
            .decode(&attachment.data_base64)
            .map_err(|_| MailError::new("invalid_mail_attachment"))?;
        attachment_bytes += bytes.len();
        if attachment_bytes > 1024 * 1024 {
            return Err(MailError::new("mail_attachments_too_large"));
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
        return Err(MailError::new("mail_message_too_large"));
    }
    Ok(message)
}

pub(super) fn forward_source(raw: &[u8]) -> Result<Vec<u8>, MailError> {
    let parsed = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| MailError::new("invalid_email_message"))?;
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
