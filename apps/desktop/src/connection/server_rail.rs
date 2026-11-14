//! The server rail on the window's edge: one icon per saved server with a
//! short name under it, in the order the user drags them into.

use super::servers::{Editor, ReleaseStatus};
use super::*;
use crate::server_icon::{self, ServerIcon};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, h_flex};

const RAIL_WIDTH: f32 = 76.;

/// The hover card beside a rail icon. A right click dismisses it until the
/// pointer leaves, so it never covers the server's menu.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum RailCard {
    #[default]
    Hidden,
    Shown(Uuid),
    Dismissed(Uuid),
}

impl RailCard {
    /// The card after the pointer enters or leaves `id`'s icon. Leaving
    /// another icon changes nothing, and no card shows during a drag.
    fn hover(self, id: Uuid, hovered: bool, dragging: bool) -> Self {
        match self {
            _ if hovered && dragging => Self::Hidden,
            Self::Dismissed(current) if hovered && current == id => self,
            _ if hovered => Self::Shown(id),
            Self::Shown(current) | Self::Dismissed(current) if current == id => Self::Hidden,
            _ => self,
        }
    }

    /// A right click on `id` hides its card until the pointer leaves.
    fn dismiss(self, id: Uuid) -> Self {
        Self::Dismissed(id)
    }
}

const ICON_SIZE: f32 = 40.;
pub(super) const LABEL_WIDTH: f32 = 60.;

/// What a server's icon is drawn from.
#[derive(Clone)]
struct Face {
    name: String,
    icon: ServerIcon,
    device: bool,
}

impl Face {
    fn render(&self, cx: &App) -> AnyElement {
        server_icon::render(&self.icon, &self.name, self.device, px(ICON_SIZE), cx)
    }
}

/// A server being dragged to a new place in the rail.
#[derive(Clone)]
pub(super) struct ServerDrag {
    id: Uuid,
    face: Face,
}

impl Render for ServerDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .rounded(px(11.))
            .shadow_lg()
            .child(self.face.render(cx))
    }
}

/// The rail's fill: one step darker than the page, so it reads as the
/// window's edge rather than another panel.
pub(super) fn rail_background(theme: &gpui_kit::component::Theme) -> Hsla {
    let mut fill = theme.background;
    fill.l *= 0.82;
    fill
}

impl Shell {
    pub(super) fn render_server_rail(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let fill = rail_background(&theme);
        let order: Vec<Uuid> = self.registry.servers.iter().map(|s| s.id).collect();
        let mut list = v_flex()
            .id("server-rail")
            .tab_group()
            .role(Role::List)
            .aria_label("Servers")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(6.))
            .pt_2()
            .gap_1();
        for saved in &self.registry.servers {
            list = list.child(self.render_rail_item(saved, &order, fill, cx));
        }
        let add = v_flex()
            .id("server-rail-add")
            .role(Role::Button)
            .aria_label("Add a server")
            .flex_none()
            .items_center()
            .gap(px(5.))
            .mx(px(6.))
            .my_2()
            .py_2()
            .rounded(px(10.))
            .cursor_pointer()
            .tab_index(0)
            .text_color(theme.muted_foreground)
            .hover(|style| style.text_color(theme.foreground))
            .child(
                div()
                    .size(px(ICON_SIZE))
                    .rounded(px(11.))
                    .border_1()
                    .border_dashed()
                    .border_color(theme.border)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Plus).size_4()),
            )
            .child(div().text_size(px(9.5)).child("Add"))
            .on_click(cx.listener(|shell, _, window, cx| {
                shell.open_server_editor(Editor::Add, window, cx)
            }));
        v_flex()
            .flex_none()
            .w(px(RAIL_WIDTH))
            .h_full()
            .bg(fill)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(list)
            .child(add)
            .into_any_element()
    }

    fn render_rail_item(
        &self,
        saved: &crate::config::SavedServer,
        order: &[Uuid],
        fill: Hsla,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let id = saved.id;
        let name = saved.name.clone();
        let selected = self.active.as_ref().is_some_and(|active| active.id == id);
        let face = Face {
            name: name.clone(),
            icon: self.icons.get(id).clone(),
            device: self.managed_server(&saved.profile),
        };
        let (status, color) = self.server_status(id, cx);
        let update = self.server_release(id) == ReleaseStatus::Available;
        let host = saved.profile.origin.as_str().to_owned();
        let ring = if selected {
            theme.foreground.opacity(0.08).blend(fill)
        } else {
            fill
        };
        let badged = rail_face(&face, color, update, ring, cx);
        let detail = if update {
            format!("{status} · Update available")
        } else {
            status.to_owned()
        };
        let card = (self.rail_card == RailCard::Shown(id))
            .then(|| rail_card(name.clone(), color, detail.clone(), host, &theme));
        let drag = ServerDrag { id, face };
        let order = order.to_vec();
        let owner = cx.weak_entity();
        v_flex()
            .id(SharedString::from(format!("server-rail-{id}")))
            .relative()
            .role(Role::ListItem)
            .aria_label(format!("{name}. {detail}"))
            .items_center()
            .gap(px(5.))
            .pt_2()
            .pb(px(7.))
            .px(px(2.))
            .rounded(px(10.))
            .cursor_pointer()
            .tab_index(0)
            .map(|item| {
                if selected {
                    item.bg(theme.foreground.opacity(0.08))
                } else {
                    item.hover(|style| style.bg(theme.foreground.opacity(0.04)))
                }
            })
            .child(badged)
            .child(
                div()
                    .max_w(px(LABEL_WIDTH))
                    .truncate()
                    .text_size(px(9.5))
                    .line_height(px(11.))
                    .text_color(if selected {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .when(selected, |label| label.font_weight(FontWeight::SEMIBOLD))
                    .child(name),
            )
            .children(card)
            .on_hover(cx.listener(move |shell, hovered: &bool, _, cx| {
                let dragging = cx.has_active_drag();
                shell.move_rail_card(|card| card.hover(id, *hovered, dragging), cx)
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |shell, _, _, cx| {
                    shell.move_rail_card(|card| card.dismiss(id), cx)
                }),
            )
            .on_click(cx.listener(move |shell, _, window, cx| shell.select_server(id, window, cx)))
            .on_drag(drag, |drag, _, _, cx| cx.new(|_| drag.clone()))
            .drag_over::<ServerDrag>(move |style, drag, _, cx| {
                let style = style.border_color(cx.theme().drag_border);
                if crate::sidebar::lands_below(&order, drag.id, id) {
                    style.border_b_2()
                } else {
                    style.border_t_2()
                }
            })
            .on_drop(cx.listener(move |shell, drag: &ServerDrag, _, cx| {
                shell.reorder_server(drag.id, id, cx)
            }))
            .context_menu(move |menu, _, cx| Self::server_rail_menu(owner.clone(), id, menu, cx))
            .into_any_element()
    }

    fn server_rail_menu(
        owner: WeakEntity<Self>,
        id: Uuid,
        menu: PopupMenu,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let Some(shell) = owner.upgrade() else {
            return menu;
        };
        let (position, count, selected) = {
            let shell = shell.read(cx);
            (
                shell.registry.servers.iter().position(|s| s.id == id),
                shell.registry.servers.len(),
                shell.registry.selected == Some(id),
            )
        };
        let Some(position) = position else {
            return menu;
        };
        let reorder_owner = owner.clone();
        let neighbour = move |offset: isize| {
            let owner = reorder_owner.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let _ = owner.update(cx, |shell, cx| {
                    let target = position
                        .checked_add_signed(offset)
                        .and_then(|at| shell.registry.servers.get(at))
                        .map(|s| s.id);
                    if let Some(target) = target {
                        shell.reorder_server(id, target, cx);
                    }
                });
            }
        };
        let act = |run: fn(&mut Shell, Uuid, &mut Window, &mut Context<Shell>)| {
            let owner = owner.clone();
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = owner.update(cx, |shell, cx| run(shell, id, window, cx));
            }
        };
        let danger = if selected {
            cx.theme().muted_foreground
        } else {
            cx.theme().danger
        };
        menu.item(
            PopupMenuItem::new("Change icon…")
                .icon(IconName::Palette)
                .on_click(act(|shell, id, window, cx| {
                    shell.open_icon_editor(id, window, cx)
                })),
        )
        .item(
            PopupMenuItem::new("Edit connection…")
                .icon(IconName::PencilLine)
                .on_click(act(|shell, id, window, cx| {
                    shell.open_server_editor(Editor::Edit(id), window, cx)
                })),
        )
        .separator()
        .item(
            PopupMenuItem::new("Move up")
                .icon(IconName::ArrowUp)
                .disabled(position == 0)
                .on_click(neighbour(-1)),
        )
        .item(
            PopupMenuItem::new("Move down")
                .icon(IconName::ArrowDown)
                .disabled(position + 1 >= count)
                .on_click(neighbour(1)),
        )
        .separator()
        .item(
            PopupMenuItem::element(move |_, _| {
                div().text_color(danger).child("Remove connection…")
            })
            .icon(Icon::new(IconName::Trash).text_color(danger))
            .disabled(selected)
            .on_click(act(|shell, id, window, cx| {
                shell.remove_server(id, window, cx)
            })),
        )
    }

    fn move_rail_card(
        &mut self,
        change: impl FnOnce(RailCard) -> RailCard,
        cx: &mut Context<Self>,
    ) {
        let next = change(self.rail_card);
        if next != self.rail_card {
            self.rail_card = next;
            cx.notify();
        }
    }

    pub(super) fn reorder_server(&mut self, dragged: Uuid, target: Uuid, cx: &mut Context<Self>) {
        if dragged == target {
            return;
        }
        if let Err(err) = self.commit_registry(|registry| registry.reorder(dragged, target)) {
            self.connection_error = Some(err);
        }
        cx.notify();
    }
}

/// A server's icon with its status badge, and an update badge when one is
/// available. The badge's `ring` matches the fill behind it.
fn rail_face(face: &Face, color: Hsla, update: bool, ring: Hsla, cx: &App) -> Div {
    let theme = cx.theme();
    let hollow = color == theme.muted_foreground;
    let badge = div()
        .absolute()
        .right(px(-3.))
        .bottom(px(-3.))
        .size(px(14.))
        .rounded_full()
        .bg(ring)
        .flex()
        .items_center()
        .justify_center()
        .child(div().size(px(8.)).rounded_full().map(|dot| {
            if hollow {
                dot.border_2().border_color(color)
            } else {
                dot.bg(color)
            }
        }));
    div()
        .relative()
        .child(face.render(cx))
        .child(badge)
        .when(update, |face| {
            face.child(
                div()
                    .absolute()
                    .right(px(-4.))
                    .top(px(-4.))
                    .size(px(16.))
                    .rounded_full()
                    .bg(theme.primary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::ArrowUp)
                            .size(px(10.))
                            .text_color(theme.primary_foreground),
                    ),
            )
        })
}

/// The hover card beside a rail icon: the full name, status and address.
fn rail_card(
    name: String,
    color: Hsla,
    detail: String,
    host: String,
    theme: &gpui_kit::component::Theme,
) -> Div {
    let muted = theme.muted_foreground;
    div().absolute().left_full().top(px(10.)).child(deferred(
        anchored()
            .anchor(Anchor::TopLeft)
            .offset(point(px(10.), px(0.)))
            .snap_to_window_with_margin(px(8.))
            .child(
                v_flex()
                    .occlude()
                    .max_w(px(260.))
                    .gap_0p5()
                    .px_3()
                    .py_2()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .text_color(theme.popover_foreground)
                    .shadow_lg()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(name),
                    )
                    .child(
                        h_flex()
                            .gap(px(6.))
                            .items_center()
                            .text_xs()
                            .text_color(muted)
                            .child(div().size(px(6.)).rounded_full().bg(color))
                            .child(detail),
                    )
                    .child(div().text_xs().text_color(muted).child(host)),
            ),
    ))
}

#[cfg(test)]
mod tests {
    use super::RailCard;
    use uuid::Uuid;

    #[::core::prelude::v1::test]
    fn the_hover_card_follows_the_pointer_and_stays_away_from_menus_and_drags() {
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let card = RailCard::Hidden.hover(a, true, false);
        assert_eq!(card, RailCard::Shown(a));
        assert_eq!(
            card.hover(b, true, false),
            RailCard::Shown(b),
            "moving to another icon"
        );
        assert_eq!(
            card.hover(b, false, false),
            card,
            "leaving another icon is ignored"
        );
        let dismissed = card.dismiss(a);
        assert_eq!(dismissed, RailCard::Dismissed(a));
        assert_eq!(
            dismissed.hover(a, true, false),
            dismissed,
            "stays hidden over its icon"
        );
        assert_eq!(dismissed.hover(a, false, false), RailCard::Hidden);
        assert_eq!(
            card.hover(b, true, true),
            RailCard::Hidden,
            "no card during a drag"
        );
    }
}
