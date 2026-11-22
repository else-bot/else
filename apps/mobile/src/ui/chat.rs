//! One bot's chat: the transcript as bubbles, the bot's replies as
//! Markdown that streams in, and the composer.

use nsh_base::{MarkdownOptions, TextViewPart, TextViewParts, input, nav_page, text_view};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::{ARROW_UP, STOP, colors};
use super::session::{self, Chat};
use crate::api::ClientError;
use crate::conversation::Row;

/// The composer's message on its way to the server. A send whose outcome
/// is unknown keeps its request ID, so sending again cannot post it twice.
#[derive(Clone, PartialEq)]
enum Outbox {
    Idle,
    Sending { request_id: Uuid },
    Unknown { request_id: Uuid, message: String },
    Refused(String),
}

impl Outbox {
    fn request_id(&self) -> Uuid {
        match self {
            Self::Unknown { request_id, .. } => *request_id,
            _ => Uuid::new_v4(),
        }
    }
}

pub fn screen(bot: Uuid, name: String) -> Screen {
    let chat = session::open_chat(bot);
    let rows = computed(move || {
        chat.with(|c| match c {
            Chat::Ready(conversation) => conversation.rows(),
            _ => Vec::new(),
        })
    });
    let thinking = computed(move || chat.with(|c| matches!(c, Chat::Ready(c) if c.thinking())));
    let busy = computed(move || chat.with(|c| matches!(c, Chat::Ready(c) if c.busy())));
    keep_read(bot);

    let transcript = dynamic(move || {
        match chat.with(|c| match c {
            Chat::Loading => Some(None),
            Chat::Failed(message) => Some(Some(message.clone())),
            Chat::Ready(_) => None,
        }) {
            Some(None) => centered("Loading…".into()).into_element(),
            Some(Some(message)) => centered(message).into_element(),
            None => untracked(|| transcript(bot, chat, rows, thinking)).into_element(),
        }
    });

    nav_page()
        .key(format!("chat-{bot}"))
        .title(name)
        .header_large_title(false)
        .child(
            view()
                .flex_1()
                .bg(colors::chat())
                .pb_keyboard()
                .child(view().flex_1().child(transcript))
                .child(composer(bot, chat, busy)),
        )
}

/// Marks the bot read on the server whenever its list row turns unread
/// while this chat is open.
fn keep_read(bot: Uuid) {
    let unread = computed(move || {
        session::bots().with(|b| {
            b.list
                .iter()
                .any(|it| it.id == bot && it.read_state.as_ref().is_some_and(|r| r.unread))
        })
    });
    let marking = stored(false);
    effect(move || {
        if !unread.get() || marking.get() {
            return;
        }
        marking.replace(true);
        let client = session::server().client.clone();
        spawn(async move {
            let _ = client.mark_read(bot).await;
            marking.replace(false);
        });
    });
}

fn centered(message: String) -> impl IntoElement {
    view()
        .flex_1()
        .items_center()
        .justify_center()
        .px_6()
        .child(
            text(message)
                .text_size(px(15.0))
                .text_center()
                .text_color(colors::secondary()),
        )
}

/// A transcript row, or the typing indicator after the last one.
#[derive(Clone, PartialEq)]
enum Line {
    Row(Row),
    Typing,
}

fn transcript(
    bot: Uuid,
    chat: Signal<Chat>,
    rows: Memo<Vec<Row>>,
    thinking: Memo<bool>,
) -> impl IntoElement {
    let lines = computed(move || {
        let mut lines: Vec<Line> = rows.get().into_iter().map(Line::Row).collect();
        if thinking.get() {
            lines.push(Line::Typing);
        }
        lines
    });
    let count = lines.with_untracked(Vec::len);
    reply_parts();
    virtual_list(
        move || lines.with(Vec::len),
        move |at: Position| {
            let line = computed(move || lines.with(|l| l.get(at.index()).cloned()));
            let kind = computed(move || {
                line.with(|l| match l {
                    Some(Line::Row(Row::Owner { .. })) => 1,
                    Some(Line::Row(Row::Bot { .. })) => 2,
                    Some(Line::Row(Row::Note { .. })) => 3,
                    Some(Line::Typing) => 4,
                    None => 0,
                })
            });
            view().child(dynamic(move || {
                let kind = kind.get();
                untracked(|| match kind {
                    1 => Some(owner_bubble(line).into_element()),
                    2 => Some(bot_bubble(line).into_element()),
                    3 => Some(note(line).into_element()),
                    4 => Some(typing().into_element()),
                    _ => None,
                })
            }))
        },
    )
    .keys(move || {
        lines.with(|l| {
            l.iter()
                .map(|line| match line {
                    Line::Row(row) => row.key(),
                    Line::Typing => "typing".to_owned(),
                })
                .collect::<Vec<_>>()
        })
    })
    .estimated_item_size(px(80.0))
    .initial_scroll_index(count.saturating_sub(1))
    .flex_1()
    .content_padding_top(px(12.0))
    .content_padding_bottom(px(12.0))
    .keyboard_dismiss_mode(KeyboardDismissMode::Interactive)
    .keyboard_taps(KeyboardTaps::Handled)
    .follow_end(px(80.0))
    .on_load_more_at_start(move || session::load_older(bot, chat))
}

fn row_text(line: Memo<Option<Line>>) -> impl Fn() -> String + Copy {
    move || {
        line.with(|l| match l {
            Some(Line::Row(
                Row::Owner { text, .. } | Row::Bot { text, .. } | Row::Note { text, .. },
            )) => text.clone(),
            _ => String::new(),
        })
    }
}

fn owner_bubble(line: Memo<Option<Line>>) -> impl IntoElement {
    let problem = computed(move || {
        line.with(|l| match l {
            Some(Line::Row(Row::Owner { problem, .. })) => problem.clone(),
            _ => None,
        })
    });
    view()
        .px_4()
        .py(px(3.0))
        .items_end()
        .child(
            view()
                .max_w(relative(0.78))
                .px(px(14.0))
                .py(px(8.0))
                .rounded(px(18.0))
                .border_curve(BorderCurve::Continuous)
                .bg(colors::bubble_owner())
                .child(
                    text(row_text(line))
                        .selectable(true)
                        .text_size(px(17.0))
                        .line_height(px(22.0))
                        .text_color(rgb(0xffffff)),
                ),
        )
        .child(dynamic(move || {
            problem.get().map(|problem| {
                text(problem)
                    .pt_1()
                    .text_size(px(12.0))
                    .text_color(colors::destructive())
            })
        }))
}

fn bot_bubble(line: Memo<Option<Line>>) -> impl IntoElement {
    let streaming = computed(move || {
        line.with(|l| {
            matches!(
                l,
                Some(Line::Row(Row::Bot {
                    streaming: true,
                    ..
                }))
            )
        })
    });
    let markdown = row_text(line);
    view().w_full().px_4().pt(px(6.0)).pb(px(10.0)).child(
        text_view()
            .markdown(move || SharedString::from(markdown()))
            .options(move || MarkdownOptions {
                streaming: streaming.get(),
                ..Default::default()
            })
            .selectable(true)
            .w_full()
            .gap_3()
            .text_size(px(17.0))
            .line_height(px(24.0))
            .text_color(colors::label()),
    )
}

/// How Markdown parts look in replies: code and tables on a quiet fill.
fn reply_parts() {
    provide_context(
        TextViewParts::new()
            .part(TextViewPart::CodeBlock, |s| {
                s.rounded(px(12.0)).bg(colors::fill())
            })
            .part(TextViewPart::CodeBlockHeader, |s| s.px_3().pt_2())
            .part(TextViewPart::CodeBlockCode, |s| {
                s.p_3().text_size(px(14.0)).line_height(px(20.0))
            })
            .part(TextViewPart::Table, |s| {
                s.rounded(px(10.0))
                    .border_1()
                    .border_color(colors::separator())
            })
            .part(TextViewPart::TableHead, |s| s.bg(colors::fill()))
            .part(TextViewPart::TableCell, |s| {
                s.px_3()
                    .py_2()
                    .border_t(px(0.5))
                    .border_color(colors::separator())
            })
            .part(TextViewPart::Blockquote, |s| {
                s.border_l(px(3.0))
                    .border_color(colors::separator())
                    .pl_3()
                    .text_color(colors::secondary())
            })
            .part(TextViewPart::Link, |s| s.text_color(colors::tint())),
    );
}

fn note(line: Memo<Option<Line>>) -> impl IntoElement {
    view().px_6().py_2().items_center().child(
        text(row_text(line))
            .text_size(px(12.0))
            .line_height(px(16.0))
            .text_center()
            .text_color(colors::secondary()),
    )
}

fn typing() -> impl IntoElement {
    view().px_4().py(px(3.0)).items_start().child(
        view()
            .px(px(14.0))
            .py(px(10.0))
            .rounded(px(18.0))
            .bg(colors::bubble_bot())
            .label("Typing")
            .child(
                text("•••")
                    .text_size(px(17.0))
                    .letter_spacing(px(2.0))
                    .text_color(colors::secondary()),
            ),
    )
}

fn composer(bot: Uuid, chat: Signal<Chat>, busy: Memo<bool>) -> impl IntoElement {
    let draft = signal(String::new());
    let outbox = signal(Outbox::Idle);
    let input_height = signal(22.0_f32);
    let sending = computed(move || matches!(outbox.get(), Outbox::Sending { .. }));
    let can_send = computed(move || !draft.with(|d| d.trim().is_empty()) && !sending.get());
    let show_stop = computed(move || busy.get() && draft.with(|d| d.trim().is_empty()));

    let send = move || {
        if !can_send.get_untracked() {
            return;
        }
        let text = draft.get_untracked().trim().to_owned();
        let request_id = outbox.with_untracked(Outbox::request_id);
        outbox.set(Outbox::Sending { request_id });
        let client = session::server().client.clone();
        spawn(async move {
            match client.send(bot, request_id, text).await {
                Ok(sent) => {
                    batch(|| {
                        draft.set(String::new());
                        outbox.set(Outbox::Idle);
                        chat.update(|c| {
                            if let Chat::Ready(conversation) = c {
                                conversation.insert_sent(sent.item, sent.turn);
                            }
                        });
                    });
                }
                Err(err) if err.outcome_unknown() => outbox.set(Outbox::Unknown {
                    request_id,
                    message: format!("{err} It may have been sent. Send again to retry safely."),
                }),
                Err(ClientError::Api { message, .. }) => outbox.set(Outbox::Refused(message)),
                Err(err) => outbox.set(Outbox::Refused(err.to_string())),
            }
        });
    };

    let stop = move || {
        let Some(request) = chat.with_untracked(|c| match c {
            Chat::Ready(conversation) => conversation.active_request(),
            _ => None,
        }) else {
            return;
        };
        let client = session::server().client.clone();
        spawn(async move {
            let _ = client.cancel(bot, request).await;
        });
    };

    let notice = dynamic(move || {
        let message = match outbox.get() {
            Outbox::Unknown { message, .. } | Outbox::Refused(message) => message,
            Outbox::Idle | Outbox::Sending { .. } => return None,
        };
        Some(
            text(message)
                .px_4()
                .pb_2()
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::destructive()),
        )
    });

    let round_button = |icon: nsh_core::element::ImageAsset, size: f32| {
        view()
            .size(px(32.0))
            .rounded(px(16.0))
            .items_center()
            .justify_center()
            .active(|s| s.opacity(0.6))
            .child(image(icon).size(px(size)).tint_color(colors::on_tint()))
    };

    let action = when(show_stop, move || {
        round_button(STOP, 12.0)
            .bg(colors::label())
            .role(AccessibilityRole::Button)
            .label("Stop")
            .on_press(stop)
    })
    .otherwise(move || {
        round_button(ARROW_UP, 16.0)
            .bg(colors::tint())
            .opacity(move || if can_send.get() { 1.0_f32 } else { 0.35 })
            .role(AccessibilityRole::Button)
            .label("Send")
            .on_press(send)
    });

    view()
        .bg(colors::chat())
        .border_t(px(0.5))
        .border_color(colors::separator())
        .pt_2()
        .pb(move || {
            px(8.0
                + if keyboard_height() > px(0.0) {
                    0.0
                } else {
                    safe_area().bottom.0
                })
        })
        .child(notice)
        .child(
            view()
                .flex_row()
                .items_end()
                .mx_3()
                .pl(px(14.0))
                .pr(px(4.0))
                .py(px(4.0))
                .gap_2()
                .rounded(px(20.0))
                .border_1()
                .border_color(colors::separator())
                .child(
                    input()
                        .flex_1()
                        .multiline(true)
                        .value(draft)
                        .on_change(move |t: &str| {
                            draft.set(t.to_owned());
                            if matches!(outbox.get_untracked(), Outbox::Refused(_)) {
                                outbox.set(Outbox::Idle);
                            }
                        })
                        .placeholder("Message")
                        .on_content_size_change(move |size: &Size<Points>| {
                            input_height.set(size.height.0.clamp(22.0, 120.0));
                        })
                        .h(move || px(input_height.get() + 8.0))
                        .py(px(4.0))
                        .text_size(px(17.0))
                        .line_height(px(22.0))
                        .text_color(colors::label())
                        .placeholder_color(|| color(colors::secondary()))
                        .caret_color(|| color(colors::tint()))
                        .label("Message"),
                )
                .child(action),
        )
}
