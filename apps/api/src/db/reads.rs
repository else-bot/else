//! The owner's read position in each bot's owner conversation.
//!
//! Unread is derived, never counted: the latest assistant item's position
//! (its seq and the UTF-8 bytes of its text) is compared with the saved read
//! position, so text appended to a reply after the owner read it is unread
//! again. Reading only moves forward; marking unread sets a flag and keeps
//! the position, so a delayed read can never undo it. Attaching a reply's
//! linked files can rewrite its text shorter, so a position past the end of a
//! reply reads it through its current end.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use super::bots::{self, BotRecord, BotStatus};

/// A point in the bot's replies, ordered by item, then by text within it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyPosition {
    pub item_seq: i64,
    pub text_bytes: i64,
}

/// The newest assistant item of the owner conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LatestReply {
    pub turn_id: Uuid,
    pub item_id: Uuid,
    pub position: ReplyPosition,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReadState {
    pub latest: Option<LatestReply>,
    pub read_through: ReplyPosition,
    pub marked_unread: bool,
    pub unread: bool,
}

/// The read columns of a bot's row, as selected with it.
#[derive(Debug, Clone, Default, sqlx::FromRow)]
pub struct ReadRow {
    read_item_seq: i64,
    read_text_bytes: i64,
    marked_unread: bool,
    head_item_id: Option<Uuid>,
    head_turn_id: Option<Uuid>,
    head_seq: Option<i64>,
    head_bytes: Option<i64>,
    head_preview: Option<String>,
}

const PREVIEW_CHARS: usize = 160;

impl ReadRow {
    pub fn state(&self) -> ReadState {
        let read_through = ReplyPosition {
            item_seq: self.read_item_seq,
            text_bytes: self.read_text_bytes,
        };
        let latest = match (
            self.head_item_id,
            self.head_turn_id,
            self.head_seq,
            self.head_bytes,
        ) {
            (Some(item_id), Some(turn_id), Some(item_seq), Some(text_bytes)) => Some(LatestReply {
                turn_id,
                item_id,
                position: ReplyPosition {
                    item_seq,
                    text_bytes,
                },
                preview: preview(self.head_preview.as_deref().unwrap_or_default()),
            }),
            _ => None,
        };
        let unread =
            self.marked_unread || latest.as_ref().is_some_and(|l| l.position > read_through);
        ReadState {
            latest,
            read_through,
            marked_unread: self.marked_unread,
            unread,
        }
    }
}

pub(super) fn preview(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= PREVIEW_CHARS {
        return line;
    }
    let cut: String = line.chars().take(PREVIEW_CHARS).collect();
    let cut = match cut.rfind(' ') {
        Some(space) if space > PREVIEW_CHARS / 2 => &cut[..space],
        _ => &cut,
    };
    format!("{}…", cut.trim_end())
}

/// An owner change to the read state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReadCommand {
    /// The owner saw replies up to here. Never clears `marked_unread`.
    Through {
        position: ReplyPosition,
    },
    /// Mark read: clear the flag and read up to `position`, when given.
    MarkRead {
        #[serde(default)]
        position: Option<ReplyPosition>,
    },
    MarkUnread,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("bot not found")]
    NotFound,
    #[error("the bot was deleted")]
    Deleted,
    #[error("the position is not within one of the bot's replies")]
    InvalidPosition,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Apply `command` and return the bot with its new state. Retrying any
/// command converges on the same state.
pub async fn apply(
    pool: &SqlitePool,
    bot_id: Uuid,
    command: ReadCommand,
) -> Result<BotRecord, ReadError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let bot = bots::find_in(&mut tx, bot_id)
        .await?
        .ok_or(ReadError::NotFound)?;
    if bot.status != BotStatus::Active {
        return Err(ReadError::Deleted);
    }
    let (position, marked_unread) = match command {
        ReadCommand::Through { position } => (Some(position), None),
        ReadCommand::MarkRead { position } => (position, Some(false)),
        ReadCommand::MarkUnread => (None, Some(true)),
    };
    if let Some(mut position) = position {
        let bytes: Option<i64> = sqlx::query_scalar(
            "SELECT length(CAST(text AS BLOB)) FROM items
             WHERE conversation_id = ?1 AND seq = ?2 AND kind = 'assistant'",
        )
        .bind(bot.conversation_id)
        .bind(position.item_seq)
        .fetch_optional(&mut *tx)
        .await?;
        match bytes {
            Some(bytes) if position.text_bytes >= 0 => {
                position.text_bytes = position.text_bytes.min(bytes);
            }
            _ => return Err(ReadError::InvalidPosition),
        }
        sqlx::query(
            "UPDATE conversations SET read_item_seq = ?2, read_text_bytes = ?3
             WHERE id = ?1 AND (read_item_seq, read_text_bytes) < (?2, ?3)",
        )
        .bind(bot.conversation_id)
        .bind(position.item_seq)
        .bind(position.text_bytes)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(flag) = marked_unread {
        sqlx::query("UPDATE conversations SET marked_unread = ?2 WHERE id = ?1")
            .bind(bot.conversation_id)
            .bind(flag)
            .execute(&mut *tx)
            .await?;
    }
    let bot = bots::find_in(&mut tx, bot_id)
        .await?
        .ok_or(ReadError::NotFound)?;
    tx.commit().await?;
    Ok(bot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_order_by_item_then_text() {
        let at = |item_seq, text_bytes| ReplyPosition {
            item_seq,
            text_bytes,
        };
        assert!(at(3, 10) > at(3, 9));
        assert!(at(4, 0) > at(3, 900));
        assert_eq!(at(2, 5).max(at(2, 7)), at(2, 7));
    }

    #[test]
    fn previews_are_one_short_line() {
        assert_eq!(preview("  Hello\n\nworld  "), "Hello world");
        let cut = preview(&"abcdefghij ".repeat(40));
        assert!(cut.chars().count() <= PREVIEW_CHARS + 1);
        assert!(cut.ends_with(" abcdefghij…"), "cut at a word: {cut}");
        let wide = "✓".repeat(500);
        assert_eq!(preview(&wide).chars().count(), PREVIEW_CHARS + 1);
    }
}
