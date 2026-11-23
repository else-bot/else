//! How an owner message that answers an earlier one is written, so the bot
//! reads the same reply from the desktop and the phone.

/// `text` as a Markdown block quote, blank lines kept.
pub fn quote(text: &str) -> String {
    text.trim_end()
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

/// A reply to one earlier message: what it answers, its quoted text, then
/// the owner's words. `subject` names the message, such as "your earlier
/// message" for the bot's.
pub fn replying_to(subject: &str, quoted: &str, body: &str) -> String {
    format!("Replying to {subject}:\n{}\n\n{body}", quote(quoted))
}

/// A reply as [`replying_to`] wrote it, read back: whose message it
/// quotes, the quoted block (with its "Replying to …:" line), and the
/// owner's words. `None` for any other message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Replying<'a> {
    /// It quotes the owner's own earlier message, not the bot's.
    pub owner: bool,
    pub quote: &'a str,
    pub body: &'a str,
}

impl<'a> Replying<'a> {
    pub fn parse(message: &'a str) -> Option<Self> {
        let (owner, rest) = [
            (true, "Replying to my earlier message:\n"),
            (false, "Replying to your earlier message:\n"),
        ]
        .into_iter()
        .find_map(|(owner, prefix)| message.strip_prefix(prefix).map(|rest| (owner, rest)))?;
        let (quote, body) = rest.split_once("\n\n")?;
        (!quote.is_empty()
            && !body.trim().is_empty()
            && quote
                .lines()
                .all(|line| line == ">" || line.starts_with("> ")))
        .then_some(Self {
            owner,
            quote: &message[..message.len() - body.len() - 2],
            body,
        })
    }

    /// The quoted message's text as the conversation stored it.
    pub fn original(&self) -> String {
        self.quote
            .lines()
            .skip(1)
            .map(|line| {
                line.strip_prefix("> ")
                    .or_else(|| line.strip_prefix('>'))
                    .unwrap_or(line)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A reaction's mark.
pub fn mark(kind: else_wire::items::ReactionKind) -> &'static str {
    match kind {
        else_wire::items::ReactionKind::ThumbsUp => "👍",
        else_wire::items::ReactionKind::ThumbsDown => "👎",
    }
}

/// The message a reaction sends the bot: the mark, the message it is
/// about quoted, and how the bot should take it.
pub fn reaction(kind: else_wire::items::ReactionKind, subject: &str, quoted: &str) -> String {
    let request = match kind {
        else_wire::items::ReactionKind::ThumbsUp => {
            "I reacted 👍 to this message. This is approval, not an instruction, so do not \
             save a rule from it. Do not send an acknowledgement. Only reply if there is \
             something actionable to tell me or a question that needs my answer."
        }
        else_wire::items::ReactionKind::ThumbsDown => {
            "I reacted 👎 to this message. Don't assume a cause. Reply with one short \
             question asking me what went wrong, then use my answer to improve."
        }
    };
    format!(
        "{} Feedback on {subject}:\n{}\n\n{request}",
        mark(kind),
        quote(quoted)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_reads_back_as_its_quote_and_words() {
        let sent = replying_to(
            "your earlier message",
            "Blocked.\n\nTried: dig",
            "Use 1.1.1.1",
        );
        let read = Replying::parse(&sent).unwrap();
        assert!(!read.owner);
        assert_eq!(read.original(), "Blocked.\n\nTried: dig");
        assert_eq!(read.body, "Use 1.1.1.1");
        assert_eq!(
            Replying::parse("Replying to your earlier message:\nplain\n\nx"),
            None
        );
    }

    #[test]
    fn a_reply_quotes_every_line_of_the_message_it_answers() {
        assert_eq!(
            replying_to(
                "your earlier message",
                "Blocked on DNS.\n\nTried: dig\n",
                "Use 1.1.1.1"
            ),
            "Replying to your earlier message:\n> Blocked on DNS.\n>\n> Tried: dig\n\nUse 1.1.1.1"
        );
    }
}
