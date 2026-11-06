use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_TEXT_BYTES: usize = 32 * 1024;
/// Largest file a conversation message may attach.
pub const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
/// Files one message may carry.
pub const MAX_MESSAGE_FILES: usize = 8;
const MAX_NAME_BYTES: usize = 255;
const MAX_MEDIA_TYPE_BYTES: usize = 127;
/// Queued turns a bot may hold before a due routine run is skipped.
pub const MAX_QUEUED_TURNS: i64 = 4;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidMessage {
    #[error("Message text must not be empty.")]
    Empty,
    #[error("Message text must be at most 32 KiB.")]
    TooLarge,
    #[error("Message text must not contain NUL characters.")]
    Nul,
    #[error("Message text must equal its parts, with each file written as [File: NAME].")]
    PartsMismatch,
    #[error("A message may carry at most 8 files.")]
    TooManyFiles,
}

/// A stored file scoped to one bot. Messages reference its ID, never a client path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: Uuid,
    pub name: String,
    pub media_type: String,
    pub size: u64,
}

/// Ordered text or a stored file in a conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MessagePart {
    Text { text: String },
    File { file: Attachment },
}

/// How a file appears in a message's plain text.
pub fn file_marker(name: &str) -> String {
    format!("[File: {name}]")
}

/// The plain text of ordered parts: text exactly as written, each file as
/// its marker.
pub fn flatten(parts: &[MessagePart]) -> String {
    let mut text = String::new();
    for part in parts {
        match part {
            MessagePart::Text { text: t } => text.push_str(t),
            MessagePart::File { file } => text.push_str(&file_marker(&file.name)),
        }
    }
    text
}

/// Check that `parts` spell `text` and return their canonical form:
/// adjacent text joined, empty text dropped, and no parts at all for a
/// message without files, so it is stored and hashed as plain text.
pub fn canonical_parts(
    text: &str,
    parts: Vec<MessagePart>,
) -> Result<Vec<MessagePart>, InvalidMessage> {
    if parts.is_empty() {
        return Ok(parts);
    }
    if flatten(&parts) != text {
        return Err(InvalidMessage::PartsMismatch);
    }
    let files = parts
        .iter()
        .filter(|part| matches!(part, MessagePart::File { .. }))
        .count();
    if files > MAX_MESSAGE_FILES {
        return Err(InvalidMessage::TooManyFiles);
    }
    if files == 0 {
        return Ok(Vec::new());
    }
    let mut canonical: Vec<MessagePart> = Vec::with_capacity(parts.len());
    for part in parts {
        match (canonical.last_mut(), part) {
            (_, MessagePart::Text { text }) if text.is_empty() => {}
            (Some(MessagePart::Text { text: last }), MessagePart::Text { text }) => {
                last.push_str(&text)
            }
            (_, part) => canonical.push(part),
        }
    }
    Ok(canonical)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidAttachment {
    #[error("A file name must be 1 to 255 bytes without slashes or control characters.")]
    Name,
    #[error("A media type must look like type/subtype.")]
    MediaType,
    #[error("File data must be standard base64.")]
    Encoding,
    #[error("A file must be at most 10 MiB.")]
    TooLarge,
}

/// A file name usable as one path component and inside a marker.
pub fn validate_name(name: &str) -> Result<(), InvalidAttachment> {
    let ok = !name.trim().is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control());
    ok.then_some(()).ok_or(InvalidAttachment::Name)
}

/// The lowercase `type/subtype` form of a media type, without parameters.
pub fn media_type(raw: &str) -> Result<String, InvalidAttachment> {
    let lower = raw.to_ascii_lowercase();
    let token = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-".contains(&b))
    };
    match lower.split_once('/') {
        Some((kind, sub)) if lower.len() <= MAX_MEDIA_TYPE_BYTES && token(kind) && token(sub) => {
            Ok(lower)
        }
        _ => Err(InvalidAttachment::MediaType),
    }
}

/// The owner's reaction to one of the bot's messages, sent with the
/// feedback message it prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reaction {
    pub item_id: Uuid,
    pub kind: ReactionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReactionKind {
    ThumbsUp,
    ThumbsDown,
}

impl ReactionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ReactionKind::ThumbsUp => "thumbs_up",
            ReactionKind::ThumbsDown => "thumbs_down",
        }
    }
}

/// Validate owner text. Whitespace is preserved exactly as sent.
pub fn validate_text(text: &str) -> Result<(), InvalidMessage> {
    if text.trim().is_empty() {
        return Err(InvalidMessage::Empty);
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(InvalidMessage::TooLarge);
    }
    if text.contains('\0') {
        return Err(InvalidMessage::Nul);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_rules() {
        assert_eq!(validate_text(" \n\t"), Err(InvalidMessage::Empty));
        assert_eq!(validate_text("a\0b"), Err(InvalidMessage::Nul));
        assert_eq!(
            validate_text(&"x".repeat(MAX_TEXT_BYTES + 1)),
            Err(InvalidMessage::TooLarge)
        );
        assert!(validate_text(&"x".repeat(MAX_TEXT_BYTES)).is_ok());
        assert!(validate_text("  keep\n  spacing  ").is_ok());
    }

    fn file(name: &str) -> MessagePart {
        MessagePart::File {
            file: Attachment {
                id: Uuid::nil(),
                name: name.into(),
                media_type: "image/png".into(),
                size: 3,
            },
        }
    }

    fn text(text: &str) -> MessagePart {
        MessagePart::Text { text: text.into() }
    }

    #[test]
    fn parts_use_the_tagged_wire_shape() {
        let json = serde_json::to_value([text("a"), file("x.png")]).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                { "type": "text", "text": "a" },
                { "type": "file", "file": {
                    "id": Uuid::nil(), "name": "x.png", "media_type": "image/png", "size": 3
                } },
            ])
        );
        let extra = serde_json::json!({ "type": "text", "text": "a", "path": "/etc" });
        assert!(serde_json::from_value::<MessagePart>(extra).is_err());
    }

    #[test]
    fn parts_must_spell_the_text_and_are_canonical() {
        let parts = vec![
            text(" look\n"),
            text(""),
            file("a.png"),
            text("  "),
            text("and "),
        ];
        let flat = " look\n[File: a.png]  and ";
        assert_eq!(flatten(&parts), flat);
        assert_eq!(
            canonical_parts(flat, parts.clone()).unwrap(),
            vec![text(" look\n"), file("a.png"), text("  and ")]
        );
        assert_eq!(
            canonical_parts("look [File: a.png]", parts),
            Err(InvalidMessage::PartsMismatch)
        );
        assert_eq!(canonical_parts("hi", vec![text("hi")]).unwrap(), vec![]);
        assert_eq!(
            canonical_parts("[File: a.png]", vec![file("a.png")]).unwrap(),
            vec![file("a.png")]
        );
        let many = vec![file("a"); MAX_MESSAGE_FILES + 1];
        assert_eq!(
            canonical_parts(&flatten(&many), many),
            Err(InvalidMessage::TooManyFiles)
        );
    }

    #[test]
    fn attachment_names_and_types() {
        for bad in ["", " ", ".", "..", "a/b", "a\\b", "a\nb", &"x".repeat(256)] {
            assert_eq!(validate_name(bad), Err(InvalidAttachment::Name), "{bad:?}");
        }
        assert!(validate_name("Screenshot 2026-11-04 at 10.00.png").is_ok());
        assert_eq!(media_type("Image/PNG").unwrap(), "image/png");
        for bad in ["png", "image/", "text/plain; charset=utf-8", "a/b/c"] {
            assert_eq!(
                media_type(bad),
                Err(InvalidAttachment::MediaType),
                "{bad:?}"
            );
        }
    }
}
