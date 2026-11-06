use super::attachments::DraftFile;
use crate::model::ReactionKind;
use gpui_kit::component::input::InputContent;
use std::collections::HashMap;
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

#[derive(Debug, Clone)]
pub(super) struct Draft {
    pub(super) content: InputContent,
    pub(super) files: HashMap<Uuid, DraftFile>,
    pub(super) reply_to: Option<MessageReference>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            content: InputContent::new(""),
            files: HashMap::new(),
            reply_to: None,
        }
    }
}

impl Draft {
    pub(super) fn message(&self) -> Option<String> {
        if self.content.text().trim().is_empty() {
            return None;
        }
        Some(match &self.reply_to {
            None => self.content.text().to_string(),
            Some(reference) => format!(
                "Replying to {}:\n{}\n\n{}",
                reference.subject(),
                reference.quote(),
                self.content.text()
            ),
        })
    }
}

pub(super) struct QuotedReply<'a> {
    pub author: Author,
    pub quote: &'a str,
    pub body: &'a str,
}

impl<'a> QuotedReply<'a> {
    pub(super) fn parse(message: &'a str) -> Option<Self> {
        let (author, rest) = [
            (Author::Owner, "Replying to my earlier message:\n"),
            (Author::Bot, "Replying to your earlier message:\n"),
        ]
        .into_iter()
        .find_map(|(author, prefix)| message.strip_prefix(prefix).map(|rest| (author, rest)))?;
        let (quote, body) = rest.split_once("\n\n")?;
        (!quote.is_empty()
            && !body.trim().is_empty()
            && quote
                .lines()
                .all(|line| line == ">" || line.starts_with("> ")))
        .then_some(Self {
            author,
            quote: &message[..message.len() - body.len() - 2],
            body,
        })
    }

    pub(super) fn excerpt(&self) -> String {
        self.quote
            .lines()
            .skip(1)
            .filter_map(|line| line.strip_prefix('>'))
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl ReactionKind {
    pub(super) fn mark(self) -> &'static str {
        match self {
            ReactionKind::ThumbsUp => "👍",
            ReactionKind::ThumbsDown => "👎",
        }
    }

    pub(super) fn message(self, reference: &MessageReference) -> String {
        let request = match self {
            ReactionKind::ThumbsUp => {
                "I reacted 👍 to this message. Work out what made it work, and remember that \
                 behavior so you keep doing it in similar situations. Do not send an \
                 acknowledgement or explain what you remembered. Only reply if there is \
                 something actionable to tell me or a question that needs my answer."
            }
            ReactionKind::ThumbsDown => {
                "I reacted 👎 to this message. Don't assume a cause. Reply with one short \
                 question asking me what went wrong, then use my answer to improve."
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
            content: InputContent::new(text.to_owned()),
            files: HashMap::new(),
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
    fn a_reply_preview_names_the_author_and_flattens_multiline_quotes() {
        for author in [Author::Owner, Author::Bot] {
            let message = draft(
                "Done",
                Some(reference(
                    author,
                    " HTTPS is enabled.\n\nRéglé.\n> Earlier quote.",
                )),
            )
            .message()
            .unwrap();
            let reply = QuotedReply::parse(&message).unwrap();
            assert_eq!(reply.author, author);
            assert_eq!(reply.excerpt(), "HTTPS is enabled. Réglé. > Earlier quote.");
            assert_eq!(reply.body, "Done");
        }
    }

    #[test]
    fn a_thumbs_up_quotes_the_message_and_asks_the_bot_to_remember_what_worked() {
        let message = ReactionKind::ThumbsUp.message(&reference(Author::Bot, "Step 1\nStep 2"));
        assert!(
            message.starts_with("👍 Feedback on your earlier message:\n> Step 1\n> Step 2\n\n")
        );
        assert!(message.contains("what made it work"));
        assert!(message.contains("remember"));
        assert!(message.contains("Do not send an acknowledgement"));
        assert!(message.contains("Only reply if"));
    }

    #[test]
    fn a_thumbs_down_asks_what_went_wrong_without_naming_a_cause() {
        let message = ReactionKind::ThumbsDown.message(&reference(Author::Bot, "Step 1"));
        assert!(message.starts_with("👎 Feedback on your earlier message:\n> Step 1\n\n"));
        assert!(message.contains("Don't assume a cause"));
        assert!(message.contains("one short question asking me what went wrong"));
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
