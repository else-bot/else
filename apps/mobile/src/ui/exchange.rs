//! Work handed between bots, as a card in the chat: the task another bot
//! sent this one, or the answer to a task this one sent, with its status.
//! Its header opens the other bot's chat; a long answer folds.

use else_wire::items::BotReplyStatus;
use nsh_base::{MarkdownOptions, text_view};
use nsh_core::prelude::*;

use super::parts::colors;
use crate::conversation::{Exchange, ExchangeKind};

/// Characters of an answer shown before it folds.
const FOLDED: usize = 600;

fn status_line(status: &BotReplyStatus, reason: Option<&str>) -> (String, bool) {
    match status {
        BotReplyStatus::Completed => ("Done".to_owned(), false),
        BotReplyStatus::Cancelled => ("Stopped".to_owned(), false),
        BotReplyStatus::Failed => (
            reason.map_or_else(|| "Failed".to_owned(), |r| format!("Failed: {r}")),
            true,
        ),
        BotReplyStatus::Other(other) => (other.clone(), false),
    }
}

/// The first `FOLDED` characters of `text`, cut at a word.
fn folded(text: &str) -> Option<String> {
    if text.chars().count() <= FOLDED {
        return None;
    }
    let cut: String = text.chars().take(FOLDED).collect();
    let cut = cut
        .rsplit_once(char::is_whitespace)
        .map_or(cut.as_str(), |(head, _)| head);
    Some(format!("{}…", cut.trim_end()))
}

pub fn card(exchange: Exchange) -> impl IntoElement {
    let open = signal(false);
    let peer = exchange.peer;
    let peer_name = exchange.peer_name.clone();
    let heading = match exchange.kind {
        ExchangeKind::Received => format!("Task from {}", exchange.peer_name),
        ExchangeKind::Answered { .. } => format!("Answer from {}", exchange.peer_name),
    };
    let open_peer = move || {
        let name = peer_name.clone();
        super::push(format!("chat-{peer}"), move || {
            super::chat::screen(peer, name.clone())
        });
    };
    let (status, answer) = match &exchange.kind {
        ExchangeKind::Answered {
            status,
            answer,
            reason,
        } => (
            Some(status_line(status, reason.as_deref())),
            Some(answer.clone()),
        ),
        ExchangeKind::Received => (None, None),
    };
    // A received task is its prompt; an answer shows the task in short.
    let prompt_lines = if answer.is_some() { 2 } else { 12 };
    view()
        .mx_4()
        .my_2()
        .p_4()
        .gap_2()
        .rounded(px(16.0))
        .border_curve(BorderCurve::Continuous)
        .bg(colors::card())
        .border_1()
        .border_color(colors::separator())
        .child(
            view()
                .flex_row()
                .items_center()
                .gap_2()
                .role(AccessibilityRole::Button)
                .label(format!("{heading}, open its chat"))
                .active(|s| s.opacity(0.6))
                .on_press(open_peer)
                .child(
                    text(heading.to_uppercase())
                        .flex_1()
                        .text_size(px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors::tint()),
                )
                .child(
                    image(super::parts::CHEVRON_RIGHT)
                        .size(px(12.0))
                        .tint_color(colors::tint()),
                ),
        )
        .child(exchange.title.clone().map(|title| {
            text(title)
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label())
        }))
        .child(status.map(|(line, failed)| {
            text(line).text_size(px(13.0)).text_color(if failed {
                colors::destructive()
            } else {
                colors::secondary()
            })
        }))
        .child(
            text(exchange.prompt.clone())
                .number_of_lines(prompt_lines)
                .selectable(true)
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(if answer.is_some() {
                    colors::secondary()
                } else {
                    colors::label()
                }),
        )
        .child(answer.filter(|a| !a.trim().is_empty()).map(|answer| {
            let short = folded(&answer);
            let foldable = short.is_some();
            view()
                .gap_2()
                .pt_1()
                .border_t(px(0.5))
                .border_color(colors::separator())
                .child(
                    text_view()
                        .markdown(move || {
                            SharedString::from(match (&short, open.get()) {
                                (Some(short), false) => short.clone(),
                                _ => answer.clone(),
                            })
                        })
                        .options(MarkdownOptions::default)
                        .selectable(true)
                        .w_full()
                        .gap_2()
                        .text_size(px(15.0))
                        .line_height(px(21.0))
                        .text_color(colors::label()),
                )
                .child(foldable.then(|| {
                    text(move || if open.get() { "Show less" } else { "Show all" })
                        .text_size(px(15.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(colors::tint())
                        .role(AccessibilityRole::Button)
                        .on_press(move || open.update(|o| *o = !*o))
                }))
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_answer_folds_at_a_word() {
        assert_eq!(folded("short"), None);
        let long = "word ".repeat(200);
        let short = folded(&long).unwrap();
        assert!(short.ends_with("word…"), "{short}");
        assert!(short.chars().count() <= FOLDED + 1);
    }
}
