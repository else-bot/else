//! The bot list: every bot of the server, those waiting on the owner
//! first, as Messages lists its conversations.

use else_client::api::ReadCommand;
use nsh_base::{MenuButtonItem, alert_dialog, context_menu, nav_page};
use nsh_core::HeaderSystemItem;
use nsh_core::element::desktop::MenuItem;
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;

use super::parts::{self, colors};
use super::session::{self, LiveState};
use super::swipe;
use std::collections::HashSet;

use uuid::Uuid;

use crate::bots::{self, BotRow, Line, Status};

pub fn screen() -> Screen {
    // Collapsed categories, for this visit of the list.
    let collapsed = signal(HashSet::<Option<Uuid>>::new());
    let lines = computed(move || {
        collapsed.with(|folded| {
            session::bots().with(|b| bots::lines(&b.list, &b.categories, b.default_bot_id, folded))
        })
    });
    let loaded = computed(|| session::bots().with(|b| b.loaded));
    let refreshing = signal(false);
    let server = session::server();
    let host = server.profile.origin.display().to_owned();

    let saved = crate::server::Profile::list();
    let servers = saved.origins.iter().map(|origin| {
        let label = crate::server::Origin::parse(origin)
            .map(|o| o.display().to_owned())
            .unwrap_or_else(|_| origin.clone());
        let active = saved.active.as_deref() == Some(origin.as_str());
        let origin = origin.clone();
        MenuItem::new(label).checked(active).on_select(move || {
            if !active && let Err(err) = session::switch(&origin) {
                log::warn!("{err}");
            }
        })
    });
    let menu = HeaderItem::symbol("ellipsis.circle")
        .key("more")
        .label("More")
        .menu([
            MenuItem::new("Categories")
                .symbol("folder")
                .on_select(super::categories::open),
            MenuItem::submenu("Servers", servers).inline(true),
            MenuItem::new("Add Server…")
                .symbol("plus")
                .on_select(super::connect::open_add),
            MenuItem::new(format!("Disconnect from {host}"))
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
            if lines.with(Vec::is_empty) {
                return placeholder("No bots yet. Tap Compose to create one.").into_element();
            }
            view()
                .pb_safe_area()
                .child(each(
                    move || lines.get(),
                    |line: &Line| line.key().to_owned(),
                    move |line, _| match line.get_untracked() {
                        Line::Header { .. } => header(line, collapsed).into_element(),
                        Line::Bot { .. } => {
                            bot_row(computed(move || match line.get() {
                                Line::Bot { row, .. } => row,
                                // A key keeps its kind.
                                Line::Header { .. } => unreachable!(),
                            }))
                            .into_element()
                        }
                    },
                ))
                .into_element()
        }));

    let compose = HeaderItem::system(HeaderSystemItem::Compose)
        .key("new-bot")
        .label("New Bot")
        .on_press(super::new_bot::open);

    nav_page()
        .key("bots")
        .title("Bots")
        .header_large_title(true)
        .header_right_items(vec![compose, menu])
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

/// A section's header; a category's folds its bots away on a tap.
fn header(line: ReadSignal<Line>, collapsed: Signal<HashSet<Option<Uuid>>>) -> impl IntoElement {
    let Line::Header {
        category,
        collapsible,
        ..
    } = line.get_untracked()
    else {
        unreachable!("a header's key stays a header")
    };
    let folded = move || collapsed.with(|c| c.contains(&category));
    let title = move || match line.get() {
        Line::Header { title, count, .. } => (title, count),
        Line::Bot { .. } => (String::new(), 0),
    };
    view()
        .flex_row()
        .items_center()
        .gap_2()
        .px_4()
        .pt_4()
        .pb(px(6.0))
        .role(AccessibilityRole::Button)
        .label(move || {
            let (title, count) = title();
            format!("{title}, {count}")
        })
        .accessibility_state(move || AccessibilityState {
            expanded: collapsible.then(|| !folded()),
            ..AccessibilityState::default()
        })
        .on_press(move || {
            if collapsible {
                collapsed.update(|c| {
                    if !c.remove(&category) {
                        c.insert(category);
                    }
                });
            }
        })
        .child(
            text(move || title().0.to_uppercase())
                .flex_1()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::secondary()),
        )
        .child(
            text(move || {
                let count = title().1.to_string();
                if !collapsible {
                    count
                } else if folded() {
                    format!("{count}  ›")
                } else {
                    format!("{count}  ⌄")
                }
            })
            .text_size(px(13.0))
            .text_color(colors::secondary()),
        )
}

fn bot_row(row: Memo<BotRow>) -> impl IntoElement {
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
    let content = view()
        .flex_row()
        .items_center()
        .pl_4()
        .role(AccessibilityRole::Button)
        .label(move || row.with(|r| format!("{}, {}", r.name, r.subtitle())))
        .child(view().w(px(14.0)).items_center().child(when(
            move || row.with(|r| r.unread),
            || view().size(px(10.0)).rounded(px(5.0)).bg(colors::tint()),
        )))
        .child(parts::avatar(move || row.with(|r| r.avatar), 48.0).ml_1())
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
                        .child(when(
                            move || row.with(|r| r.muted),
                            || {
                                image(parts::BELL_SLASH)
                                    .size(px(14.0))
                                    .tint_color(colors::secondary())
                                    .label("Muted")
                            },
                        ))
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
        );
    // Rebuilt when what the actions say changes (read, muted).
    let state = computed(move || row.with(|r| (r.id, r.unread, r.muted)));
    dynamic(move || {
        let (id, unread, muted) = state.get();
        let name = row.with_untracked(|r| r.name.clone());
        let main = session::bots().with_untracked(|b| b.default_bot_id == Some(id));
        let content = content.clone();
        Some(untracked(|| {
            let leading = vec![swipe::Action::new(
                if unread { "Read" } else { "Unread" },
                0x2b88fb,
                move || mark_read(id, unread),
            )];
            let mut trailing = Vec::new();
            if !main {
                let name = name.clone();
                trailing.push(swipe::Action::new("Delete", 0xff3b30, move || {
                    confirm_delete(id, name.clone())
                }));
            }
            trailing.push(swipe::Action::new(
                if muted { "Unmute" } else { "Mute" },
                0x5856d6,
                move || set_muted(id, !muted),
            ));
            let mut items = vec![
                menu_item(
                    "read",
                    if unread {
                        "Mark as Read"
                    } else {
                        "Mark as Unread"
                    },
                    if unread {
                        "envelope.open"
                    } else {
                        "envelope.badge"
                    },
                ),
                menu_item(
                    "mute",
                    if muted { "Unmute" } else { "Mute" },
                    if muted { "bell" } else { "bell.slash" },
                ),
            ];
            if !main {
                let mut delete = menu_item("delete", "Delete", "trash");
                delete.destructive = true;
                items.push(delete);
            }
            context_menu()
                .items(items)
                .trigger(swipe::row(content, leading, trailing, open).into_element())
                .on_select(move |action: &SharedString| match action.as_ref() {
                    "read" => mark_read(id, unread),
                    "mute" => set_muted(id, !muted),
                    "delete" => confirm_delete(id, name.clone()),
                    _ => {}
                })
                .into_element()
        }))
    })
}

fn confirm_disconnect() {
    spawn(async move {
        let choice = alert_dialog()
            .title("Disconnect from this server?")
            .description(
                "Its owner key is removed from this iPhone. You can connect again with the key.",
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

fn menu_item(id: &'static str, label: &'static str, symbol: &'static str) -> MenuButtonItem {
    let mut item = MenuButtonItem::new(id, label);
    item.icon = Some(symbol.into());
    item
}

/// Marks the bot read through its newest message, or unread; the bots
/// channel brings the new state.
fn mark_read(bot: Uuid, unread: bool) {
    let client = session::server().client.clone();
    spawn(async move {
        let command = if unread {
            ReadCommand::MarkRead { position: None }
        } else {
            ReadCommand::MarkUnread
        };
        if client.clone().read(bot, command).await.is_ok() {
            haptic(Haptic::Selection);
        }
        session::refresh_bots();
    });
}

fn set_muted(bot: Uuid, muted: bool) {
    let client = session::server().client.clone();
    spawn(async move {
        if let Ok(saved) = client.set_muted(bot, muted).await {
            session::update_bot(saved);
            haptic(Haptic::Selection);
        }
    });
}

/// Deletes the bot after the owner confirms, as its details do.
fn confirm_delete(bot: Uuid, name: String) {
    spawn(async move {
        let choice = alert_dialog()
            .title(format!("Delete {name}?"))
            .description(
                "Any work in progress stops and the bot leaves your list. Its history, memory and files are kept.",
            )
            .cancel("cancel", "Cancel")
            .destructive("delete", "Delete")
            .show()
            .await;
        if !matches!(choice, Ok(r) if r.action.as_deref() == Some("delete")) {
            return;
        }
        let client = session::server().client.clone();
        match client.delete_bot(bot).await {
            Ok(_) => {
                haptic(Haptic::Notification(HapticNotification::Success));
                session::close_bot_screens(bot);
                session::refresh_bots();
            }
            Err(err) => {
                let _ = alert_dialog()
                    .title("Couldn't delete the bot")
                    .description(err.to_string())
                    .cancel("ok", "OK")
                    .show()
                    .await;
            }
        }
    });
}
