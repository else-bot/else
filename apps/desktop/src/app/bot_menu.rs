use super::{Connection, ElseApp};
use crate::inbox::Mark;
use crate::sidebar::{fold, known_category};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon};
use gpui_kit::*;
use uuid::Uuid;

impl ElseApp {
    pub(super) fn bot_menu(
        owner: WeakEntity<Self>,
        id: Uuid,
        menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(app) = owner.upgrade() else {
            return menu;
        };
        let app = app.read(cx);
        let Some(bot) = app.active_bots().find(|b| b.id == id) else {
            return menu;
        };
        let current = known_category(&app.categories, bot.category_id);
        let mut categories: Vec<(Uuid, String)> = app
            .categories
            .iter()
            .map(|c| (c.id, c.name.clone()))
            .collect();
        categories.sort_by_key(|(id, name)| (fold(name), *id));
        let movable = !categories.is_empty() && app.movable_bot(id).is_some();
        let unread = app.is_unread(id);
        let muted = bot.muted;
        let silenced = bot.silenced();
        let connected = app.connection == Connection::Connected;
        let deletable = app.default_bot_id != Some(id) && connected && !app.deleting.contains(&id);
        let delete_color = if deletable {
            cx.theme().danger
        } else {
            cx.theme().muted_foreground
        };

        let act = |f: fn(&mut Self, Uuid, &mut Window, &mut Context<Self>)| {
            let owner = owner.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = owner.update(cx, |app, cx| f(app, id, window, cx));
            }
        };
        let destination = {
            let owner = owner.clone();
            move |submenu: PopupMenu, label: String, category: Option<Uuid>| {
                let owner = owner.clone();
                submenu.item(
                    PopupMenuItem::new(label)
                        .checked(category == current)
                        .disabled(category == current)
                        .on_click(move |_, window, cx| {
                            let _ = owner.update(cx, |app, cx| {
                                app.assign_category(id, category, window, cx)
                            });
                        }),
                )
            }
        };

        let menu = if movable {
            menu.submenu_with_icon(
                Some(Icon::new(IconName::FolderInput)),
                "Move to",
                window,
                cx,
                move |submenu, _, _| {
                    let submenu = categories
                        .iter()
                        .fold(submenu, |submenu, (category, name)| {
                            destination(submenu, name.clone(), Some(*category))
                        });
                    destination(submenu.separator(), "Unassigned".into(), None)
                },
            )
        } else {
            menu.item(
                PopupMenuItem::new("Move to")
                    .icon(IconName::FolderInput)
                    .disabled(true),
            )
        };
        let menu = if silenced {
            menu
        } else {
            menu.item(
                PopupMenuItem::new(if unread {
                    "Mark as read"
                } else {
                    "Mark as unread"
                })
                .icon(IconName::BellDot)
                .on_click(act(Self::toggle_unread)),
            )
        };
        menu.item(
            PopupMenuItem::new(if muted { "Unmute bot" } else { "Mute bot" })
                .icon(if muted {
                    IconName::Bell
                } else {
                    IconName::BellOff
                })
                .disabled(!connected)
                .on_click(act(Self::toggle_mute)),
        )
        .item(
            PopupMenuItem::new("Rename bot")
                .icon(IconName::PencilLine)
                .on_click(act(Self::rename_bot)),
        )
        .separator()
        .item(
            PopupMenuItem::element(move |_, _| div().text_color(delete_color).child("Delete bot…"))
                .icon(Icon::new(IconName::Trash).text_color(delete_color))
                .disabled(!deletable)
                .on_click(act(Self::open_delete_dialog)),
        )
    }

    fn toggle_unread(&mut self, id: Uuid, _: &mut Window, cx: &mut Context<Self>) {
        let mark = if self.is_unread(id) {
            Mark::Read
        } else {
            Mark::Unread
        };
        self.mark_bot(id, mark, cx);
    }

    fn toggle_mute(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(muted) = self.active_bots().find(|b| b.id == id).map(|b| !b.muted) else {
            return;
        };
        let client = self.client.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.set_muted(id, muted).await;
            let _ = this.update_in(cx, |app, window, cx| {
                if let Err(err) = result {
                    app.notice = Some(format!("Could not save the mute setting: {err}"));
                }
                app.refresh_now(window, cx);
            });
        })
        .detach();
    }
}
