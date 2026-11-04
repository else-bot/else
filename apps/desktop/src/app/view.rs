//! Rendering of the sidebar, header, conversation, and composer.

use super::reply::{Author, MessageReference};
use super::row_text::RowText;
use super::{
    APP_CONTEXT, COMPOSER_CONTEXT, Connection, DeleteBot, ElseApp, FocusComposer, FocusSidebar,
    LoadOlder, MoveBot, NewBot, NewCategory, RowKey, SIDEBAR_CONTEXT, SelectNext, SelectPrev,
    StopWork, ToggleSettings,
};
use crate::api::Bot;
use crate::avatar::bot_avatar;
use crate::model::ReactionKind;
use chrono::{DateTime, Datelike as _, Local, TimeZone, Utc};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Textarea;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, h_flex, v_flex,
};
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
    pub(crate) fn connected(&self) -> bool {
        self.connection == Connection::Connected
    }

    pub(crate) fn status_text(&self) -> (SharedString, bool) {
        match &self.connection {
            Connection::Connecting => ("Connecting…".into(), false),
            Connection::Connected => ("Connected".into(), false),
            Connection::Disconnected(message) => {
                (format!("Disconnected — retrying. {message}").into(), true)
            }
            Connection::InvalidCredentials(message) => (message.clone().into(), true),
        }
    }

    pub(super) fn provider_label(&self, id: &str) -> String {
        self.caps
            .as_ref()
            .and_then(|caps| caps.providers.iter().find(|p| p.id == id))
            .map(|p| p.label.clone())
            .unwrap_or_else(|| id.to_owned())
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut list = v_flex()
            .id("bot-list")
            .gap_1()
            .p_2()
            .flex_1()
            .overflow_y_scroll();
        let grouped = !self.categories.is_empty();
        let layout = crate::sidebar::Layout::new(&self.categories, &self.bots, self.default_bot_id);
        let can_edit = self.connection == Connection::Connected;
        let owner = cx.entity().downgrade();
        if let Some(notice) = self.category_notice.clone() {
            list = list.child(
                div()
                    .px_2()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(notice),
            );
        }
        if let Some(main) = layout.pinned {
            list = list.child(self.render_bot_row(main, cx));
        }
        for group in layout.groups {
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

    fn render_bot_row(&self, bot: &Bot, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let id = bot.id;
        let selected = self.selected == Some(id);
        let unread = self.unread.contains(&id);
        let pending = !bot.is_active();
        let activity = if pending {
            "Deleting…".to_owned()
        } else {
            activity_label(bot)
        };
        let mut name = div().text_sm().truncate().child(bot.name.clone());
        if unread {
            name = name.font_weight(FontWeight::SEMIBOLD);
        }
        let mut name_row = h_flex().gap_2().items_center().child(name);
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
        if unread {
            name_row = name_row.child(
                div()
                    .ml_auto()
                    .flex_none()
                    .size_2()
                    .rounded_full()
                    .bg(theme.primary),
            );
        }
        let mut avatar = bot_avatar("avatar", bot.avatar, px(28.));
        if pending {
            avatar = avatar.still();
        }
        let mut row = h_flex()
            .id(SharedString::from(format!("bot-{id}")))
            .gap_2p5()
            .items_center()
            .px_2()
            .py_1p5()
            .rounded_md()
            .child(avatar)
            .child(
                v_flex().flex_1().min_w_0().child(name_row).child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(activity),
                ),
            );
        if pending {
            row = row.opacity(0.5);
        } else {
            row = row
                .cursor_pointer()
                .hover(|style| style.bg(theme.list_hover))
                .on_click(cx.listener(move |app, _, window, cx| app.open_bot(id, window, cx)));
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
        if pending {
            return row.into_any_element();
        }
        let owner = cx.entity().downgrade();
        row.context_menu(move |menu, window, cx| {
            Self::bot_menu(owner.clone(), id, menu, window, cx)
        })
        .into_any_element()
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
        if !self.settings_open {
            actions = actions.child(
                Button::new("toggle-settings")
                    .icon(IconName::PanelRightOpen)
                    .small()
                    .ghost()
                    .accessibility_label("Show bot settings")
                    .tooltip_with_action("Show bot settings", &ToggleSettings, Some(APP_CONTEXT))
                    .on_click(cx.listener(|app, _, window, cx| app.toggle_settings(window, cx))),
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
                h_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_2()
                    .items_center()
                    .child(bot_avatar(
                        ElementId::Name(format!("header-avatar-{}", bot.id).into()),
                        bot.avatar,
                        px(26.),
                    ))
                    .child(
                        div()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(bot.name.clone()),
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
        let card = |text: &RowText, user: bool| {
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
                .child(text.render())
        };
        // Wrapped text needs a definite width, so bubbles span the row and
        // are offset to their side.
        let side = |user: bool, card: AnyElement| {
            let side = div().w_full();
            let side = if user {
                side.pl(px(96.))
            } else {
                side.pr(px(96.))
            };
            side.child(card)
        };
        let reacted = |card: AnyElement, kind: ReactionKind| {
            const HEIGHT: f32 = 30.;
            div()
                .relative()
                .w_full()
                .pt(px(HEIGHT / 2.))
                .child(card)
                .child(
                    h_flex()
                        .absolute()
                        .top_0()
                        .right(px(-6.))
                        .h(px(HEIGHT))
                        .min_w(px(HEIGHT + 4.))
                        .px_1p5()
                        .justify_center()
                        .rounded_full()
                        .bg(theme.secondary)
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(16.))
                        .line_height(px(20.))
                        .child(kind.mark()),
                )
                .into_any_element()
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
                let Some(text) = view.texts.get(&key) else {
                    return div().into_any_element();
                };
                let user = item.kind == "user";
                let turn = conversation.turn(item.turn_id);
                let run = conversation.run(id);
                // Children stretch so bubbles get a definite width; notes
                // are aligned to the bubble's side.
                let mut column = v_flex().w_full().gap_1().px_4().py_1();
                let card = card(text, user);
                let card = match Author::of(&item.kind) {
                    Some(author) => {
                        let (owner, bot) = (cx.entity().downgrade(), view.bot_id);
                        card.id(SharedString::from(format!("message-{id}")))
                            .context_menu(move |menu, window, cx| {
                                Self::message_menu(owner.clone(), bot, id, author, menu, window, cx)
                            })
                            .into_any_element()
                    }
                    None => card.into_any_element(),
                };
                let card = match conversation.reaction_for(id).filter(|_| !user) {
                    Some(kind) => reacted(card, kind),
                    None => card,
                };
                column = column.child(side(user, card)).children(
                    item.created_at
                        .map(|at| beside(user, note(stamp(at), false))),
                );
                if let Some(turn) = turn {
                    let show_on_user = user && !turn.is_finished()
                        || user
                            && turn.status != "completed"
                            && !conversation
                                .items()
                                .any(|i| i.turn_id == turn.id && i.kind == "assistant");
                    let show_on_assistant = !user && run.last && turn.status != "completed";
                    if show_on_user || show_on_assistant {
                        let (label, error) = turn_label(turn);
                        if !label.is_empty() {
                            column = column.child(beside(user, note(label, error)));
                        }
                    }
                }
                column.into_any_element()
            }
            RowKey::SecretRequest(id) => self.render_secret_card(id, cx),
            RowKey::Pending(request_id) => {
                let Some(pending) = self
                    .selected
                    .and_then(|bot| self.pending.get(&bot))
                    .and_then(|list| list.iter().find(|p| p.request_id == request_id))
                else {
                    return div().into_any_element();
                };
                let Some(text) = view.texts.get(&key) else {
                    return div().into_any_element();
                };
                let mut column = v_flex()
                    .w_full()
                    .gap_1()
                    .px_4()
                    .py_1()
                    .child(side(true, card(text, true).into_any_element()))
                    .child(beside(true, note(stamp(pending.sent_at), false)));
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
        if let Some(reference) = self.drafts.get(&bot.id).and_then(|d| d.reply_to.as_ref()) {
            footer = footer.child(self.render_reply_strip(bot, reference, cx));
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
                        .h(COMPOSER_ROW)
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

    fn render_reply_strip(
        &self,
        bot: &Bot,
        reference: &MessageReference,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let bot_id = bot.id;
        let author = match reference.author {
            Author::Owner => "yourself".to_owned(),
            Author::Bot => bot.name.clone(),
        };
        let preview = reference
            .text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        h_flex()
            .id(SharedString::from(format!("reply-{}", reference.item)))
            .min_w_0()
            .gap_2()
            .items_center()
            .pl_3()
            .pr_1()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .text_sm()
            .child(
                div()
                    .flex_none()
                    .max_w(relative(0.5))
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("Replying to {author}")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.muted_foreground)
                    .child(preview),
            )
            .child(
                Button::new("cancel-reply")
                    .flex_none()
                    .icon(IconName::Close)
                    .xsmall()
                    .ghost()
                    .tooltip("Cancel reply")
                    .accessibility_label("Cancel reply")
                    .on_click(
                        cx.listener(move |app, _, window, cx| {
                            app.dismiss_reply(bot_id, window, cx)
                        }),
                    ),
            )
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
        "waiting" if turn.secret_requests.iter().any(|r| r.is_pending()) => (
            "Waiting for you to answer the secret request…".into(),
            false,
        ),
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

const NARROW: Pixels = px(1000.);

/// One row of the composer: gpui-kit's medium Textarea draws a 1.25rem line
/// (20px at the default rem) with 8px padding above and below inside a 1px
/// border. A medium Button is only 32px, so Send takes this height to line up.
const COMPOSER_ROW: Pixels = px(38.);

fn beside(user: bool, note: Div) -> Div {
    let row = h_flex().w_full().child(note);
    if user { row.justify_end() } else { row }
}

fn stamp(at: DateTime<Utc>) -> String {
    stamp_in(at, Local::now())
}

fn stamp_in<Tz: TimeZone>(at: DateTime<Utc>, now: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let at = at.with_timezone(&now.timezone());
    if at.year() == now.year() {
        at.format("%-d %b, %H:%M").to_string()
    } else {
        at.format("%-d %b %Y, %H:%M").to_string()
    }
}

impl Render for ElseApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(welcome) = &self.welcome {
            let theme = cx.theme().foreground;
            return div()
                .size_full()
                .text_color(theme)
                .child(self.render_welcome(welcome, cx))
                .into_any_element();
        }
        let settings = self
            .settings_shown()
            .then(|| self.selected_bot().cloned())
            .flatten();
        let width = window.viewport_size().width;
        let crowded = settings.is_some() && width < NARROW;
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
                if let Some(id) = app.selected {
                    app.open_delete_dialog(id, window, cx);
                }
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
            .on_action(
                cx.listener(|app, _: &ToggleSettings, window, cx| app.toggle_settings(window, cx)),
            )
            .size_full()
            .text_color(theme.foreground)
            .children((!crowded).then(|| self.render_sidebar(cx)))
            .child(self.render_main(cx))
            .children(settings.map(|bot| self.render_settings(&bot, cx)))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::stamp_in;
    use chrono::{DateTime, FixedOffset, Utc};

    #[test]
    fn stamps_use_the_viewer_zone_and_name_another_year() {
        let zone = FixedOffset::east_opt(2 * 3600).unwrap();
        let now = "2026-11-04T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .unwrap()
            .with_timezone(&zone);
        let at = |text: &str| text.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(stamp_in(at("2026-11-04T07:05:00Z"), now), "4 Nov, 09:05");
        assert_eq!(stamp_in(at("2026-01-31T23:30:00Z"), now), "1 Feb, 01:30");
        assert_eq!(
            stamp_in(at("2025-12-31T21:59:00Z"), now),
            "31 Dec 2025, 23:59"
        );
    }
}
