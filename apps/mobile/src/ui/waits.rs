//! What the bot waits on, docked above the composer: background jobs on
//! MCP servers or its computer, and tasks it handed to other bots. Each
//! row can be stopped on its own; the turn's update removes it.

use else_client::model::Wait;
use else_client::request::Request;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::colors;
use super::session::{self, Chat};

/// One wait as the tray shows it.
#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: Uuid,
    title: String,
    status: Option<String>,
    stoppable: bool,
}

fn rows(chat: &Chat) -> Vec<Row> {
    chat.conversation()
        .map(|conversation| {
            conversation
                .waiting_on()
                .iter()
                .map(|wait| Row {
                    id: wait.id(),
                    title: wait.title(),
                    status: wait.status(),
                    stoppable: wait.stoppable(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn header(chat: &Chat, name: &str) -> Option<String> {
    let conversation = chat.conversation()?;
    let waits = conversation.waiting_on();
    (!waits.is_empty()).then(|| format!("{name} is waiting on {}", Wait::summary(&waits)))
}

pub fn tray(bot: Uuid, name: String, chat: Signal<Chat>) -> impl IntoElement {
    let rows = computed(move || chat.with(rows));
    let header = computed(move || chat.with(|c| header(c, &name)));
    let open = signal(true);
    dynamic(move || {
        let title = header.get()?;
        Some(untracked(|| {
            view()
                .mx_3()
                .mb_2()
                .px_3()
                .py_2()
                .gap_2()
                .rounded(px(14.0))
                .border_curve(BorderCurve::Continuous)
                .bg(colors::card())
                .border_1()
                .border_color(colors::separator())
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .min_h(px(28.0))
                        .role(AccessibilityRole::Button)
                        .label(title.clone())
                        .active(|s| s.opacity(0.6))
                        .on_press(move || open.update(|o| *o = !*o))
                        .child(
                            text(title)
                                .flex_1()
                                .number_of_lines(1)
                                .text_size(px(15.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors::label()),
                        )
                        .child(
                            text(move || if open.get() { "Hide" } else { "Show" })
                                .text_size(px(15.0))
                                .text_color(colors::tint()),
                        ),
                )
                .child(when(open, move || {
                    view().gap_2().child(each(
                        move || rows.get(),
                        |row: &Row| row.id,
                        move |row, _| wait_row(bot, row),
                    ))
                }))
        }))
    })
}

fn wait_row(bot: Uuid, row: ReadSignal<Row>) -> impl IntoElement {
    let request = signal(Request::<Uuid>::Idle);
    let id = row.with_untracked(|r| r.id);
    let stop = move || {
        let client = session::server().client.clone();
        session::submit(
            request,
            id,
            move |wait| async move {
                match client.cancel_wait(bot, wait).await {
                    // It ended first; its update says how.
                    Err(err) if err.code() == Some("wait_ended") => Ok(()),
                    result => result,
                }
            },
            |()| {},
        );
    };
    view()
        .flex_row()
        .items_center()
        .gap_3()
        .child(
            view()
                .flex_1()
                .gap(px(2.0))
                .child(
                    text(move || row.with(|r| r.title.clone()))
                        .number_of_lines(2)
                        .text_size(px(15.0))
                        .line_height(px(20.0))
                        .text_color(colors::label()),
                )
                .child(dynamic(move || {
                    row.with(|r| r.status.clone()).map(|status| {
                        text(status)
                            .number_of_lines(1)
                            .text_size(px(13.0))
                            .text_color(colors::secondary())
                    })
                }))
                .child(dynamic(move || {
                    request.with(|r| r.error().map(str::to_owned)).map(|error| {
                        text(error)
                            .text_size(px(13.0))
                            .text_color(colors::destructive())
                    })
                })),
        )
        .child(when(
            move || row.with(|r| r.stoppable),
            move || {
                text(move || {
                    if request.with(Request::busy) {
                        "Stopping…"
                    } else {
                        "Stop"
                    }
                })
                .px_2()
                .py_1()
                .text_size(px(15.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::destructive())
                .disabled(move || request.with(Request::busy))
                .disabled_style(|s| s.opacity(0.45))
                .active(|s| s.opacity(0.5))
                .role(AccessibilityRole::Button)
                .label("Stop waiting")
                .on_press(stop)
            },
        ))
}
