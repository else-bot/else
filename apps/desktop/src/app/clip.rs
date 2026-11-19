//! Where long text is cut before a "Show all" or "Show more".

/// Where `text` ends when cut to at most `chars` characters, between words
/// where it can be. The whole length when it fits.
pub(super) fn word_cut(text: &str, chars: usize) -> usize {
    let Some((limit, _)) = text.char_indices().nth(chars) else {
        return text.len();
    };
    let head = &text[..limit];
    if text[limit..].starts_with(char::is_whitespace) {
        return head.trim_end().len();
    }
    match head.rfind(char::is_whitespace) {
        Some(at) if !head[..at].trim().is_empty() => head[..at].trim_end().len(),
        // One word longer than the budget: cut it.
        _ => limit,
    }
}

/// Whether hiding `hidden` is worth a control to reveal it: more than one
/// line, or at least a quarter of the `chars` budget the cut kept.
pub(super) fn hides_enough(hidden: &str, chars: usize) -> bool {
    let hidden = hidden.trim();
    hidden.lines().count() > 1 || hidden.chars().count() >= chars / 4
}
