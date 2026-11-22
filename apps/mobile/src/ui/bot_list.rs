//! The bot list: every bot of the server, those waiting on the owner
//! first, as Messages lists its conversations.

use nsh_base::{alert_dialog, nav_page};
use nsh_core::element::desktop::MenuItem;
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;

use super::parts::{self, colors};
use super::session::{self, LiveState};
use crate::bots::{self, BotRow, Status};

pub fn screen() -> Screen {
    let rows = computed(|| session::bots().with(|b| bots::rows(&b.list)));
    let loaded = computed(|| session::bots().with(|b| b.loaded));
    let refreshing = signal(false);
    let server = session::server();
    let host = server.profile.origin.display().to_owned();

    let menu = HeaderItem::symbol("ellipsis.circle")
        .key("more")
        .label("More")
        .menu([
            MenuItem::new(format!("Connected to {host}")).disabled(true),
            MenuItem::new("Disconnect")
                .symbol("rectangle.portrait.and.arrow.right")
                .destructive(true)
                .on_select(confirm_disconnect),
        ]);

    let list = scroll()
        .flex_1()
        .bg(colors::chat())
        .refreshing(refreshing)
        .on_refresh(move || {
            refreshing.set(true);
            let task = session::refresh_bots();
            spawn(async move {
                task.await;
                refreshing.set(false);
            });
        })
        .child(banner())
        .child(dynamic(move || {
            if !loaded.get() {
                return placeholder("Loading bots…").into_element();
            }
            if rows.with(Vec::is_empty) {
                return placeholder("No bots yet. Create one in the desktop app.").into_element();
            }
            view()
                .pb_safe_area()
                .child(each(
                    move || rows.get(),
                    |row: &BotRow| row.id,
                    |row, _| bot_row(row),
                ))
                .into_element()
        }));

    nav_page()
        .key("bots")
        .title("Bots")
        .header_large_title(true)
        .header_right_items(vec![menu])
        .child(list)
}

fn placeholder(message: &'static str) -> impl IntoElement {
    view().px_6().py(px(80.0)).items_center().child(
        text(message)
            .text_size(px(15.0))
            .text_center()
            .text_color(colors::secondary()),
    )
}

/// The live connection's problem, when it has one.
fn banner() -> impl IntoElement {
    dynamic(|| {
        let message = match session::live_state().get() {
            LiveState::Offline(reason) => format!("Reconnecting… {reason}"),
            LiveState::Unauthorized => "The server rejected the owner key. Disconnect and connect again with the current key.".into(),
            LiveState::Connecting | LiveState::Live => return None,
        };
        Some(
            view()
                .mx_4()
                .my_2()
                .px_4()
                .py_3()
                .rounded(px(10.0))
                .bg(colors::fill())
                .child(
                    text(message)
                        .text_size(px(13.0))
                        .line_height(px(18.0))
                        .text_color(colors::secondary()),
                ),
        )
    })
}

fn bot_row(row: ReadSignal<BotRow>) -> impl IntoElement {
    let open = move || {
        let BotRow { id, name, .. } = row.get_untracked();
        super::push(format!("chat-{id}"), move || {
            super::chat::screen(id, name.clone())
        });
    };
    let time = move || {
        row.with(|r| {
            r.last_message_at
                .map(|at| bots::time_label(at, chrono::Utc::now()))
                .unwrap_or_default()
        })
    };
    let subtitle_color = move || match row.with(|r| r.status.clone()) {
        Status::NeedsYou(_) => colors::warning(),
        Status::Problem(_) => colors::destructive(),
        _ => colors::secondary(),
    };
    view()
        .flex_row()
        .items_center()
        .pl_4()
        .active(|s| s.bg(colors::fill()))
        .role(AccessibilityRole::Button)
        .label(move || row.with(|r| format!("{}, {}", r.name, r.subtitle())))
        .on_press(open)
        .child(view().w(px(14.0)).items_center().child(when(
            move || row.with(|r| r.unread),
            || view().size(px(10.0)).rounded(px(5.0)).bg(colors::tint()),
        )))
        .child(
            parts::avatar(
                move || SharedString::from(row.with(|r| r.initial.clone())),
                move || row.with(|r| r.color),
                48.0,
            )
            .ml_1(),
        )
        .child(
            view()
                .flex_1()
                .ml_3()
                .py(px(11.0))
                .pr_4()
                .gap(px(2.0))
                .border_b(px(0.5))
                .border_color(colors::separator())
                .child(
                    view()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            text(move || row.with(|r| r.name.clone()))
                                .flex_1()
                                .number_of_lines(1)
                                .text_size(px(17.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors::label()),
                        )
                        .child(
                            text(time)
                                .text_size(px(15.0))
                                .text_color(colors::secondary()),
                        ),
                )
                .child(
                    text(move || row.with(BotRow::subtitle))
                        .number_of_lines(2)
                        .text_size(px(15.0))
                        .line_height(px(20.0))
                        .text_color(subtitle_color),
                ),
        )
}

fn confirm_disconnect() {
    spawn(async move {
        let choice = alert_dialog()
            .title("Disconnect from this server?")
            .description(
                "The owner key is removed from this iPhone. You can connect again with the key.",
            )
            .cancel("cancel", "Cancel")
            .destructive("disconnect", "Disconnect")
            .show()
            .await;
        if matches!(choice, Ok(r) if r.action.as_deref() == Some("disconnect")) {
            session::disconnect();
        }
    });
}
