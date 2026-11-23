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
