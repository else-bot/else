//! Owner message rules.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_TEXT_BYTES: usize = 32 * 1024;
/// Waiting (not yet started) messages allowed per bot.
pub const MAX_QUEUED_TURNS: i64 = 4;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidMessage {
    #[error("Message text must not be empty.")]
    Empty,
    #[error("Message text must be at most 32 KiB.")]
    TooLarge,
    #[error("Message text must not contain NUL characters.")]
    Nul,
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
}
