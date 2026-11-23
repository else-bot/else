//! One bot's chat: the transcript as bubbles, the bot's replies as
//! Markdown that streams in, and the composer.

use std::collections::HashMap;

use else_wire::items::{Reaction, ReactionKind};
use nsh_base::{
    MarkdownOptions, MenuButtonItem, TextViewPart, TextViewParts, context_menu, nav_page, text_view,
};
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::colors;
use super::session::{self, Chat};
use else_client::api::ReadCommand;

use else_client::request::Request;

use crate::api::Message;
use crate::conversation::{self, Ask, Outcome, Retry, Row};

pub fn screen(bot: Uuid, name: String) -> Screen {
    let chat = session::open_chat(bot);
    let rows = computed(move || {
        chat.with(|c| c.conversation().map(conversation::rows).unwrap_or_default())
    });
    let thinking =
        computed(move || chat.with(|c| c.conversation().is_some_and(conversation::thinking)));
    // Stop shows only with a turn at work to stop.
    let busy = computed(move || {
        chat.with(|c| {
            c.conversation()
                .is_some_and(|c| conversation::active_request(c).is_some())
        })
    });
    keep_read(bot, chat);
    // The message the next one replies to, picked from a message's menu.
    let quote = signal(None::<Quote>);

    let transcript = dynamic(move || {
        match chat.with(|c| match c {
            Chat::Loading => Some(None),
            Chat::Failed(message) => Some(Some(message.clone())),
            Chat::Ready(_) | Chat::Recovering(_) => None,
        }) {
            Some(None) => centered("Loading…".into()).into_element(),
            Some(Some(message)) => centered(message).into_element(),
            None => untracked(|| transcript(bot, chat, rows, thinking, quote)).into_element(),
        }
    });

    let tray = super::waits::tray(bot, name.clone(), chat);
    nav_page()
        .key(format!("chat-{bot}"))
        .title(name)
        .header_large_title(false)
        .header_right_items(move || {
            vec![
                HeaderItem::symbol("info.circle")
                    .key("details")
                    .label("Details")
                    .on_press(move || super::details::open(bot, chat)),
                HeaderItem::symbol("magnifyingglass")
                    .key("search")
                    .label("Search")
                    .on_press(move || super::search::open(bot)),
            ]
        })
        .child(
            view()
                .flex_1()
                .bg(colors::chat())
                .pb_keyboard()
                .child(view().flex_1().child(transcript))
                .child(tray)
                .child(super::composer::composer(bot, chat, busy, quote)),
        )
}

/// Marks the bot read through its newest reply while its loaded chat is
/// on screen. Repeating a read is harmless, so a newer position simply
/// sends again; an effect re-run cancels the older request.
fn keep_read(bot: Uuid, chat: Signal<Chat>) {
    let loaded = computed(move || chat.with(|c| c.conversation().is_some()));
    let unread_through = computed(move || {
        session::bots().with(|b| {
            b.list
                .iter()
                .find(|it| it.id == bot)
                .and_then(|it| it.read_state.as_ref())
                .filter(|read| read.unread)
                .and_then(|read| read.latest.as_ref().map(|latest| latest.position))
        })
    });
    effect(move || {
        let Some(position) = unread_through.get().filter(|_| loaded.get()) else {
            return;
        };
        let client = session::server().client.clone();
        spawn(async move {
            // The bots channel brings the updated read state.
            let _ = client.read(bot, ReadCommand::Through { position }).await;
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

/// Which view a transcript line takes.
#[derive(Clone, Copy, PartialEq)]
enum LineKind {
    Owner,
    Bot,
    Note,
    Exchange,
    Mail,
    Typing,
    Ask,
}

impl LineKind {
    fn of(line: &Line) -> Self {
        match line {
            Line::Row(Row::Owner { .. }) => Self::Owner,
            Line::Row(Row::Bot { .. }) => Self::Bot,
            Line::Row(Row::Note { .. }) => Self::Note,
            Line::Row(Row::Exchange(_)) => Self::Exchange,
            Line::Row(Row::Mail { .. }) => Self::Mail,
            Line::Typing => Self::Typing,
            Line::Ask(_) => Self::Ask,
        }
    }
}

/// A transcript row, or the typing indicator after the last one.
#[derive(Clone, PartialEq)]
enum Line {
    Row(Row),
    Ask(Ask),
    Typing,
}

fn transcript(
    bot: Uuid,
    chat: Signal<Chat>,
    rows: Memo<Vec<Row>>,
    thinking: Memo<bool>,
    quote: Signal<Option<Quote>>,
) -> impl IntoElement {
    let asks = computed(move || {
        chat.with(|c| c.conversation().map(conversation::asks).unwrap_or_default())
    });
    let lines = computed(move || {
        let mut lines: Vec<Line> = rows.get().into_iter().map(Line::Row).collect();
        lines.extend(asks.get().into_iter().map(Line::Ask));
        if thinking.get() && asks.with(Vec::is_empty) {
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
            // Rebuilt only when the line changes kind; within a kind, the
            // view follows `line`.
            let kind = computed(move || line.with(|l| l.as_ref().map(LineKind::of)));
            view().child(dynamic(move || {
                let kind = kind.get()?;
                untracked(|| {
                    Some(match kind {
                        LineKind::Owner => {
                            with_menu(bot, chat, line, quote, owner_bubble(bot, line))
                        }
                        LineKind::Bot => with_menu(bot, chat, line, quote, bot_bubble(line)),
                        LineKind::Note => note(line).into_element(),
                        LineKind::Exchange => dynamic(move || {
                            line.with(|l| match l {
                                Some(Line::Row(Row::Exchange(exchange))) => {
                                    Some((**exchange).clone())
                                }
                                _ => None,
                            })
                            .map(|exchange| untracked(|| super::exchange::card(exchange)))
                        })
                        .into_element(),
                        LineKind::Mail => dynamic(move || {
                            line.with(|l| match l {
                                Some(Line::Row(Row::Mail { mails, .. })) => Some(mails.clone()),
                                _ => None,
                            })
                            .map(|mails| untracked(|| super::mail::card(mails)))
                        })
                        .into_element(),
                        LineKind::Typing => typing().into_element(),
                        LineKind::Ask => dynamic(move || {
                            line.with(|l| match l {
                                Some(Line::Ask(ask)) => Some(ask.clone()),
                                _ => None,
                            })
                            .map(|ask| untracked(|| super::asks::card(bot, ask)))
                        })
                        .into_element(),
                    })
                })
            }))
        },
    )
    .keys(move || {
        lines.with(|l| {
            l.iter()
                .map(|line| match line {
                    Line::Row(row) => row.key(),
                    Line::Ask(ask) => ask.key(),
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

fn owner_bubble(bot: Uuid, line: Memo<Option<Line>>) -> impl IntoElement {
    let outcome = computed(move || {
        line.with(|l| match l {
            Some(Line::Row(Row::Owner { outcome, .. })) => outcome.clone(),
            _ => None,
        })
    });
    let files = computed(move || {
        line.with(|l| match l {
            Some(Line::Row(Row::Owner { files, .. })) => files.clone(),
            _ => Vec::new(),
        })
    });
    let has_text = computed(move || !row_text(line)().is_empty());
    view()
        .px_4()
        .py(px(3.0))
        .gap_1()
        .items_end()
        .child(dynamic(move || {
            let files = files.get();
            (!files.is_empty()).then(|| untracked(|| super::files::strip(bot, files, true)))
        }))
        .child(when(has_text, move || {
            view()
                .max_w(relative(0.78))
                .px(px(14.0))
                .py(px(8.0))
                .rounded(px(18.0))
                .border_curve(BorderCurve::Continuous)
                .bg(colors::bubble_owner())
                .gap(px(6.0))
                // A reply shows the message it quotes above the owner's words.
                .child(dynamic(move || {
                    let quoted = row_text(line)();
                    let read = else_client::reply::Replying::parse(&quoted)?;
                    Some(
                        view()
                            .pl_2()
                            .border_l(px(2.0))
                            .border_color(rgba(0xffffff99))
                            .child(
                                text(read.original())
                                    .number_of_lines(3)
                                    .text_size(px(14.0))
                                    .line_height(px(19.0))
                                    .text_color(rgba(0xffffffcc)),
                            ),
                    )
                }))
                // Mentions of bots show in bold, the rest exactly as typed.
                .child(
                    text_view()
                        .markdown(move || {
                            let shown = row_text(line)();
                            let words = else_client::reply::Replying::parse(&shown)
                                .map_or_else(|| shown.clone(), |read| read.body.to_owned());
                            SharedString::from(crate::mentions::plain(&words, &mention_bots()))
                        })
                        .selectable(true)
                        .text_size(px(17.0))
                        .line_height(px(22.0))
                        .text_color(rgb(0xffffff)),
                )
        }))
        .child(dynamic(move || {
            outcome.get().map(|outcome| {
                let color = if outcome.failed {
                    colors::destructive()
                } else {
                    colors::secondary()
                };
                text(outcome.label)
                    .pt_1()
                    .text_size(px(12.0))
                    .text_color(color)
            })
        }))
        .child(dynamic(move || {
            let retry = outcome.with(|o| match o {
                Some(Outcome { retry, .. }) => *retry,
                _ => None,
            })?;
            Some(untracked(|| retry_button(bot, retry)))
        }))
}

/// Runs a failed message again. The turn's update that queues it replaces
/// the failure; until then a lost response can be retried as is.
fn retry_button(bot: Uuid, retry: Retry) -> impl IntoElement {
    let request = signal(Request::<i64>::Idle);
    let press = move || {
        let client = session::server().client.clone();
        session::submit(
            request,
            retry.attempt,
            move |attempt| client.retry_turn(bot, retry.turn, attempt),
            |_| {},
        );
    };
    view()
        .items_end()
        .gap_1()
        .child(
            text(move || {
                SharedString::from(if request.with(Request::busy) {
                    "Retrying…"
                } else {
                    "Retry"
                })
            })
            .px_2()
            .py_1()
            .text_size(px(15.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(colors::tint())
            .disabled(move || request.with(Request::busy))
            .disabled_style(|s| s.opacity(0.45))
            .active(|s| s.opacity(0.5))
            .role(AccessibilityRole::Button)
            .label("Retry this message")
            .on_press(press),
        )
        .child(dynamic(move || {
            request.with(|r| r.error().map(str::to_owned)).map(|error| {
                text(error)
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
    let reaction = computed(move || {
        line.with(|l| match l {
            Some(Line::Row(Row::Bot { reaction, .. })) => *reaction,
            _ => None,
        })
    });
    view()
        .w_full()
        .px_4()
        .pt(px(6.0))
        .pb(px(10.0))
        .gap_1()
        .child(
            text_view()
                .markdown(move || {
                    SharedString::from(crate::mentions::markdown(&markdown(), &mention_bots()))
                })
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
        .child(dynamic(move || {
            reaction.get().map(|kind| {
                text(else_client::reply::mark(kind))
                    .text_size(px(15.0))
                    .label(match kind {
                        ReactionKind::ThumbsUp => "You reacted thumbs up",
                        ReactionKind::ThumbsDown => "You reacted thumbs down",
                    })
            })
        }))
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

/// The bots a message can mention, as the list has them now.
fn mention_bots() -> else_client::mention::Registry {
    session::bots().with(|b| else_client::mention::Registry::new(&b.list))
}

fn menu_item(id: &'static str, label: &'static str, symbol: &'static str) -> MenuButtonItem {
    let mut item = MenuButtonItem::new(id, label);
    item.icon = Some(symbol.into());
    item
}

/// The message the next one replies to: who wrote it and its text.
#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub owner: bool,
    pub text: String,
}

impl Quote {
    /// How the reply names it, as the desktop does.
    pub fn subject(&self) -> &'static str {
        if self.owner {
            "my earlier message"
        } else {
            "your earlier message"
        }
    }
}

/// A message with the system's long-press menu: react to the bot's
/// message, reply to either, or copy its text.
fn with_menu(
    bot: Uuid,
    chat: Signal<Chat>,
    line: Memo<Option<Line>>,
    quote: Signal<Option<Quote>>,
    bubble: impl IntoElement,
) -> AnyElement {
    let picked = move || {
        line.with_untracked(|l| match l {
            Some(Line::Row(Row::Owner { id, text, .. })) => Some((*id, true, text.clone())),
            Some(Line::Row(Row::Bot { id, text, .. })) => Some((*id, false, text.clone())),
            _ => None,
        })
    };
    let owner = picked().is_some_and(|(_, owner, _)| owner);
    let mut items = Vec::new();
    if !owner {
        items.push(menu_item("up", "Good Response", "hand.thumbsup"));
        items.push(menu_item("down", "Bad Response", "hand.thumbsdown"));
    }
    items.push(menu_item("reply", "Reply", "arrowshape.turn.up.left"));
    items.push(menu_item("copy", "Copy", "doc.on.doc"));
    context_menu()
        .items(items)
        .trigger(bubble.into_element())
        .on_select(move |action: &SharedString| {
            let Some((id, owner, text)) = picked() else {
                return;
            };
            match action.as_ref() {
                "up" => react(bot, chat, id, text, ReactionKind::ThumbsUp),
                "down" => react(bot, chat, id, text, ReactionKind::ThumbsDown),
                "reply" => quote.set(Some(Quote { owner, text })),
                "copy" => set_clipboard(text),
                _ => {}
            }
        })
        .into_element()
}

/// Reactions sent whose answer is not in yet, by the message and the
/// mark: choosing the same one again resends it under its request ID, so
/// the server keeps one.
#[derive(Clone, Default, PartialEq)]
struct PendingReactions(HashMap<(Uuid, bool), Uuid>);

/// Reacts to a bot's message: a message carrying the reaction, which the
/// chat shows as the mark on that message.
fn react(bot: Uuid, chat: Signal<Chat>, item: Uuid, text: String, kind: ReactionKind) {
    let pending = global::<PendingReactions>();
    // Keyed by the message and whether it is a thumbs up.
    let key = (item, kind == ReactionKind::ThumbsUp);
    let request_id = pending
        .with_untracked(|p| p.0.get(&key).copied())
        .unwrap_or_else(|| {
            let fresh = Uuid::new_v4();
            pending.update(|p| {
                p.0.insert(key, fresh);
            });
            fresh
        });
    let message = Message {
        text: else_client::reply::reaction(kind, "your earlier message", &text),
        files: Vec::new(),
        draft: None,
        answers_notice: None,
        reaction: Some(Reaction {
            item_id: item,
            kind,
        }),
    };
    let client = session::server().client.clone();
    session::spawn_held(async move {
        match client.send(bot, message.request(request_id)).await {
            Ok(sent) => {
                pending.update(|p| {
                    p.0.remove(&key);
                });
                session::try_update(chat, |c| {
                    if let Some(conversation) = c.conversation_mut() {
                        conversation.insert_item(sent.item);
                        conversation.insert_turn(sent.turn);
                    }
                });
            }
            Err(err) => {
                let _ = nsh_base::alert_dialog()
                    .title("Couldn't confirm the reaction")
                    .description(format!(
                        "{err} It may have been sent. Choosing it again can't send it twice."
                    ))
                    .cancel("ok", "OK")
                    .show()
                    .await;
            }
        }
    });
}
