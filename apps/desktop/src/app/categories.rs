//! Category forms, assignment, and move dialogs.

use super::delete_dialog::DeleteTarget;
use super::{Connection, ElseApp};
use crate::api::{Bot, Category};
use crate::sidebar::known_category;
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::radio::RadioGroup;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

mod creation;
mod form;

pub(super) use form::CategoryFormHolder;

/// The Move dialog's pending choice for one bot.
struct MoveHolder {
    bot: Uuid,
    current: Option<Uuid>,
    choice: Option<Uuid>,
}

impl ElseApp {
    // ----- Categories (POC-031) ---------------------------------------------

    pub(super) fn movable_bot(&self, bot: Uuid) -> Option<&Bot> {
        if self.connection != Connection::Connected || self.moving.contains(&bot) {
            return None;
        }
        self.bots.iter().find(|b| b.id == bot && b.is_active())
    }

    pub(super) fn can_assign_category(&self, bot: Uuid, category: Option<Uuid>) -> bool {
        self.movable_bot(bot).is_some_and(|b| {
            category.is_none_or(|id| self.categories.iter().any(|c| c.id == id))
                && known_category(&self.categories, b.category_id) != category
        })
    }

    /// Assign (or with `None` unassign) a bot. One move per bot at a time.
    /// The list itself changes only from the server's `bots` snapshots (the
    /// one ordered source), never from this response or an extra fetch.
    pub(super) fn assign_category(
        &mut self,
        bot: Uuid,
        category: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.can_assign_category(bot, category) {
            return;
        }
        let client = self.client.clone();
        self.moving.insert(bot);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.set_category(bot, category).await;
            let _ = this.update_in(cx, |app, _, cx| {
                app.moving.remove(&bot);
                match result {
                    Ok(_) => app.category_notice = None,
                    Err(err) => app.category_notice = Some(format!("Moving the bot failed: {err}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Rename and Delete for a category's right-click menu.
    pub(super) fn category_menu(
        owner: WeakEntity<Self>,
        category: &Category,
        menu: PopupMenu,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let editable = owner
            .upgrade()
            .is_some_and(|app| app.read(cx).connection == Connection::Connected);
        let danger = if editable {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };
        let (rename, delete) = (category.clone(), category.clone());
        let rename_owner = owner.clone();
        menu.item(
            PopupMenuItem::new("Rename category…")
                .icon(IconName::PencilLine)
                .disabled(!editable)
                .on_click(move |_, window, cx| {
                    let _ = rename_owner.update(cx, |app, cx| {
                        app.open_category_dialog(Some(rename.clone()), window, cx)
                    });
                }),
        )
        .separator()
        .item(
            PopupMenuItem::element(move |_, _| div().text_color(danger).child("Delete category…"))
                .icon(Icon::new(IconName::Trash).text_color(danger))
                .disabled(!editable)
                .on_click(move |_, window, cx| {
                    let _ = owner.update(cx, |app, cx| {
                        app.open_delete_category_dialog(delete.clone(), window, cx)
                    });
                }),
        )
    }

    pub(super) fn open_delete_category_dialog(
        &mut self,
        category: Category,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let bots = self
            .bots
            .iter()
            .filter(|b| b.is_active() && b.category_id == Some(category.id))
            .count();
        self.open_delete_confirmation(
            DeleteTarget::Category {
                name: category.name,
                bots,
            },
            window,
            cx,
            move |app, window, cx| app.delete_category(category.id, window, cx),
        );
    }

    fn delete_category(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.delete_category(id).await;
            let _ = this.update_in(cx, |app, _, cx| {
                // The `bots` snapshot shows the result; a failure is shown
                // in the sidebar, where it is visible with or without a
                // selected bot.
                app.category_notice = match result {
                    Ok(()) => None,
                    Err(err) => Some(format!("Deleting the category failed: {err}")),
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Move the selected bot: a radio group of every category and
    /// Unassigned (Tab and Space, or a click, choose; Enter or Move applies).
    pub(super) fn open_move_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(bot) = self.selected.and_then(|id| self.movable_bot(id)).cloned() else {
            return;
        };
        let categories = self.categories.clone();
        let current = known_category(&categories, bot.category_id);
        let holder = cx.new(|_| MoveHolder {
            bot: bot.id,
            current,
            choice: current,
        });
        let app = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let options: Vec<(Option<Uuid>, String)> = categories
                .iter()
                .map(|c| (Some(c.id), c.name.clone()))
                .chain(std::iter::once((None, "Unassigned".to_owned())))
                .collect();
            let state = holder.read(cx);
            let selected = options.iter().position(|(id, _)| *id == state.choice);
            let unchanged = state.choice == state.current;
            let ids: Vec<Option<Uuid>> = options.iter().map(|(id, _)| *id).collect();
            let choose = holder.clone();
            let group = RadioGroup::vertical("move-choice")
                .cursor_pointer()
                .children(options.iter().map(|(id, label)| {
                    if *id == state.current {
                        format!("{label} (current)")
                    } else {
                        label.clone()
                    }
                }))
                .selected_index(selected)
                .on_change(move |ix, window, cx| {
                    let pick = ids[*ix];
                    choose.update(cx, |h, cx| {
                        h.choice = pick;
                        cx.notify();
                    });
                    window.refresh();
                });
            let apply = {
                let app = app.clone();
                let holder = holder.clone();
                move |window: &mut Window, cx: &mut App| {
                    let (bot, choice, current) = {
                        let h = holder.read(cx);
                        (h.bot, h.choice, h.current)
                    };
                    if choice != current {
                        let _ =
                            app.update(cx, |app, cx| app.assign_category(bot, choice, window, cx));
                    }
                }
            };
            let apply_ok = apply.clone();
            dialog
                .title(format!("Move {}", bot.name))
                .w(px(380.))
                .on_ok(move |_, window, cx| {
                    apply_ok(window, cx);
                    true
                })
                .child(
                    v_flex()
                        .gap_2()
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(
                                    "Only its place in the sidebar changes. Tab to a category, \
                                     Space to choose, Enter moves, Esc closes.",
                                ),
                        )
                        .child(group),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("move-cancel")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Cancel")
                                .ghost()
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("move-apply")
                                .focus_ring(false)
                                .cursor_pointer()
                                .label("Move")
                                .primary()
                                .disabled(unchanged)
                                .on_click(move |_, window, cx| {
                                    apply(window, cx);
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }
}
