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
