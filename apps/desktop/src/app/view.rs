//! Rendering of the sidebar, header, conversation, and composer.

use super::{
    APP_CONTEXT, COMPOSER_CONTEXT, Connection, DeleteBot, ElseApp, FocusComposer, FocusSidebar,
    LoadOlder, MoveBot, NewBot, NewCategory, RowKey, SIDEBAR_CONTEXT, SelectNext, SelectPrev,
    StopWork,
};
use crate::api::Bot;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::*;
use uuid::Uuid;

#[derive(Clone)]
struct BotDrag {
    bot: Uuid,
    name: SharedString,
}

impl Render for BotDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        div()
            .w(px(244.))
            .px_2()
            .py_1p5()
            .rounded_md()
            .border_1()
            .border_color(theme.drag_border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .shadow_md()
            .text_sm()
            .truncate()
            .child(self.name.clone())
    }
}

impl ElseApp {
    fn status_text(&self) -> (SharedString, bool) {
        match &self.connection {
            Connection::Connecting => ("Connecting…".into(), false),
            Connection::Connected => ("Connected".into(), false),
            Connection::Disconnected(message) => {
                (format!("Disconnected — retrying. {message}").into(), true)
            }
            Connection::InvalidCredentials(message) => (message.clone().into(), true),
        }
    }

    fn provider_label(&self, id: &str) -> String {
        self.caps
            .as_ref()
            .and_then(|caps| caps.providers.iter().find(|p| p.id == id))
            .map(|p| p.label.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (status, is_error) = self.status_text();
        let mut list = v_flex()
            .id("bot-list")
            .gap_1()
            .p_2()
            .flex_1()
            .overflow_y_scroll();
        let grouped = !self.categories.is_empty();
        let groups = crate::sidebar::groups(&self.categories, &self.bots);
        let can_edit = self.connection == Connection::Connected;
        let owner = cx.entity().downgrade();
        for group in groups {
            let destination = group.category.map(|c| c.id);
            let owner = owner.clone();
            let mut zone = v_flex()
                .id(SharedString::from(match destination {
                    Some(id) => format!("category-group-{id}"),
                    None => "category-group-unassigned".to_owned(),
                }))
                .gap_1()
                .rounded_md()
                .can_drop(move |value, _, cx| {
                    value.downcast_ref::<BotDrag>().is_some_and(|drag| {
                        owner.upgrade().is_some_and(|app| {
                            app.read(cx).can_assign_category(drag.bot, destination)
                        })
                    })
                })
                .drag_over::<BotDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                .on_drop(cx.listener(move |app, drag: &BotDrag, window, cx| {
                    app.assign_category(drag.bot, destination, window, cx)
                }));
            if grouped {
                // A header per category (empty ones stay), then Unassigned.
                let mut header = h_flex().mt_2().px_2().gap_1().items_center().child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.muted_foreground)
                        .truncate()
                        .child(match group.category {
                            Some(c) => format!("{} · {}", c.name, group.bots.len()),
                            None => format!("Unassigned · {}", group.bots.len()),
                        }),
                );
                if let Some(category) = group.category {
                    let rename = category.clone();
                    let delete = category.clone();
                    header = header
                        .child(
                            Button::new(SharedString::from(format!("rename-{}", category.id)))
                                .label("Rename")
                                .xsmall()
                                .ghost()
                                .disabled(!can_edit)
                                .on_click(cx.listener(move |app, _, window, cx| {
                                    app.open_category_dialog(Some(rename.clone()), window, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("delete-cat-{}", category.id)))
                                .label("Delete")
                                .xsmall()
                                .ghost()
                                .disabled(!can_edit)
                                .on_click(cx.listener(move |app, _, window, cx| {
                                    app.open_delete_category_dialog(delete.clone(), window, cx)
                                })),
                        );
                }
                zone = zone.child(header);
                if group.bots.is_empty() {
                    zone = zone.child(
                        div()
                            .px_2()
                            .text_xs()
                            .italic()
                            .text_color(theme.muted_foreground)
                            .child("No bots"),
                    );
                }
            }
            for bot in group.bots {
                zone = zone.child(self.render_bot_row(bot, cx));
            }
            list = list.child(zone);
        }
        let can_create = self.connection == Connection::Connected;
        v_flex()
            .id("sidebar")
            .key_context(SIDEBAR_CONTEXT)
            .track_focus(&self.sidebar_focus)
            .on_action(
                cx.listener(|app, _: &SelectNext, window, cx| app.move_selection(1, window, cx)),
            )
            .on_action(
                cx.listener(|app, _: &SelectPrev, window, cx| app.move_selection(-1, window, cx)),
            )
            .w(px(260.))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                v_flex()
                    .p_3()
                    .gap_1()
                    .border_b_1()
                    .border_color(theme.sidebar_border)
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("else.bot"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(if is_error {
                                theme.danger
                            } else {
                                theme.muted_foreground
                            })
                            .child(status),
                    )
                    .children(
                        self.category_notice
                            .clone()
                            .map(|notice| div().text_xs().text_color(theme.danger).child(notice)),
                    ),
            )
            .child(list)
            .child(
                div()
                    .p_2()
                    .border_t_1()
                    .border_color(theme.sidebar_border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new("new-bot")
                                    .label("New bot")
                                    .flex_1()
                                    .disabled(!can_create)
                                    .on_click(cx.listener(|app, _, window, cx| {
                                        app.open_create_dialog(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("new-category")
                                    .label("New category")
                                    .ghost()
                                    .disabled(!can_create)
                                    .on_click(cx.listener(|app, _, window, cx| {
                                        app.open_category_dialog(None, window, cx)
                                    })),
                            ),
                    ),
            )
    }

    fn render_bot_row(&self, bot: &Bot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let id = bot.id;
        let selected = self.selected == Some(id);
        let pending = !bot.is_active();
        let activity = if pending {
            "Deleting…".to_owned()
        } else {
            activity_label(bot)
        };
        let mut name_row = h_flex()
            .gap_2()
            .child(div().text_sm().truncate().child(bot.name.clone()));
        if self.default_bot_id == Some(id) {
            name_row = name_row.child(
                div()
                    .text_xs()
                    .px_1()
                    .rounded_sm()
                    .bg(theme.secondary)
                    .text_color(theme.secondary_foreground)
                    .child("Main"),
            );
        }
        let mut row = v_flex()
            .id(SharedString::from(format!("bot-{id}")))
            .px_2()
            .py_1p5()
            .rounded_md()
            .child(name_row)
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(activity),
            );
        if pending {
            row = row.opacity(0.5);
        } else {
            row = row
                .cursor_pointer()
                .hover(|style| style.bg(theme.list_hover))
                .on_click(cx.listener(move |app, _, window, cx| app.select(Some(id), window, cx)));
        }
        if !self.categories.is_empty() && self.movable_bot(id).is_some() {
            row = row.on_drag(
                BotDrag {
                    bot: id,
                    name: bot.name.clone().into(),
                },
                |drag, _, _, cx| cx.new(|_| drag.clone()),
            );
        }
        if selected {
            row = row.bg(theme.list_active);
        }
        row
    }

    fn render_header(&self, bot: &Bot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let activity = self
            .view
            .as_ref()
            .and_then(|v| v.conversation.as_ref())
            .and_then(|c| c.activity.clone())
            .or(bot.activity.clone());
        let busy = activity.as_ref().is_some_and(|a| a.phase != "idle");
        let stopping = self.stopping.contains(&bot.id)
            || activity.as_ref().is_some_and(|a| a.phase == "cancelling");
        let mut actions = h_flex().gap_2();
        if busy || stopping {
            actions = actions.child(
                Button::new("stop-work")
                    .label(if stopping { "Stopping…" } else { "Stop work" })
                    .small()
                    .disabled(stopping)
                    .on_click(cx.listener(|app, _, window, cx| app.stop_work(window, cx))),
            );
        }
        let category = bot
            .category_id
            .and_then(|id| self.categories.iter().find(|c| c.id == id))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "Unassigned".to_owned());
        actions = actions.child(
            Button::new("move-bot")
                .label(if self.moving.contains(&bot.id) {
                    "Moving…".to_owned()
                } else {
                    format!("Category: {category}")
                })
                .small()
                .ghost()
                .disabled(self.connection != Connection::Connected || self.moving.contains(&bot.id))
                .on_click(cx.listener(|app, _, window, cx| app.open_move_dialog(window, cx))),
        );
        if self.default_bot_id != Some(bot.id) {
            actions = actions.child(
                Button::new("delete-bot")
                    .label("Delete")
                    .small()
                    .ghost()
                    .on_click(cx.listener(|app, _, window, cx| app.open_delete_dialog(window, cx))),
            );
        }
        h_flex()
            .px_4()
            .py_2()
            .gap_4()
            .justify_between()
            .border_b_1()
            .border_color(theme.border)
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(bot.name.clone()),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(bot.role.clone().unwrap_or_default()),
                    ),
            )
            .child(actions.flex_none())
    }

    fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let Some(key) = view.rows.get(ix).copied() else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let bubble = |state: &Entity<TextareaState>, user: bool| {
            // Wrapped text needs a definite width, so bubbles span the row
            // and are offset to their side.
            let side = div().w_full();
            let side = if user {
                side.pl(px(96.))
            } else {
                side.pr(px(96.))
            };
            side.child(
                div()
                    .w_full()
                    .px_3()
                    .py_2()
                    .rounded_lg()
                    .bg(if user {
                        theme.secondary
                    } else {
                        theme.background
                    })
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        Textarea::new(state)
                            .readonly(true)
                            .appearance(false)
                            .bordered(false),
                    ),
            )
        };
        let note = |text: String, error: bool| {
            div()
                .text_xs()
                .text_color(if error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(text)
        };
        match key {
            RowKey::Item(id) => {
                let Some(conversation) = view.conversation.as_ref() else {
                    return div().into_any_element();
                };
                let Some(item) = conversation.item(id) else {
                    return div().into_any_element();
                };
                let Some((state, _)) = view.texts.get(&key) else {
                    return div().into_any_element();
                };
                let user = item.kind == "user";
                let turn = conversation.turn(item.turn_id);
                // Children stretch so bubbles get a definite width; notes
                // are aligned to the bubble's side.
                let mut column = v_flex().w_full().gap_1().px_4().py_1();
                if !user {
                    if let Some(provider) = &item.provider_id {
                        column = column.child(note(self.provider_label(provider), false));
                    }
                }
                column = column.child(bubble(state, user));
                if let Some(turn) = turn {
                    let show_on_user = user && !turn.is_finished()
                        || user
                            && turn.status != "completed"
                            && !conversation
                                .items()
                                .any(|i| i.turn_id == turn.id && i.kind == "assistant");
                    let show_on_assistant = !user && turn.status != "completed";
                    if show_on_user || show_on_assistant {
                        let (label, error) = turn_label(turn);
                        if !label.is_empty() {
                            let row = h_flex().w_full().child(note(label, error));
                            column = column.child(if user { row.justify_end() } else { row });
                        }
                    }
                }
                column.into_any_element()
            }
            RowKey::Pending(request_id) => {
                let Some(pending) = self
                    .selected
                    .and_then(|bot| self.pending.get(&bot))
                    .and_then(|list| list.iter().find(|p| p.request_id == request_id))
                else {
                    return div().into_any_element();
                };
                let Some((state, _)) = view.texts.get(&key) else {
                    return div().into_any_element();
                };
                let mut column = v_flex()
                    .w_full()
                    .gap_1()
                    .px_4()
                    .py_1()
                    .child(bubble(state, true));
                if let Some(reason) = &pending.rejected {
                    return column
                        .child(
                            h_flex()
                                .w_full()
                                .justify_end()
                                .gap_2()
                                .child(note(format!("Not sent. {reason}"), true))
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "restore-{request_id}"
                                    )))
                                    .label("Restore to composer")
                                    .xsmall()
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.take_rejected(request_id, true, window, cx)
                                    })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "dismiss-{request_id}"
                                    )))
                                    .label("Dismiss")
                                    .xsmall()
                                    .ghost()
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.take_rejected(request_id, false, window, cx)
                                    })),
                                ),
                        )
                        .into_any_element();
                }
                column = match &pending.unknown {
                    None => column.child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .child(note("Sending…".into(), false)),
                    ),
                    Some(reason) => column.child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .gap_2()
                            .child(note(format!("Not confirmed. {reason}"), true))
                            .child(
                                Button::new(SharedString::from(format!("retry-{request_id}")))
                                    .label("Retry")
                                    .xsmall()
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.retry_pending(request_id, window, cx)
                                    })),
                            ),
                    ),
                };
                column.into_any_element()
            }
        }
    }

    fn render_conversation(&self, bot: &Bot, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let view = self.view.as_ref().filter(|v| v.bot_id == bot.id);
        let transcript: AnyElement = match view {
            Some(view) if view.conversation.is_some() || !view.rows.is_empty() => {
                if view.rows.is_empty() {
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(theme.muted_foreground)
                        .child("No messages yet. Say hello.")
                        .into_any_element()
                } else {
                    let older = view
                        .conversation
                        .as_ref()
                        .is_some_and(|c| c.older_before_seq.is_some());
                    // Outside the list, so prepending history shifts the
                    // anchored message instead of the button.
                    let mut column = v_flex().flex_1().min_h_0();
                    if older {
                        let loading = view.loading_older;
                        column =
                            column.child(
                                h_flex().justify_center().py_1().child(
                                    Button::new("load-older")
                                        .label(if loading {
                                            "Loading…"
                                        } else {
                                            "Load older messages"
                                        })
                                        .xsmall()
                                        .ghost()
                                        .disabled(loading)
                                        .on_click(cx.listener(|app, _, window, cx| {
                                            app.load_older(window, cx)
                                        })),
                                ),
                            );
                    }
                    column
                        .child(
                            list(
                                view.list.clone(),
                                cx.processor(|app, ix, window, cx| app.render_row(ix, window, cx)),
                            )
                            .flex_1()
                            .py_2(),
                        )
                        .into_any_element()
                }
            }
            _ => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child("Loading conversation…")
                .into_any_element(),
        };
        let mut footer = v_flex()
            .gap_1()
            .px_4()
            .py_3()
            .border_t_1()
            .border_color(theme.border);
        if let Some(problem) = view.and_then(|v| v.stream_problem.clone()) {
            footer = footer.child(div().text_xs().text_color(theme.warning).child(problem));
        }
        if let Some(notice) = &self.notice {
            footer = footer.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(notice.clone()),
            );
        }
        let can_send = self.connection == Connection::Connected;
        footer = footer.child(
            h_flex()
                .gap_2()
                .items_end()
                .child(
                    div()
                        .key_context(COMPOSER_CONTEXT)
                        .flex_1()
                        .min_w_0()
                        .child(Textarea::new(&self.composer)),
                )
                .child(
                    Button::new("send")
                        .label("Send")
                        .primary()
                        .disabled(!can_send)
                        .on_click(cx.listener(|app, _, window, cx| app.send(window, cx))),
                ),
        );
        v_flex()
            .size_full()
            .min_w_0()
            .child(self.render_header(bot, cx))
            .child(transcript)
            .child(footer)
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if let Some(bot) = self.selected_bot().filter(|b| b.is_active()).cloned() {
            return v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_hidden()
                .bg(theme.background)
                .child(self.render_conversation(&bot, cx))
                .into_any_element();
        }
        let message = match self.connection {
            Connection::Connected => "Create a bot to start a conversation.",
            Connection::InvalidCredentials(_) => {
                "Use Change server to fix the address or owner token."
            }
            _ => "Waiting for the API…",
        };
        let mut body = v_flex()
            .flex_1()
            .h_full()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(theme.background)
            .child(div().text_color(theme.muted_foreground).child(message));
        if self.connection == Connection::Connected {
            body = body.child(
                Button::new("empty-create")
                    .label("Create bot")
                    .primary()
                    .on_click(cx.listener(|app, _, window, cx| app.open_create_dialog(window, cx))),
            );
        }
        body.into_any_element()
    }
}

fn activity_label(bot: &Bot) -> String {
    let Some(activity) = &bot.activity else {
        return String::new();
    };
    match activity.phase.as_str() {
        "working" if activity.queued_count > 0 => {
            format!("Working · {} queued", activity.queued_count)
        }
        "working" => "Working".into(),
        "queued" => format!("{} queued", activity.queued_count),
        "cancelling" => "Stopping…".into(),
        "unavailable" => "Unavailable".into(),
        _ => "Idle".into(),
    }
}

/// A short public description of a turn's state. Returns (text, is_error).
fn turn_label(turn: &crate::model::Turn) -> (String, bool) {
    let detail = turn
        .error
        .as_ref()
        .and_then(|e| e.message.clone())
        .unwrap_or_default();
    match turn.status.as_str() {
        "queued" => ("Queued".into(), false),
        "preparing" | "sending" | "running" => ("Working…".into(), false),
        "cancelling" => ("Stopping…".into(), false),
        // The main bot handed part of the request to another bot.
        "waiting" => ("Waiting for another bot…".into(), false),
        "interrupted" => (
            if turn.error.as_ref().is_some_and(|e| e.code == "cancelled") {
                "Stopped before it ran".into()
            } else {
                "Stopped".into()
            },
            false,
        ),
        "failed" => (format!("Failed. {detail}"), true),
        "uncertain" => (format!("Outcome unknown. {detail}"), true),
        _ => (String::new(), false),
    }
}

impl Render for ElseApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(welcome) = &self.welcome {
            let theme = cx.theme().foreground;
            return div()
                .size_full()
                .text_color(theme)
                .child(self.render_welcome(welcome, cx))
                .into_any_element();
        }
        let theme = cx.theme();
        h_flex()
            .key_context(APP_CONTEXT)
            .on_action(cx.listener(|app, _: &FocusComposer, window, cx| {
                if app.selected.is_some() {
                    app.composer.read(cx).focus_handle(cx).focus(window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &FocusSidebar, window, cx| {
                app.sidebar_focus.focus(window, cx);
            }))
            .on_action(cx.listener(|app, _: &NewBot, window, cx| {
                if app.connection == Connection::Connected {
                    app.open_create_dialog(window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &StopWork, window, cx| {
                if app.selected_bot().is_some_and(|b| b.is_active()) {
                    app.stop_work(window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &DeleteBot, window, cx| {
                app.open_delete_dialog(window, cx);
            }))
            .on_action(cx.listener(|app, _: &LoadOlder, window, cx| app.load_older(window, cx)))
            .on_action(cx.listener(|app, _: &NewCategory, window, cx| {
                if app.connection == Connection::Connected {
                    app.open_category_dialog(None, window, cx);
                }
            }))
            .on_action(cx.listener(|app, _: &MoveBot, window, cx| {
                if app.connection == Connection::Connected {
                    app.open_move_dialog(window, cx);
                }
            }))
            .size_full()
            .text_color(theme.foreground)
            .child(self.render_sidebar(cx))
            .child(self.render_main(cx))
            .into_any_element()
    }
}
