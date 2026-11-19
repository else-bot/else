//! Where long text is cut before a "Show all" or "Show more".

/// The byte `chars` characters into `text`, or its length when shorter.
pub(super) fn char_end(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(at, _)| at)
}

/// `end` moved back to the last break between words in `text`, with the
/// space before it trimmed. One word that fills everything up to `end` is
/// cut at `end`.
pub(super) fn word_end(text: &str, end: usize) -> usize {
    let head = &text[..end];
    if end == text.len() || text[end..].starts_with(char::is_whitespace) {
        return head.trim_end().len();
    }
    match head.rfind(char::is_whitespace) {
        Some(at) if !head[..at].trim().is_empty() => head[..at].trim_end().len(),
        _ => end,
    }
}

/// Whether hiding `hidden` is worth a control to reveal it: more than one
/// line, or at least a quarter of the `chars` budget. A code fence's own
/// marker line draws nothing, so it does not count.
pub(super) fn hides_enough(hidden: &str, chars: usize) -> bool {
    let lines = hidden
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("```"))
        .count();
    lines > 1 || hidden.trim().chars().count() >= chars / 4
}
