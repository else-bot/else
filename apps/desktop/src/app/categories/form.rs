mod view;

use std::collections::HashSet;

use super::creation::Creation;
use super::{Connection, ElseApp};
use crate::api::Category;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Sizable as _, WindowExt as _,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use uuid::Uuid;

#[derive(Clone)]
enum Operation {
    Rename(Uuid),
    Create(Creation),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BotFilter {
    All,
    Unassigned,
}

pub(in crate::app) struct CategoryFormHolder {
    operation: Operation,
    name: Entity<InputState>,
    search: Entity<InputState>,
    selected: HashSet<Uuid>,
    filter: BotFilter,
    error: Option<String>,
    busy: bool,
    _search: Subscription,
}

impl CategoryFormHolder {
    fn creating(&self) -> bool {
        matches!(self.operation, Operation::Create(_))
    }

    fn locked(&self) -> bool {
        self.busy || matches!(&self.operation, Operation::Create(create) if create.locked())
    }

    fn action(&self) -> &'static str {
        match &self.operation {
            Operation::Rename(_) => "Rename",
            Operation::Create(create) if create.adding_bots() => "Retry adding bots",
            Operation::Create(create) if create.locked() => "Retry create",
            Operation::Create(_) => "Create category",
        }
    }
}

impl ElseApp {
    pub(in crate::app) fn open_category_dialog(
        &mut self,
        renaming: Option<Category>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = cx.new(|cx| {
            let mut input = InputState::new(window, cx).placeholder("e.g. Everyday");
            if let Some(category) = &renaming {
                input.set_value(category.name.clone(), window, cx);
            }
            input
        });
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search bots…"));
        let search_events =
            cx.subscribe_in(&search, window, |_, _, event: &InputEvent, window, _| {
                if matches!(event, InputEvent::Change) {
                    window.refresh();
                }
            });
        let holder = cx.new(|_| CategoryFormHolder {
            operation: renaming
                .map(|category| Operation::Rename(category.id))
                .unwrap_or_else(|| Operation::Create(Creation::new())),
            name: name.clone(),
            search,
            selected: HashSet::new(),
            filter: BotFilter::All,
            error: None,
            busy: false,
            _search: search_events,
        });
        self.category_form = Some(holder.clone());
        self._category_enter = Some(cx.subscribe_in(
            &name,
            window,
            |app, _, event: &InputEvent, window, cx| match event {
                InputEvent::PressEnter { .. } => app.submit_category(window, cx),
                InputEvent::Change => window.refresh(),
                _ => {}
            },
        ));
        let app = cx.entity().downgrade();
        let focus_once = std::cell::Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if focus_once.replace(false) {
                name.read(cx).focus_handle(cx).focus(window, cx);
            }
            let Some(owner) = app.upgrade() else {
                return dialog;
            };
            let state = owner.read(cx);
            let form = holder.read(cx);
            let theme = cx.theme();
            let creating = form.creating();
            let connected = state.connection == Connection::Connected;
            let stacked = window.viewport_size().width < px(760.);
            let selected = state
                .active_bots()
                .filter(|bot| form.selected.contains(&bot.id))
                .count();
            let mut body = if creating {
                form.render_create(&holder, state, stacked, cx)
            } else {
                v_flex()
                    .gap_2()
                    .child(div().text_xs().child("Category name"))
                    .child(
                        Input::new(&form.name)
                            .focus_ring(false)
                            .aria_label("Category name")
                            .disabled(form.locked()),
                    )
                    .into_any_element()
            };
            if let Some(error) = &form.error {
                body = v_flex()
                    .gap_3()
                    .child(body)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.danger)
                            .child(error.clone()),
                    )
                    .into_any_element();
            }
            let submit = app.clone();
            let ok = app.clone();
            let close = app.clone();
            let mine = holder.clone();
            let submit_name = name.clone();
            let busy = form.busy;
            let can_submit = connected && !form.name.read(cx).value().trim().is_empty();
            let title = if creating {
                "New category"
            } else {
                "Rename category"
            };
            let footer = h_flex()
                .justify_end()
                .gap_2()
                .when(creating, |footer| {
                    footer
                        .mx(-px(24.))
                        .px_6()
                        .py_3p5()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            h_flex()
                                .flex_1()
                                .gap_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(
                                    Icon::new(IconName::Users)
                                        .size_3p5()
                                        .text_color(theme.primary),
                                )
                                .child(match selected {
                                    0 => "No bots selected".to_owned(),
                                    1 => "1 bot selected".to_owned(),
                                    n => format!("{n} bots selected"),
                                }),
                        )
                })
                .child(
                    Button::new("category-cancel")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label("Cancel")
                        .ghost()
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    Button::new("category-submit")
                        .focus_ring(false)
                        .cursor_pointer()
                        .label(form.action())
                        .primary()
                        .loading(busy)
                        .disabled(busy || !can_submit)
                        .on_click(move |_, window, cx| {
                            let _ = submit.update(cx, |app, cx| app.submit_category(window, cx));
                        }),
                );
            dialog
                .w(if creating { px(704.) } else { px(420.) })
                .p_6()
                .when(creating, |dialog| dialog.pb_0())
                .gap_5()
                .close_button(false)
                .title(
                    h_flex()
                        .gap_3()
                        .items_start()
                        .when(creating, |header| {
                            header.child(
                                div()
                                    .size_9()
                                    .flex_none()
                                    .rounded(theme.radius)
                                    .bg(theme.primary.opacity(0.1))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        Icon::new(IconName::Folder)
                                            .size_5()
                                            .text_color(theme.primary),
                                    ),
                            )
                        })
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_1()
                                .child(div().text_lg().child(title))
                                .when(creating, |heading| {
                                    heading.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child("Give your bots a place to belong."),
                                    )
                                }),
                        )
                        .child(
                            Button::new("category-close")
                                .small()
                                .ghost()
                                .focus_ring(false)
                                .cursor_pointer()
                                .icon(IconName::Close)
                                .accessibility_label("Close")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    if submit_name.read(cx).focus_handle(cx).is_focused(window) {
                        let _ = ok.update(cx, |app, cx| app.submit_category(window, cx));
                    }
                    false
                })
                .on_close(move |_, _, cx| {
                    let _ = close.update(cx, |app, _| {
                        if app.category_form.as_ref() == Some(&mine) {
                            app.category_form = None;
                        }
                    });
                })
                .child(body)
                .footer(footer)
        });
    }

    fn submit_category(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(holder) = self.category_form.clone() else {
            return;
        };
        let form = holder.read(cx);
        if form.busy || self.connection != Connection::Connected {
            return;
        }
        let name = form.name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            return;
        }
        let mut operation = form.operation.clone();
        let draft_ids: Vec<_> = self
            .active_bots()
            .filter(|bot| form.selected.contains(&bot.id))
            .map(|bot| bot.id)
            .collect();
        let bot_ids = match &operation {
            Operation::Rename(_) => Vec::new(),
            Operation::Create(create) => create.bot_ids(&draft_ids),
        };
        if bot_ids.iter().any(|id| self.moving.contains(id)) {
            holder.update(cx, |form, _| {
                form.error =
                    Some("A selected bot is already moving. Try again when it finishes.".to_owned())
            });
            window.refresh();
            return;
        }
        self.moving.extend(bot_ids.iter().copied());
        holder.update(cx, |form, _| {
            form.busy = true;
            form.error = None;
        });
        cx.notify();
        window.refresh();
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = match &mut operation {
                Operation::Rename(id) => client
                    .rename_category(*id, name)
                    .await
                    .map_err(|err| err.to_string()),
                Operation::Create(create) => create.save(client, name, bot_ids.clone()).await,
            };
            let _ = this.update_in(cx, |app, window, cx| {
                for bot in bot_ids {
                    app.moving.remove(&bot);
                }
                let current = app.category_form.as_ref() == Some(&holder);
                match result {
                    Ok(_) if current => {
                        app.category_form = None;
                        app.category_notice = None;
                        window.close_dialog(cx);
                    }
                    Err(error) => {
                        if current {
                            holder.update(cx, |form, _| {
                                form.operation = operation;
                                form.busy = false;
                                form.error = Some(error);
                            });
                            window.refresh();
                        } else {
                            app.category_notice = Some(error);
                        }
                    }
                    Ok(_) => {}
                }
                cx.notify();
            });
        })
        .detach();
    }
}
