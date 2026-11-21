use super::attachments::DraftFile;
use crate::model::ReactionKind;
use else_wire::items::ItemKind;
use gpui_kit::component::input::InputContent;
use std::collections::HashMap;
use std::ops::Range;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Author {
    Owner,
    Bot,
}

impl Author {
    pub(super) fn of(kind: ItemKind) -> Option<Self> {
        match kind {
            ItemKind::User => Some(Author::Owner),
            ItemKind::Assistant => Some(Author::Bot),
            ItemKind::Status | ItemKind::Routine | ItemKind::Unknown => None,
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
        Self::subject_of(self.author)
    }

    fn subject_of(author: Author) -> &'static str {
        match author {
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

/// How a quote entered the draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QuoteKind {
    /// A whole message picked with Reply; a draft holds at most one, and the
    /// composer's own text answers it.
    Reply,
    /// Text the owner selected and cited, which may carry its own comment.
    Passage,
}

/// A passage of an earlier message the owner is answering, with what they
/// want to say about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Quote {
    pub(super) id: Uuid,
    pub(super) kind: QuoteKind,
    pub(super) source: MessageReference,
    /// The quoted span in the source row's rendered text, for highlighting.
    pub(super) highlight: Option<Range<usize>>,
    pub(super) comment: String,
    /// The owner ask this reply answers, so sending settles that ask only.
    pub(super) answers: Option<Uuid>,
}

impl Quote {
    pub(super) fn reply(source: MessageReference) -> Self {
        Self::new(QuoteKind::Reply, source, None)
    }

    pub(super) fn passage(source: MessageReference, highlight: Option<Range<usize>>) -> Self {
        Self::new(QuoteKind::Passage, source, highlight)
    }

    /// A reply to the bot's ask `notice`, quoting its text so the bot knows
    /// which ask the answer is for. `settles` is false for servers that
    /// cannot settle one ask.
    pub(super) fn ask(notice: Uuid, text: String, settles: bool) -> Self {
        Self {
            answers: settles.then_some(notice),
            ..Self::reply(MessageReference {
                item: notice,
                author: Author::Bot,
                text,
            })
        }
    }

    fn new(kind: QuoteKind, source: MessageReference, highlight: Option<Range<usize>>) -> Self {
        Self {
            id: Uuid::new_v4(),
            kind,
            source,
            highlight,
            comment: String::new(),
            answers: None,
        }
    }

    pub(super) fn excerpt(&self) -> String {
        flatten(&self.source.text)
    }
}

/// The quotes a sent message was written with, kept so an edit of a
/// rejected send gets them back as quotes, with the ask a reply answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Quoted {
    pub(super) quotes: Vec<Quote>,
    /// Bytes of the message the quotes take before the owner's own text.
    pub(super) prefix: usize,
}

impl Quoted {
    /// The ask the reply answers, if it replies to one.
    pub(super) fn answers(&self) -> Option<Uuid> {
        self.quotes.iter().find_map(|quote| quote.answers)
    }
}

#[derive(Debug, Clone)]
pub(super) struct Draft {
    pub(super) content: InputContent,
    pub(super) files: HashMap<Uuid, DraftFile>,
    pub(super) quotes: Vec<Quote>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            content: InputContent::new(""),
            files: HashMap::new(),
            quotes: Vec::new(),
        }
    }
}

impl Draft {
    /// Makes `reply` the draft's one whole-message reply, in place of any
    /// earlier one, keeping cited passages.
    pub(super) fn set_reply(&mut self, reply: Quote) {
        match self.quotes.iter_mut().find(|q| q.kind == QuoteKind::Reply) {
            Some(existing) => *existing = reply,
            None => self.quotes.push(reply),
        }
    }

    /// The draft's quotes as sent with `message`, which starts with their
    /// text and ends with the composer's own.
    pub(super) fn quoted(&self, message: &str) -> Option<Quoted> {
        (!self.quotes.is_empty()).then(|| Quoted {
            quotes: self.quotes.clone(),
            prefix: message.len() - self.content.text().len(),
        })
    }

    pub(super) fn has_unsaved_work(&self) -> bool {
        self.message().is_some()
            || super::attachments::active_file_ids(&self.content)
                .next()
                .is_some()
            || !self.quotes.is_empty()
    }

    /// The text to send, which always ends with the composer's own text so
    /// file parts can be laid after the quoted prefix.
    pub(super) fn message(&self) -> Option<String> {
        let note = self.content.text();
        let has_note = !note.trim().is_empty();
        match self.quotes.as_slice() {
            [] => has_note.then(|| note.to_string()),
            [quote] => {
                let comment = quote.comment.trim();
                let body = match (comment.is_empty(), has_note) {
                    (true, false) => return None,
                    (true, true) => note.to_string(),
                    (false, false) => comment.to_owned(),
                    (false, true) => format!("{comment}\n\n{note}"),
                };
                Some(format!(
                    "Replying to {}:\n{}\n\n{body}",
                    quote.source.subject(),
                    quote.source.quote()
                ))
            }
            quotes => {
                if !has_note && quotes.iter().all(|q| q.comment.trim().is_empty()) {
                    return None;
                }
                let mut message = format!("Replying to {} passages:", quotes.len());
                for (ix, quote) in quotes.iter().enumerate() {
                    message.push_str(&format!(
                        "\n\n[{}] From {}:\n{}",
                        ix + 1,
                        quote.source.subject(),
                        quote.source.quote()
                    ));
                    let comment = quote.comment.trim();
                    if !comment.is_empty() {
                        message.push_str("\n\n");
                        message.push_str(comment);
                    }
                }
                if has_note {
                    message.push_str("\n\n---\n\n");
                    message.push_str(note);
                }
                Some(message)
            }
        }
    }
}

pub(super) fn flatten(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One quoted passage of a sent multi-passage reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Passage {
    pub author: Author,
    /// The header line through the last quoted line.
    pub block: Range<usize>,
    pub original: String,
}

impl Passage {
    /// Every quoted passage of a sent reply, with the span before the first
    /// one (the "Replying to N passages:" line and its blank line), or `None`
    /// for any other message.
    pub(super) fn parse_all(message: &str) -> Option<(Range<usize>, Vec<Self>)> {
        let first = message.split_once('\n').map_or(message, |(line, _)| line);
        let count: usize = first
            .strip_prefix("Replying to ")?
            .strip_suffix(" passages:")?
            .parse()
            .ok()
            // Each passage takes several bytes, so a larger count is not one.
            .filter(|count| (2..=message.len()).contains(count))?;
        let mut passages = Vec::new();
        let mut at = first.len();
        for number in 1..=count {
            let mut header = None;
            for author in [Author::Bot, Author::Owner] {
                let line = format!(
                    "\n\n[{number}] From {}:\n",
                    MessageReference::subject_of(author)
                );
                if let Some(offset) = message[at..].find(&line) {
                    header = header.filter(|(earlier, _, _)| *earlier < offset).or(Some((
                        offset,
                        line.len(),
                        author,
                    )));
                }
            }
            let (offset, len, author) = header.filter(|(offset, ..)| number > 1 || *offset == 0)?;
            let start = at + offset + 2;
            let mut end = at + offset + len;
            let mut original = Vec::new();
            for line in message[end..].split('\n') {
                let Some(quoted) = line.strip_prefix('>') else {
                    break;
                };
                original.push(quoted.trim_prefix(' '));
                end += line.len() + 1;
            }
            if original.is_empty() {
                return None;
            }
            end = end.min(message.len());
            if message.as_bytes()[end - 1] == b'\n' {
                end -= 1;
            }
            passages.push(Self {
                author,
                block: start..end,
                original: original.join("\n"),
            });
            at = end;
        }
        Some((0..passages[0].block.start, passages))
    }

    pub(super) fn excerpt(&self) -> String {
        flatten(&self.original)
    }
}

/// Whether `original` is all of `text`, an earlier message.
pub(super) fn quotes_whole(text: &str, original: &str) -> bool {
    text.trim_end().lines().eq(original.lines())
}

/// Whether `text`, an earlier message, shows `original`, a part of it as the
/// owner selected it on screen: without Markdown, ignoring spacing and
/// punctuation.
pub(super) fn quotes_part(text: &str, original: &str) -> bool {
    let needle = letters(original);
    !needle.is_empty() && letters(&rendered(text)).contains(&needle)
}

/// Where `quote` appears in `shown`, the text a message renders, matched as
/// [`quotes_part`] matches it.
pub(super) fn passage_range(shown: &str, quote: &str) -> Option<Range<usize>> {
    let needle: Vec<char> = letters(quote).chars().collect();
    let hay: Vec<(usize, char)> = shown
        .char_indices()
        .filter(|(_, c)| c.is_alphanumeric())
        .flat_map(|(at, c)| c.to_lowercase().map(move |lower| (at, lower)))
        .collect();
    if needle.is_empty() {
        return None;
    }
    let start = hay
        .windows(needle.len())
        .position(|window| window.iter().map(|(_, c)| *c).eq(needle.iter().copied()))?;
    let (first, _) = hay[start];
    let (last, _) = hay[start + needle.len() - 1];
    let end = shown[last..]
        .chars()
        .next()
        .map_or(last, |c| last + c.len_utf8());
    Some(first..end)
}

fn letters(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The text a Markdown message shows, without link destinations or markup.
fn rendered(text: &str) -> String {
    fn visit(node: &markdown::mdast::Node, out: &mut String) {
        use markdown::mdast::Node;
        match node {
            Node::Text(t) => out.push_str(&t.value),
            Node::InlineCode(c) => out.push_str(&c.value),
            Node::Code(c) => out.push_str(&c.value),
            _ => {}
        }
        for child in node.children().into_iter().flatten() {
            visit(child, out);
        }
        out.push(' ');
    }
    let Ok(root) = markdown::to_mdast(text, &markdown::ParseOptions::gfm()) else {
        return text.to_owned();
    };
    let mut out = String::new();
    visit(&root, &mut out);
    out
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

    /// The quoted message's text as the conversation stored it.
    pub(super) fn original(&self) -> String {
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

    pub(super) fn excerpt(&self) -> String {
        flatten(
            &self
                .quote
                .lines()
                .skip(1)
                .filter_map(|line| line.strip_prefix('>'))
                .collect::<Vec<_>>()
                .join(" "),
        )
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
                "I reacted 👍 to this message. This is approval, not an instruction, so do not \
                 save a rule from it. Do not send an acknowledgement. Only reply if there is \
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

    #[test]
    fn a_passage_is_found_in_the_rendered_text_ignoring_spacing_and_punctuation() {
        let shown = "Fixed market list. BTC, ETH, SOL, HYPE and NVDA, nothing added.";
        let range = passage_range(shown, "btc eth sol").unwrap();
        assert_eq!(&shown[range], "BTC, ETH, SOL");
        let range = passage_range(shown, "list.\nBTC").unwrap();
        assert_eq!(&shown[range], "list. BTC");
    }

    #[test]
    fn a_passage_range_covers_whole_characters() {
        let shown = "Café: déjà vu";
        let range = passage_range(shown, "café déjà").unwrap();
        assert_eq!(&shown[range], "Café: déjà");
    }

    #[test]
    fn a_missing_or_empty_passage_has_no_range() {
        assert_eq!(passage_range("Fixed market list", "stocks"), None);
        assert_eq!(passage_range("Fixed market list", "..."), None);
    }

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
            quotes: reply_to.map(Quote::reply).into_iter().collect(),
        }
    }

    #[test]
    fn a_new_reply_replaces_the_earlier_one_and_keeps_cited_passages() {
        let mut draft = passages("", &[(Author::Bot, "Cited.", "Why?")]);
        draft.set_reply(Quote::reply(reference(Author::Bot, "First message.")));
        draft.set_reply(Quote::reply(reference(Author::Owner, "Second message.")));
        let quotes: Vec<_> = draft
            .quotes
            .iter()
            .map(|q| (q.kind, q.source.text.as_str()))
            .collect();
        assert_eq!(
            quotes,
            [
                (QuoteKind::Passage, "Cited."),
                (QuoteKind::Reply, "Second message."),
            ]
        );
    }

    fn passages(note: &str, quotes: &[(Author, &str, &str)]) -> Draft {
        Draft {
            content: InputContent::new(note.to_owned()),
            files: HashMap::new(),
            quotes: quotes
                .iter()
                .map(|(author, text, comment)| Quote {
                    comment: (*comment).to_owned(),
                    ..Quote::passage(reference(*author, text), None)
                })
                .collect(),
        }
    }

    #[test]
    fn one_commented_passage_is_sent_as_a_plain_reply() {
        let message = passages("", &[(Author::Bot, "run it first", "On dev.")])
            .message()
            .unwrap();
        assert_eq!(
            message,
            "Replying to your earlier message:\n> run it first\n\nOn dev."
        );
        assert_eq!(QuotedReply::parse(&message).unwrap().body, "On dev.");
        let both = passages("Thanks", &[(Author::Bot, "run it first", "On dev.")])
            .message()
            .unwrap();
        assert!(both.ends_with("\n\nOn dev.\n\nThanks"));
    }

    #[test]
    fn several_passages_number_each_quote_and_end_with_the_note() {
        let draft = passages(
            "Ship it after.",
            &[
                (
                    Author::Bot,
                    "Hide internal rows.\nKeep counts.",
                    "Only on hover.",
                ),
                (Author::Owner, "Use tabs.", ""),
            ],
        );
        let message = draft.message().unwrap();
        assert_eq!(
            message,
            "Replying to 2 passages:\n\n\
             [1] From your earlier message:\n\
             > Hide internal rows.\n\
             > Keep counts.\n\n\
             Only on hover.\n\n\
             [2] From my earlier message:\n\
             > Use tabs.\n\n\
             ---\n\n\
             Ship it after."
        );
        let (header, found) = Passage::parse_all(&message).unwrap();
        assert_eq!(&message[header], "Replying to 2 passages:\n\n");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].author, Author::Bot);
        assert_eq!(found[0].original, "Hide internal rows.\nKeep counts.");
        assert_eq!(
            &message[found[0].block.clone()],
            "[1] From your earlier message:\n> Hide internal rows.\n> Keep counts."
        );
        assert_eq!(found[1].author, Author::Owner);
        assert_eq!(
            &message[found[1].block.clone()],
            "[2] From my earlier message:\n> Use tabs."
        );
    }

    #[test]
    fn several_passages_need_a_comment_or_a_note() {
        let bare = [(Author::Bot, "a", ""), (Author::Bot, "b", "")];
        assert_eq!(passages(" ", &bare).message(), None);
        assert!(passages("why?", &bare).message().is_some());
        assert_eq!(passages("", &[(Author::Bot, "a", "  ")]).message(), None);
    }

    #[test]
    fn a_passage_reply_that_ends_on_a_quote_still_parses() {
        let message = passages("", &[(Author::Bot, "a", "x"), (Author::Bot, "b", "")])
            .message()
            .unwrap();
        let (_, found) = Passage::parse_all(&message).unwrap();
        assert_eq!(
            &message[found[1].block.clone()],
            "[2] From your earlier message:\n> b"
        );
        assert!(
            Passage::parse_all("Replying to 2 passages:\n\n[1] From your earlier message:\n> a")
                .is_none()
        );
        assert!(Passage::parse_all("Replying to your earlier message:\n> a\n\nb").is_none());
        let shifted = message.replacen("passages:\n\n", "passages:\n🥔\n\n", 1);
        assert!(Passage::parse_all(&shifted).is_none());
        let huge = message.replacen("2 passages", &format!("{} passages", usize::MAX), 1);
        assert!(Passage::parse_all(&huge).is_none());
    }

    #[test]
    fn a_quote_matches_its_message_without_markdown_or_line_breaks() {
        let message = "Plan:\n\n- **Hide** the rows,\n  then count them.";
        assert!(quotes_part(message, "Hide the rows, then count"));
        assert!(quotes_whole(message, message));
        assert!(!quotes_whole("Not done.", "Done."));
        assert!(!quotes_part(message, "Show the rows"));
        assert!(!quotes_part(message, "…"));
        let linked = "Click [settings](https://example.com) to continue.";
        assert!(quotes_part(linked, "Click settings to continue."));
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
        let original = "Use tabs.\n\n  Indented, kept.";
        let message = draft(
            "Sure.",
            Some(MessageReference {
                item: Uuid::nil(),
                author: Author::Bot,
                text: format!("{original}\n"),
            }),
        )
        .message()
        .unwrap();
        assert_eq!(QuotedReply::parse(&message).unwrap().original(), original);
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
    fn a_thumbs_up_quotes_the_message_without_asking_for_a_rule() {
        let message = ReactionKind::ThumbsUp.message(&reference(Author::Bot, "Step 1\nStep 2"));
        assert!(
            message.starts_with("👍 Feedback on your earlier message:\n> Step 1\n> Step 2\n\n")
        );
        assert!(message.contains("do not save a rule"));
        assert!(!message.contains("remember"));
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

    #[test]
    fn a_reply_to_an_ask_quotes_it_and_comes_back_from_its_text() {
        let notice = Uuid::new_v4();
        let mut answer = draft("Approve it.", None);
        answer.set_reply(Quote::ask(notice, "Approve the Romow DM?".into(), true));
        let message = answer.message().unwrap();
        let quoted = answer.quoted(&message).unwrap();
        assert_eq!(quoted.answers(), Some(notice));
        assert_eq!(&message[quoted.prefix..], "Approve it.");
        let quoted = QuotedReply::parse(&message).unwrap();
        assert_eq!(quoted.author, Author::Bot);
        assert_eq!(quoted.original(), "Approve the Romow DM?");
        assert_eq!(quoted.body, "Approve it.");

        let mut legacy = draft("Approve it.", None);
        legacy.set_reply(Quote::ask(notice, "Approve the Romow DM?".into(), false));
        let legacy_message = legacy.message().unwrap();
        assert_eq!(
            legacy.quoted(&legacy_message).unwrap().answers(),
            None,
            "an older server settles by itself"
        );
        assert_eq!(legacy_message, message);
    }

    #[test]
    fn an_ask_reply_with_a_cited_passage_keeps_its_quotes_and_target() {
        let notice = Uuid::new_v4();
        let mut answer = passages("Ship it.", &[(Author::Bot, "Retries persist.", "Why?")]);
        answer.set_reply(Quote::ask(notice, "Approve the migration?".into(), true));
        let message = answer.message().unwrap();
        assert!(message.starts_with("Replying to 2 passages:"), "{message}");
        let quoted = answer.quoted(&message).unwrap();
        assert_eq!(quoted.answers(), Some(notice));
        assert_eq!(quoted.quotes, answer.quotes);
        assert_eq!(&message[quoted.prefix..], "Ship it.");
    }
}
