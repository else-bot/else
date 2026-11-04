use crate::model::ReactionKind;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Author {
    Owner,
    Bot,
}

impl Author {
    pub(super) fn of(kind: &str) -> Option<Self> {
        match kind {
            "user" => Some(Author::Owner),
            "assistant" => Some(Author::Bot),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MessageReference {
    pub(super) item: Uuid,
    pub(super) author: Author,
    pub(super) text: String,
}

impl MessageReference {
    fn subject(&self) -> &'static str {
        match self.author {
            Author::Owner => "my earlier message",
            Author::Bot => "your earlier message",
        }
    }

    fn quote(&self) -> String {
        self.text
            .trim_end()
            .lines()
            .map(|line| {
                if line.is_empty() {
                    ">".to_owned()
                } else {
                    format!("> {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Draft {
    pub(super) text: String,
    pub(super) reply_to: Option<MessageReference>,
}

impl Draft {
    pub(super) fn message(&self) -> Option<String> {
        if self.text.trim().is_empty() {
            return None;
        }
        Some(match &self.reply_to {
            None => self.text.clone(),
            Some(reference) => format!(
                "Replying to {}:\n{}\n\n{}",
                reference.subject(),
                reference.quote(),
                self.text
            ),
        })
    }
}

impl ReactionKind {
    pub(super) fn mark(self) -> &'static str {
        match self {
            ReactionKind::ThumbsUp => "👍",
            ReactionKind::ThumbsDown => "👎",
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            ReactionKind::ThumbsUp => "👍 Thumbs up",
            ReactionKind::ThumbsDown => "👎 Thumbs down",
        }
    }

    pub(super) fn message(self, reference: &MessageReference) -> String {
        let request = match self {
            ReactionKind::ThumbsUp => {
                "This worked well for me. Work out what made it work, and remember that \
                 behavior so you keep doing it in similar situations."
            }
            ReactionKind::ThumbsDown => {
                "This didn't work for me. Ask me what went wrong and how you can correct it. \
                 Don't assume a cause. Use my feedback to improve."
            }
        };
        format!(
            "{} Feedback on {}:\n{}\n\n{request}",
            self.mark(),
            reference.subject(),
            reference.quote()
        )
    }
}

pub(super) fn too_long(message: &str, limit: usize) -> Option<String> {
    (message.len() > limit).then(|| {
        format!(
            "The message is too long ({} KiB maximum, including any quoted message).",
            limit / 1024
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference(author: Author, text: &str) -> MessageReference {
        MessageReference {
            item: Uuid::nil(),
            author,
            text: text.to_owned(),
        }
    }

    fn draft(text: &str, reply_to: Option<MessageReference>) -> Draft {
        Draft {
            text: text.to_owned(),
            reply_to,
        }
    }

    #[test]
    fn a_plain_draft_is_sent_as_typed() {
        assert_eq!(
            draft("  hello\nthere ", None).message().as_deref(),
            Some("  hello\nthere ")
        );
    }

    #[test]
    fn a_reply_needs_the_owners_own_text() {
        let reply = Some(reference(Author::Bot, "Done."));
        assert_eq!(draft(" \n\t", reply).message(), None);
        assert_eq!(draft("", None).message(), None);
    }

    #[test]
    fn a_reply_quotes_every_line_of_the_whole_message_before_the_owners_text() {
        let reply = reference(Author::Bot, "First line\n\n> already quoted\r\nlast\n\n");
        let message = draft("Why?\nPlease explain.", Some(reply))
            .message()
            .unwrap();
        assert_eq!(
            message,
            "Replying to your earlier message:\n\
             > First line\n\
             >\n\
             > > already quoted\n\
             > last\n\
             \n\
             Why?\nPlease explain."
        );
    }

    #[test]
    fn a_reply_names_who_wrote_the_quoted_message() {
        let own = draft("Still true?", Some(reference(Author::Owner, "Use tabs.")))
            .message()
            .unwrap();
        let bots = draft("Still true?", Some(reference(Author::Bot, "Use tabs.")))
            .message()
            .unwrap();
        assert!(own.starts_with("Replying to my earlier message:\n> Use tabs.\n\n"));
        assert!(bots.starts_with("Replying to your earlier message:\n> Use tabs.\n\n"));
    }

    #[test]
    fn a_thumbs_up_quotes_the_message_and_asks_the_bot_to_remember_what_worked() {
        let message = ReactionKind::ThumbsUp.message(&reference(Author::Bot, "Step 1\nStep 2"));
        assert!(
            message.starts_with("👍 Feedback on your earlier message:\n> Step 1\n> Step 2\n\n")
        );
        assert!(message.contains("what made it work"));
        assert!(message.contains("remember"));
    }

    #[test]
    fn a_thumbs_down_asks_what_went_wrong_without_naming_a_cause() {
        let message = ReactionKind::ThumbsDown.message(&reference(Author::Bot, "Step 1"));
        assert!(message.starts_with("👎 Feedback on your earlier message:\n> Step 1\n\n"));
        assert!(message.contains("Don't assume a cause"));
        assert!(message.contains("what went wrong"));
        assert!(message.contains("Ask me what went wrong and how you can correct it"));
    }

    #[test]
    fn the_size_limit_counts_the_quote_in_utf8_bytes() {
        let quoted = reference(Author::Bot, &"é".repeat(600));
        let message = draft("ok", Some(quoted)).message().unwrap();
        assert!(message.chars().count() < 1024);
        assert!(too_long(&message, 1024).is_some());
        assert_eq!(too_long("ok", 1024), None);
        assert_eq!(too_long(&"a".repeat(1024), 1024), None);
    }
}
