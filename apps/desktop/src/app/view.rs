use super::layout::{CategoryDrag, SidebarEdge};
use super::mentions::MentionAction;
use super::{
    APP_CONTEXT, COMPOSER_CONTEXT, CiteSelection, Connection, DeleteBot, ElseApp, FocusComposer,
    FocusSidebar, LoadOlder, MoveBot, NewBot, NewCategory, SIDEBAR_CONTEXT, SelectNext, SelectPrev,
    StopWork, TakeOverComputer, ToggleSettings,
};
use crate::api::Bot;
use crate::avatar::bot_avatar;
use crate::sidebar::status::{Kind, Subtitle};
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::Icon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Enter, Escape, IndentInline, MoveDown, MoveUp, Textarea};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
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
            .rounded(theme.radius)
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

    fn render_sidebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut list = v_flex()
            .id("bot-list")
            .gap_0p5()
            .px_2()
            .pb_2()
            .flex_1()
            .overflow_y_scroll();
        let grouped = !self.categories.is_empty();
        let layout = self.sidebar_layout();
        let can_edit = self.connection == Connection::Connected;
        let owner = cx.entity().downgrade();
        let category_order: std::rc::Rc<[Uuid]> = self.categories.iter().map(|c| c.id).collect();
        if let Some(notice) = self.category_notice.clone() {
            list = list.child(
                div()
                    .px_2()
                    .pb_1()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(notice),
            );
        }
        if let Some(main) = layout.pinned {
            list = list.child(self.render_bot_row(main, cx));
        }
        if !layout.waiting.is_empty() {
            let mut tray = v_flex()
                .id("waiting-tray")
                .mt_2()
                .p_0p5()
                .gap_0p5()
                .rounded(theme.radius)
                .border_1()
                .border_color(theme.warning.opacity(0.35))
                .bg(theme.warning.opacity(0.06))
                .child(
                    h_flex()
                        .h(px(28.))
                        .px_2()
                        .items_center()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.warning)
                        .child(div().flex_1().child("Needs you"))
                        .child(layout.waiting.len().to_string()),
                );
            for bot in layout.waiting {
                tray = tray.child(self.render_bot_row(bot, cx));
            }
            list = list.child(tray);
        }
        for group in layout.groups {
            let destination = group.category.map(|c| c.id);
            let drop_owner = owner.clone();
            let category_target = destination;
            let mut zone = v_flex()
                .id(SharedString::from(match destination {
                    Some(id) => format!("category-group-{id}"),
                    None => "category-group-unassigned".to_owned(),
                }))
                .gap_0p5()
                .rounded(theme.radius)
                .can_drop(move |value, _, cx| {
                    value
                        .downcast_ref::<CategoryDrag>()
                        .is_some_and(|drag| category_target.is_some_and(|id| id != drag.id))
                        || value.downcast_ref::<BotDrag>().is_some_and(|drag| {
                            drop_owner.upgrade().is_some_and(|app| {
                                app.read(cx).can_assign_category(drag.bot, destination)
                            })
                        })
                })
                .drag_over::<BotDrag>(|style, _, _, cx| style.bg(cx.theme().drop_target))
                .on_drop(cx.listener(move |app, drag: &BotDrag, window, cx| {
                    app.assign_category(drag.bot, destination, window, cx)
                }));
            if grouped {
                let name = group
                    .category
                    .map_or_else(|| "Unassigned".to_owned(), |c| c.name.clone());
                let collapsed = self.preferences.collapsed.contains(&destination);
                let mut header = h_flex()
                    .id(SharedString::from(format!(
                        "category-header-{destination:?}"
                    )))
                    .mt_3()
                    .h(px(28.))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .rounded(theme.radius)
                    .cursor_pointer()
                    .hover(|style| {
                        style
                            .bg(theme.sidebar_accent)
                            .text_color(theme.sidebar_foreground)
                    })
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(name.clone()),
                    )
                    .on_click(
                        cx.listener(move |app, _, _, cx| app.toggle_category(destination, cx)),
                    );
                if let Some(category) = group.category.cloned() {
                    let context = owner.clone();
                    header = header.on_drag(
                        CategoryDrag {
                            id: category.id,
                            name: name.clone().into(),
                        },
                        |drag, _, _, cx| cx.new(|_| drag.clone()),
                    );
                    let order = category_order.clone();
                    let target = category.id;
                    zone = zone
                        .drag_over::<CategoryDrag>(move |style, drag, _, cx| {
                            let style = style.rounded_none().border_color(cx.theme().drag_border);
                            if crate::sidebar::lands_below(&order, drag.id, target) {
                                style.border_b_2()
                            } else {
                                style.border_t_2()
                            }
                        })
                        .on_drop(cx.listener(move |app, drag: &CategoryDrag, _, cx| {
                            app.reorder_category(drag.id, category.id, cx);
                        }))
                        .child(header.context_menu(move |menu, _, cx| {
                            Self::category_menu(context.clone(), &category, menu, cx)
                        }));
                } else {
                    zone = zone.child(header);
                }
            }
            if !grouped || !self.preferences.collapsed.contains(&destination) {
                if grouped && group.bots.is_empty() {
                    zone = zone.child(
                        div()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("No bots"),
                    );
                }
                for bot in group.bots {
                    zone = zone.child(self.render_bot_row(bot, cx));
                }
            }
            list = list.child(zone);
        }
        let brand = h_flex()
            .flex_none()
            .h(px(52.))
            .pl_4()
            .pr_2()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .child("else")
                    .child(div().text_color(theme.primary).child(".")),
            )
            .child(
                Button::new("new-bot")
                    .focus_ring(false)
                    .cursor_pointer()
                    .icon(IconName::SquarePen)
                    .small()
                    .ghost()
                    .accessibility_label("New bot")
                    .tooltip_with_action("New bot", &NewBot, Some(APP_CONTEXT))
                    .disabled(!can_edit)
                    .on_click(cx.listener(|app, _, window, cx| app.open_create_dialog(window, cx))),
            );
        let footer =
            h_flex()
                .flex_none()
                .px_2()
                .py_2()
                .border_t_1()
                .border_color(theme.sidebar_border)
                .child(
                    Button::new("new-category")
                        .focus_ring(false)
                        .cursor_pointer()
                        .icon(IconName::FolderPlus)
                        .label("New category")
                        .small()
                        .ghost()
                        .w_full()
                        .justify_start()
                        .children(
                            Kbd::binding_for_action(&NewCategory, Some(APP_CONTEXT), window).map(
                                |kbd| {
                                    div()
                                        .flex_none()
                                        .ml_auto()
                                        .pl_2()
                                        .text_xs()
                                        .child(kbd.appearance(false))
                                },
                            ),
                        )
                        .disabled(!can_edit)
                        .on_click(cx.listener(|app, _, window, cx| {
                            app.open_category_dialog(None, window, cx)
                        })),
                );
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
            .relative()
            .w(px(self.preferences.sidebar_width))
            .on_drag_move(cx.listener(Self::resize_sidebar))
            .flex_none()
            .h_full()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(brand)
            .child(list)
            .child(footer)
            .child(self.sidebar_handle(SidebarEdge::Left, cx))
    }

    /// Up to three overlapping faces of the bots in `ids`, each once.
    pub(super) fn face_stack(
        &self,
        key: &str,
        ids: impl IntoIterator<Item = Uuid>,
        size: Pixels,
    ) -> Option<Div> {
        let mut shown: Vec<&Bot> = Vec::new();
        for id in ids {
            if shown.len() < 3
                && !shown.iter().any(|bot| bot.id == id)
                && let Some(bot) = self.bots.iter().find(|bot| bot.id == id)
            {
                shown.push(bot);
            }
        }
        (!shown.is_empty()).then(|| {
            h_flex()
                .flex_none()
                .children(shown.into_iter().enumerate().map(|(ix, bot)| {
                    div().when(ix > 0, |face| face.ml(px(-5.))).child(
                        bot_avatar(
                            SharedString::from(format!("{key}-{}", bot.id)),
                            bot.avatar,
                            size,
                        )
                        .still(),
                    )
                }))
        })
    }

    /// The faces of the bots `bot` waits on, before its status text.
    fn waiting_faces(&self, key: &str, bot: &Bot) -> Option<Div> {
        if bot.waiting().is_some() {
            return None;
        }
        let waiting = bot.activity.as_ref()?.waiting_on.as_ref()?;
        self.face_stack(
            &format!("{key}-{}", bot.id),
            waiting.bots.iter().map(|b| b.id),
            px(14.),
        )
    }

    fn render_bot_identity_avatar(
        &self,
        id: impl Into<ElementId>,
        bot: &Bot,
        working: bool,
    ) -> impl IntoElement {
        let mut avatar = bot_avatar(id, bot.avatar, px(30.));
        if !bot.is_active() {
            avatar = avatar.still();
        } else if working {
            avatar = avatar.working();
        }
        div()
            .relative()
            .flex_none()
            .size(px(30.))
            .child(avatar)
            .when(self.default_bot_id == Some(bot.id), |avatar| {
                avatar.child(
                    div()
                        .id("main-bot-badge")
                        .role(Role::Image)
                        .aria_label("Main bot")
                        .absolute()
                        .right_0()
                        .bottom_0()
                        .size(px(12.))
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(rgb(0xf59e0b))
                        .text_color(white())
                        .child(Icon::new(IconName::Star).with_size(px(8.))),
                )
            })
    }

    fn render_bot_row(&self, bot: &Bot, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let id = bot.id;
        let selected = self.selected == Some(id);
        let unread = self.is_unread(id);
        let pending = !bot.is_active();
        let muted = bot.muted && !pending;
        let connected = self.connected();
        let subtitle = Subtitle::new(bot, connected, self.stopping.contains(&id));
        let waiting_on = bot
            .activity
            .as_ref()
            .and_then(|a| a.waiting_on.as_ref())
            .filter(|_| subtitle.kind == Kind::Waiting);
        let waited = bot
            .waiting()
            .and_then(|w| w.since)
            .or(waiting_on.map(|w| w.since))
            .filter(|_| self.default_bot_id != Some(id))
            .map(|since| crate::sidebar::status::waited(since, chrono::Utc::now()));
        let faces = waiting_on
            .is_some()
            .then(|| self.waiting_faces("sidebar-waiting", bot))
            .flatten();
        let working = subtitle.kind == Kind::Working && connected;
        let color = if !connected {
            theme.muted_foreground
        } else {
            match subtitle.kind {
                Kind::Working => theme.primary,
                Kind::Answer | Kind::Approval | Kind::Takeover | Kind::Credentials => theme.warning,
                Kind::Failed | Kind::Uncertain | Kind::Unavailable => theme.danger,
                _ => theme.muted_foreground,
            }
        };
        let icon = match subtitle.kind {
            Kind::Message | Kind::Working => None,
            Kind::Waiting if faces.is_some() => None,
            Kind::Waiting => Some(IconName::Hourglass),
            Kind::Queued => Some(IconName::Clock),
            Kind::Answer => Some(IconName::CircleQuestionMark),
            Kind::Approval => Some(IconName::BadgeCheck),
            Kind::Takeover => Some(IconName::Laptop),
            Kind::Credentials => Some(IconName::LockKeyhole),
            Kind::Stopping => Some(IconName::Square),
            Kind::Stopped => Some(IconName::Pause),
            Kind::Failed => Some(IconName::CircleX),
            Kind::Uncertain => Some(IconName::TriangleAlert),
            Kind::Unavailable => Some(IconName::CircleOff),
            Kind::Deleting => Some(IconName::Trash),
        };
        let subtitle = h_flex()
            .min_w_0()
            .gap_1()
            .items_center()
            .text_xs()
            .text_color(color)
            .when(!pending, |line| {
                line.group_hover("sidebar-bot-row", |style| {
                    style.text_color(theme.foreground)
                })
            })
            .when(working, |line| {
                line.child(working_dots(id, color, cx.reduce_motion()))
            })
            .children(faces)
            .when_some(icon, |line, icon| {
                line.child(
                    div()
                        .flex_none()
                        .child(Icon::new(icon).xsmall().text_color(color)),
                )
            })
            .child(div().flex_1().min_w_0().truncate().child(subtitle.text));
        let mut row = h_flex()
            .id(SharedString::from(format!("bot-{id}")))
            .group("sidebar-bot-row")
            .min_h(px(46.))
            .gap_2p5()
            .items_center()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .child(self.render_bot_identity_avatar("avatar", bot, working))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .truncate()
                                    .font_weight(if unread {
                                        FontWeight::SEMIBOLD
                                    } else {
                                        FontWeight::MEDIUM
                                    })
                                    .child(bot.name.clone()),
                            )
                            .when(muted, |line| {
                                line.child(
                                    div()
                                        .id("muted")
                                        .flex_none()
                                        .role(Role::Image)
                                        .aria_label("Muted")
                                        .child(
                                            Icon::new(IconName::BellOff)
                                                .xsmall()
                                                .text_color(theme.muted_foreground),
                                        ),
                                )
                            }),
                    )
                    .child(subtitle),
            )
            .children(waited.map(|waited| {
                div()
                    .flex_none()
                    .self_start()
                    .mt_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(waited)
            }))
            .children(unread.then(|| div().flex_none().size_2().rounded_full().bg(theme.primary)));
        if pending {
            row = row.opacity(0.5);
        } else {
            row = row
                .when(bot.silenced() && !pending && !selected, |row| {
                    row.opacity(0.5)
                })
                .cursor_pointer()
                .hover(|style| style.bg(theme.sidebar_accent).opacity(1.))
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
        let theme = cx.theme().clone();
        let unshown_status = self
            .view
            .as_ref()
            .and_then(|view| view.conversation.as_ref())
            .and_then(|conversation| conversation.latest_unshown_turn())
            .and_then(|turn| self.render_turn_status(turn, bot.id, cx));
        let mut actions = h_flex().flex_none().gap_1().items_center();
        if !self.settings_open {
            actions = actions.child(
                Button::new("toggle-settings")
                    .focus_ring(false)
                    .cursor_pointer()
                    .icon(IconName::PanelRightOpen)
                    .small()
                    .ghost()
                    .accessibility_label("Show bot details")
                    .tooltip_with_action("Show bot details", &ToggleSettings, Some(APP_CONTEXT))
                    .on_click(cx.listener(|app, _, window, cx| app.toggle_settings(window, cx))),
            );
        }
        let main = self.default_bot_id == Some(bot.id);
        h_flex()
            .flex_none()
            .h(HEADER_HEIGHT)
            .border_b_1()
            .border_color(theme.foreground.opacity(0.06))
            .px_5()
            .gap_3()
            .items_center()
            .child(self.render_bot_identity_avatar(
                ElementId::Name(format!("header-avatar-{}", bot.id).into()),
                bot,
                self.connected()
                    && Subtitle::new(bot, true, self.stopping.contains(&bot.id)).kind
                        == Kind::Working,
            ))
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(bot.name.clone()),
                    )
                    .child(
                        h_flex()
                            .min_w_0()
                            .gap_1()
                            .items_center()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .children(self.waiting_faces("header-waiting", bot))
                            .child(div().min_w_0().truncate().child(subtitle(bot, main))),
                    ),
            )
            .children(unshown_status)
            .child(actions)
    }

    fn render_conversation(
        &self,
        bot: &Bot,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let view = self.view.as_ref().filter(|v| v.bot_id == bot.id);
        let transcript: AnyElement = match view {
            Some(view) if view.conversation.is_some() || !view.rows.is_empty() => {
                if view.rows.is_empty() {
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .px_5()
                        .child(bot_avatar("empty-conversation-avatar", bot.avatar, px(40.)).still())
                        .child(
                            div()
                                .text_size(px(18.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(bot.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(14.))
                                .text_color(theme.muted_foreground)
                                .child("Send a message to get started."),
                        )
                        .into_any_element()
                } else {
                    let older = view
                        .conversation
                        .as_ref()
                        .is_some_and(|c| c.older_before_seq.is_some());
                    let mut column = v_flex().flex_1().min_w_0().min_h_0();
                    if older {
                        let loading = view.loading_older;
                        column =
                            column.child(
                                h_flex().justify_center().py_1().child(
                                    Button::new("load-older")
                                        .focus_ring(false)
                                        .cursor_pointer()
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
                    let mut transcript = div().relative().flex_1().min_w_0().min_h_0().child(
                        list(
                            view.list.clone(),
                            cx.processor(|app, ix, window, cx| {
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .justify_center()
                                    .child(app.render_row(ix, window, cx))
                                    .into_any_element()
                            }),
                        )
                        .size_full()
                        .py_2(),
                    );
                    if self.unseen_below() {
                        transcript = transcript.child(
                            h_flex()
                                .absolute()
                                .bottom_3()
                                .w_full()
                                .justify_center()
                                .child(
                                    Button::new("jump-to-latest")
                                        .focus_ring(false)
                                        .cursor_pointer()
                                        .label("New messages")
                                        .icon(IconName::ArrowDown)
                                        .small()
                                        .primary()
                                        .rounded_full()
                                        .shadow_md()
                                        .on_click(cx.listener(|app, _, window, cx| {
                                            app.jump_to_latest(window, cx)
                                        })),
                                ),
                        );
                    }
                    column.child(transcript).into_any_element()
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
        let mut notices = v_flex().gap_1();
        if let Some(problem) = view.and_then(|v| v.stream_problem.clone()) {
            notices = notices.child(div().text_xs().text_color(theme.warning).child(problem));
        }
        if let Some(notice) = &self.notice {
            notices = notices.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(notice.clone()),
            );
        }
        if let Some(text) = self.pause.text(std::time::Instant::now()) {
            notices = notices.child(div().text_xs().text_color(theme.warning).child(text));
        }
        let owner = cx.entity().downgrade();
        let preview_owner = owner.clone();
        let files = self
            .drafts
            .get(&bot.id)
            .map(|d| d.files.clone())
            .unwrap_or_default();
        let preview_files = files.clone();
        let bot_id = bot.id;
        let commented = self
            .drafts
            .get(&bot.id)
            .is_some_and(|d| d.quotes.iter().any(|q| !q.comment.trim().is_empty()));
        let can_send = self.connection == Connection::Connected
            && (commented || !self.composer.read(cx).value().trim().is_empty())
            && self.composer.read(cx).tokens().iter().all(|span| {
                crate::attachments::file_id(span.token().id())
                    .and_then(|id| files.get(&id))
                    .is_some_and(|file| {
                        matches!(file.state, super::attachments::UploadState::Ready { .. })
                    })
            });
        let activity = self
            .view
            .as_ref()
            .and_then(|v| v.conversation.as_ref())
            .and_then(|c| c.activity.clone())
            .or(bot.activity.clone());
        let requested_stop = self.stopping.contains(&bot.id);
        let working = requested_stop || activity.as_ref().is_some_and(|a| a.phase != "idle");
        let stopping = requested_stop || activity.as_ref().is_some_and(|a| a.phase == "cancelling");
        let mut frame = v_flex()
            .key_context(COMPOSER_CONTEXT)
            .capture_action(
                cx.listener(|app, _: &gpui_kit::base::input::Paste, window, cx| {
                    if app.paste_clipboard(window, cx) {
                        cx.stop_propagation();
                    }
                }),
            )
            .capture_action(cx.listener(|app, _: &MoveDown, window, cx| {
                app.mention_action(MentionAction::Next, window, cx);
            }))
            .capture_action(cx.listener(|app, _: &MoveUp, window, cx| {
                app.mention_action(MentionAction::Previous, window, cx);
            }))
            .capture_action(cx.listener(|app, action: &Enter, window, cx| {
                if !action.shift && !action.secondary {
                    app.mention_action(MentionAction::Accept, window, cx);
                }
            }))
            .capture_action(cx.listener(|app, _: &IndentInline, window, cx| {
                app.mention_action(MentionAction::Accept, window, cx);
            }))
            .capture_action(cx.listener(|app, _: &Escape, window, cx| {
                app.mention_action(MentionAction::Dismiss, window, cx);
            }))
            .capture_action(cx.listener(|app, _: &FocusSidebar, window, cx| {
                app.mention_action(MentionAction::Dismiss, window, cx);
            }))
            .w_full()
            .min_w_0()
            .relative()
            .pl_3()
            .pr(px(if working && can_send { 88. } else { 48. }))
            .py_2()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover);
        if let Some(quotes) = self.render_quote_stack(bot.id, &bot.name, cx) {
            frame = frame.child(quotes);
        }
        frame = frame
            .child(
                Textarea::new(&self.composer)
                    .appearance(false)
                    .bordered(false)
                    .min_w_0()
                    .min_h(px(40.))
                    .text_size(px(14.))
                    .token(move |token, _, cx| {
                        super::attachments::draft_chip(token, &files, owner.clone(), cx)
                    })
                    .on_token_click(move |event, window, cx| {
                        if let Some((id, file)) = crate::attachments::file_id(event.token().id())
                            .and_then(|id| preview_files.get(&id).map(|file| (id, file)))
                        {
                            let _ = preview_owner.update(cx, |app, cx| {
                                app.preview_attachment(
                                    bot_id,
                                    file.preview_metadata(id),
                                    window,
                                    cx,
                                )
                            });
                        }
                    })
                    .aria_label(format!("Message {}", bot.name)),
            )
            .child(
                h_flex()
                    .absolute()
                    .right_2()
                    .bottom_2()
                    .gap_2()
                    .items_center()
                    .when(!working || can_send, |row| {
                        row.child(
                            Button::new("send")
                                .focus_ring(false)
                                .when(can_send, |button| button.cursor_pointer())
                                .icon(IconName::ArrowUp)
                                .primary()
                                .small()
                                .size(px(32.))
                                .accessibility_label("Send message")
                                .disabled(!can_send)
                                .on_click(cx.listener(|app, _, window, cx| app.send(window, cx))),
                        )
                    })
                    .when(working, |row| {
                        row.child(
                            Button::new("stop-work")
                                .focus_ring(false)
                                .when(!stopping, |button| button.cursor_pointer())
                                .icon(IconName::Square)
                                .small()
                                .size(px(32.))
                                .accessibility_label(if stopping {
                                    "Stopping work"
                                } else {
                                    "Stop work"
                                })
                                .tooltip_with_action(
                                    if stopping { "Stopping…" } else { "Stop work" },
                                    &StopWork,
                                    Some(APP_CONTEXT),
                                )
                                .loading(stopping)
                                .on_click(
                                    cx.listener(|app, _, window, cx| app.stop_work(window, cx)),
                                ),
                        )
                    }),
            );
        let footer = reading_column()
            .flex_none()
            .gap_1p5()
            .pt_1()
            .pb_3()
            .child(notices)
            .children(self.render_waiting_tray(cx))
            .children(self.render_question_bar(&bot.name, cx))
            .children(self.render_handoff_bar(bot, window, cx))
            .child(
                div()
                    .relative()
                    .child(frame)
                    .children(self.render_mentions(cx)),
            )
            .child(
                div()
                    .px_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("Enter to send · Shift+Enter for a new line"),
            );
        v_flex()
            .size_full()
            .min_w_0()
            .child(self.render_header(bot, cx))
            .child(transcript)
            .child(footer)
    }

    fn render_main(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        if let Some(bot) = self.selected_bot().filter(|b| b.is_active()).cloned() {
            return v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_hidden()
                .bg(theme.background)
                .child(self.render_conversation(&bot, window, cx))
                .into_any_element();
        }
        let message = match self.connection {
            Connection::Connected => "Create a bot to start a conversation.",
            Connection::InvalidCredentials(_) => {
                "Open Settings to fix the server address or owner key."
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
                    .focus_ring(false)
                    .cursor_pointer()
                    .label("Create bot")
                    .primary()
                    .on_click(cx.listener(|app, _, window, cx| app.open_create_dialog(window, cx))),
            );
        }
        body.into_any_element()
    }
}

/// Three dots that ripple left to right in place of a spinner icon.
pub(super) fn working_dots(id: Uuid, color: Hsla, still: bool) -> impl IntoElement {
    h_flex()
        .flex_none()
        .size(px(14.))
        .gap(px(2.))
        .items_center()
        .justify_center()
        .children((0..3).map(move |i| {
            let dot = div().relative().size(px(3.)).rounded_full().bg(color);
            if still {
                return dot.into_any_element();
            }
            dot.with_animation(
                SharedString::from(format!("working-dot-{id}-{i}")),
                Animation::new(std::time::Duration::from_millis(1050)).repeat(),
                move |dot, t| {
                    let lift = (std::f32::consts::TAU * t - i as f32 * 0.75).sin().max(0.);
                    dot.top(px(-2.6 * lift * lift)).opacity(0.45 + 0.55 * lift)
                },
            )
            .into_any_element()
        }))
}

fn subtitle(bot: &Bot, main: bool) -> String {
    let idle = || {
        if main {
            "Main assistant".to_owned()
        } else {
            "Idle".to_owned()
        }
    };
    let Some(activity) = &bot.activity else {
        return idle();
    };
    // An owner request outranks the bots it waits on, as in the sidebar.
    let working = match activity
        .waiting_on
        .as_ref()
        .filter(|_| bot.waiting().is_none())
    {
        Some(waiting) => crate::sidebar::status::waiting_label(waiting),
        None => "Working".into(),
    };
    match activity.phase.as_str() {
        "working" if activity.queued_count > 0 => {
            format!("{working} · {} queued", activity.queued_count)
        }
        "working" => working,
        "queued" => format!("{} queued", activity.queued_count),
        "cancelling" => "Stopping…".into(),
        "unavailable" => "Unavailable".into(),
        _ => idle(),
    }
}

pub(super) const HEADER_HEIGHT: Pixels = px(56.);
pub(super) const READING_WIDTH: Pixels = px(760.);

pub(super) fn reading_column() -> Div {
    v_flex()
        .flex_none()
        .w_full()
        .min_w_0()
        .max_w(READING_WIDTH)
        .mx_auto()
        .px_5()
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
        if let Some(computer) = self.computer.clone().filter(|view| view.read(cx).expanded) {
            return h_flex()
                .size_full()
                .child(div().flex_1().min_w_0().h_full().child(computer))
                .children(self.render_handoff_panel(cx))
                .into_any_element();
        }
        let settings = self
            .settings_shown()
            .then(|| self.selected_bot().cloned())
            .flatten();
        let width = window.viewport_size().width;
        let crowded = settings.is_some()
            && f32::from(width)
                < self.preferences.sidebar_width + self.preferences.details_width + 360.;
        let theme = cx.theme();
        h_flex()
            .relative()
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
            .on_action(cx.listener(|app, _: &TakeOverComputer, window, cx| {
                app.take_over_computer(window, cx);
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
            .on_action(
                cx.listener(|app, _: &CiteSelection, window, cx| app.cite_selection(window, cx)),
            )
            .capture_any_mouse_down(cx.listener(|app, event: &MouseDownEvent, _, _| {
                if event.button == MouseButton::Left {
                    app.quote_press = Some(event.position);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|app, event: &MouseUpEvent, window, cx| {
                    app.offer_quote(event.position, window, cx)
                }),
            )
            .size_full()
            .text_color(theme.foreground)
            .children((!crowded).then(|| self.render_sidebar(window, cx)))
            .child(self.render_main(window, cx))
            .children(settings.map(|bot| self.render_settings(&bot, width, cx)))
            .children(self.image_viewer.clone())
            .children(self.render_quote_prompt(window, cx))
            .children(self.render_quote_editor(cx))
            .into_any_element()
    }
}
