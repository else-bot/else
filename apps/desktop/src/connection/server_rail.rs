//! The server rail on the window's edge: one icon per saved server with a
//! short name under it, in the order the user drags them into.

use super::servers::{Editor, ReleaseStatus};
use super::*;
use crate::controls::{Control as _, Scale, Tier, text_field};
use crate::server_icon::{self, EMOJI, HUES, LetterStyle, ServerIcon};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, Selectable as _, h_flex};

const RAIL_WIDTH: f32 = 76.;

/// The hover card beside a rail icon. A right click dismisses it until the
/// pointer leaves, so it never covers the server's menu.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum RailCard {
    #[default]
    Hidden,
    Shown(Uuid),
    Dismissed(Uuid),
}
const ICON_SIZE: f32 = 40.;
const LABEL_WIDTH: f32 = 60.;

/// A server being dragged to a new place in the rail.
#[derive(Clone)]
pub(super) struct ServerDrag {
    id: Uuid,
    name: String,
    icon: ServerIcon,
    device: bool,
}

impl Render for ServerDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .rounded(px(11.))
            .shadow_lg()
            .child(server_icon::render(
                &self.icon,
                &self.name,
                self.device,
                px(ICON_SIZE),
                cx,
            ))
    }
}

/// The rail's fill: one step darker than the page, so it reads as the
/// window's edge rather than another panel.
fn rail_background(theme: &gpui_kit::component::Theme) -> Hsla {
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
        let device = self.managed_server(&saved.profile);
        let icon = self.icons.get(id).clone();
        let (status, color) = self.server_status(id, cx);
        let update = self.server_release(id) == ReleaseStatus::Available;
        let host = saved.profile.origin.as_str().to_owned();
        let hollow = color == theme.muted_foreground;
        let badge = div()
            .absolute()
            .right(px(-3.))
            .bottom(px(-3.))
            .size(px(14.))
            .rounded_full()
            .bg(if selected {
                theme.foreground.opacity(0.08).blend(fill)
            } else {
                fill
            })
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
        let face = div()
            .relative()
            .child(server_icon::render(&icon, &name, device, px(ICON_SIZE), cx))
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
            });
        let detail = if update {
            format!("{status} · Update available")
        } else {
            status.to_owned()
        };
        let card = (self.rail_card == RailCard::Shown(id)).then(|| {
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
                                    .child(name.clone()),
                            )
                            .child(
                                h_flex()
                                    .gap(px(6.))
                                    .items_center()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(div().size(px(6.)).rounded_full().bg(color))
                                    .child(detail.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(host),
                            ),
                    ),
            ))
        });
        let drag = ServerDrag {
            id,
            name: name.clone(),
            icon,
            device,
        };
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
            .child(face)
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
            .on_hover(
                cx.listener(move |shell, hovered: &bool, _, cx| shell.hover_rail(id, *hovered, cx)),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |shell, _, _, cx| {
                    shell.rail_card = RailCard::Dismissed(id);
                    cx.notify();
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

    fn hover_rail(&mut self, id: Uuid, hovered: bool, cx: &mut Context<Self>) {
        let next = match self.rail_card {
            _ if hovered && cx.has_active_drag() => RailCard::Hidden,
            RailCard::Dismissed(current) if hovered && current == id => return,
            _ if hovered => RailCard::Shown(id),
            RailCard::Shown(current) | RailCard::Dismissed(current) if current == id => {
                RailCard::Hidden
            }
            _ => return,
        };
        if next != self.rail_card {
            self.rail_card = next;
            cx.notify();
        }
    }

    pub(super) fn reorder_server(&mut self, dragged: Uuid, target: Uuid, cx: &mut Context<Self>) {
        if dragged == target {
            return;
        }
        let mut registry = self.registry.clone();
        let result = registry.reorder(dragged, target).and_then(|()| {
            self.launch
                .store
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|store| store.save(&registry))
        });
        match result {
            Ok(()) => self.registry = registry,
            Err(err) => self.connection_error = Some(err),
        }
        cx.notify();
    }

    fn save_server_icon(&mut self, id: Uuid, icon: ServerIcon) -> Result<(), String> {
        let device = self
            .registry
            .find(id)
            .is_some_and(|saved| self.managed_server(&saved.profile));
        let store = self.launch.store.as_ref().map_err(Clone::clone)?;
        self.icons.set(store, id, icon, device)
    }

    pub(super) fn open_icon_editor(
        &mut self,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(saved) = self.registry.find(id) else {
            return;
        };
        let device = self.managed_server(&saved.profile);
        let name = saved.name.clone();
        let draft = self.icons.get(id).clone();
        let shell = cx.weak_entity();
        let editor = cx.new(|cx| IconEditor::new(shell, id, name, device, draft, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .w(px(600.))
                .title("Server icon")
                .child(editor.clone())
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Generated,
    Emoji,
    Picture,
}

struct IconEditor {
    shell: WeakEntity<Shell>,
    id: Uuid,
    name: String,
    device: bool,
    draft: ServerIcon,
    tab: Tab,
    letters: Entity<InputState>,
    error: Option<String>,
    _letters: Subscription,
    _pick: Option<Task<()>>,
}

impl IconEditor {
    fn new(
        shell: WeakEntity<Shell>,
        id: Uuid,
        name: String,
        device: bool,
        draft: ServerIcon,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let chosen = match &draft {
            ServerIcon::Letters {
                letters: Some(letters),
                ..
            } => letters.clone(),
            _ => String::new(),
        };
        let placeholder = server_icon::initials(&name);
        let letters = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(chosen)
        });
        let subscription = cx.subscribe_in(&letters, window, |editor, input, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                let text = input.read(cx).value().to_string();
                editor.set_letters(&text);
                cx.notify();
            }
        });
        let tab = match draft {
            ServerIcon::Emoji { .. } => Tab::Emoji,
            ServerIcon::Picture(_) => Tab::Picture,
            _ => Tab::Generated,
        };
        Self {
            shell,
            id,
            name,
            device,
            draft,
            tab,
            letters,
            error: None,
            _letters: subscription,
            _pick: None,
        }
    }

    fn typed_letters(&self, cx: &App) -> Option<String> {
        server_icon::letters(&self.letters.read(cx).value())
    }

    fn set_letters(&mut self, text: &str) {
        self._pick = None;
        let editing = matches!(self.draft, ServerIcon::Letters { .. });
        if text.trim().is_empty() && !editing {
            self.error = None;
            return;
        }
        let letters = server_icon::letters(text);
        self.error = (letters.is_none() && !text.trim().is_empty())
            .then(|| "Use one or two characters.".to_owned());
        let style = match &self.draft {
            ServerIcon::Letters { style, .. } => *style,
            _ => LetterStyle::Gradient,
        };
        self.draft = ServerIcon::Letters {
            style,
            letters,
            hue: self.draft.hue(),
        };
    }

    /// Tint the draft, keeping it in the open tab's kind of icon.
    fn set_hue(&mut self, hue: Option<u16>, cx: &mut Context<Self>) {
        self._pick = None;
        self.draft = match (self.tab, self.draft.clone()) {
            (Tab::Emoji, icon @ ServerIcon::Emoji { .. }) => icon.with_hue(hue),
            (Tab::Emoji, _) => ServerIcon::Emoji {
                emoji: EMOJI[0].to_owned(),
                hue,
            },
            (_, ServerIcon::Emoji { .. }) => ServerIcon::Letters {
                style: LetterStyle::Gradient,
                letters: self.typed_letters(cx),
                hue,
            },
            (_, icon) => icon.with_hue(hue),
        };
        cx.notify();
    }

    fn pick_picture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose".into()),
        });
        self._pick = Some(cx.spawn_in(window, async move |editor, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let loaded = cx
                .background_executor()
                .spawn(async move {
                    let meta = std::fs::metadata(&path).map_err(|err| err.to_string())?;
                    if meta.len() > server_icon::MAX_SOURCE_BYTES {
                        return Err("Choose an image smaller than 32 MB.".to_owned());
                    }
                    let bytes = std::fs::read(&path).map_err(|err| err.to_string())?;
                    server_icon::square_picture(&bytes)
                        .map_err(|err| format!("This file is not an image the app can read. {err}"))
                })
                .await;
            let _ = editor.update(cx, |editor, cx| {
                match loaded {
                    Ok(image) => {
                        editor.draft = ServerIcon::Picture(image);
                        editor.error = None;
                    }
                    Err(err) => editor.error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let typed = self.letters.read(cx).value().to_string();
        if matches!(self.draft, ServerIcon::Letters { .. })
            && !typed.trim().is_empty()
            && server_icon::letters(&typed).is_none()
        {
            self.error = Some("Use one or two characters.".to_owned());
            cx.notify();
            return;
        }
        let (id, draft) = (self.id, self.draft.clone());
        let saved = self
            .shell
            .update(cx, |shell, cx| {
                let saved = shell.save_server_icon(id, draft);
                cx.notify();
                saved
            })
            .unwrap_or(Ok(()));
        match saved {
            Ok(()) => window.close_dialog(cx),
            Err(err) => {
                self.error = Some(err);
                cx.notify();
            }
        }
    }

    fn icon(&self, icon: &ServerIcon, size: f32, cx: &App) -> AnyElement {
        server_icon::render(icon, &self.name, self.device, px(size), cx)
    }

    fn render_generated(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let hue = self.draft.hue();
        let letters = self.typed_letters(cx);
        let mut styles = vec![
            (
                "Gradient",
                ServerIcon::Letters {
                    style: LetterStyle::Gradient,
                    letters: letters.clone(),
                    hue,
                },
            ),
            (
                "Monogram",
                ServerIcon::Letters {
                    style: LetterStyle::Monogram,
                    letters,
                    hue,
                },
            ),
            ("Pattern", ServerIcon::Pattern { hue }),
        ];
        if self.device {
            styles.insert(0, ("Laptop", ServerIcon::Default));
        }
        let kind = |icon: &ServerIcon| match icon {
            ServerIcon::Default if !self.device => Some(1),
            ServerIcon::Default => Some(0),
            ServerIcon::Letters {
                style: LetterStyle::Gradient,
                ..
            } => Some(1),
            ServerIcon::Letters {
                style: LetterStyle::Monogram,
                ..
            } => Some(2),
            ServerIcon::Pattern { .. } => Some(3),
            _ => None,
        };
        let current = kind(&self.draft);
        let row = h_flex()
            .gap_2()
            .children(styles.into_iter().enumerate().map(|(i, (label, icon))| {
                let on = kind(&icon) == current;
                v_flex()
                    .id(("icon-style", i))
                    .role(Role::RadioButton)
                    .aria_label(label)
                    .items_center()
                    .gap_1p5()
                    .w(px(76.))
                    .py_2()
                    .rounded(px(10.))
                    .border_1()
                    .cursor_pointer()
                    .tab_index(0)
                    .border_color(if on {
                        theme.primary
                    } else {
                        gpui_kit::transparent_black()
                    })
                    .when(!on, |b| b.hover(|s| s.bg(theme.foreground.opacity(0.04))))
                    .text_xs()
                    .text_color(if on {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .child(self.icon(&icon, 40., cx))
                    .child(label)
                    .on_click(cx.listener(move |editor, _, _, cx| {
                        editor._pick = None;
                        editor.draft = icon.clone();
                        cx.notify();
                    }))
            }));
        let swatches = self.render_swatches(0.65, 0.52, cx);
        let field_label = |text: &'static str| {
            div()
                .w(px(56.))
                .flex_none()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        v_flex()
            .gap_3()
            .child(row)
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(field_label("Colour").pt_1())
                    .child(swatches),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(field_label("Letters"))
                    .child(div().w(px(84.)).child(
                        text_field(Input::new(&self.letters), cx).aria_label("Icon letters"),
                    ))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Blank uses the name's initials"),
                    ),
            )
            .into_any_element()
    }

    /// The colour choices: the name's own hue first, then [`HUES`], drawn
    /// at the saturation and lightness the tab's icons use.
    fn render_swatches(&self, saturation: f32, lightness: f32, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let hue = self.draft.hue();
        let fill = |h: u16| hsla(f32::from(h) / 360., saturation, lightness, 1.);
        let swatch = |id: usize, hue: Option<u16>, fill: Hsla, on: bool| {
            div()
                .id(("icon-hue", id))
                .role(Role::RadioButton)
                .aria_label(
                    hue.map_or_else(|| "Colour from the name".to_owned(), |h| format!("Hue {h}")),
                )
                .size(px(22.))
                .rounded_full()
                .cursor_pointer()
                .tab_index(0)
                .border_2()
                .border_color(if on {
                    theme.foreground
                } else {
                    gpui_kit::transparent_black()
                })
                .bg(fill)
                .on_click(cx.listener(move |editor, _, _, cx| editor.set_hue(hue, cx)))
        };
        h_flex()
            .flex_wrap()
            .gap_1p5()
            .child(swatch(
                0,
                None,
                fill(server_icon::name_hue(&self.name)),
                hue.is_none(),
            ))
            .children(
                HUES.iter()
                    .enumerate()
                    .map(|(i, &h)| swatch(i + 1, Some(h), fill(h), hue == Some(h))),
            )
    }

    fn render_emoji(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let hue = self.draft.hue();
        let current = match &self.draft {
            ServerIcon::Emoji { emoji, .. } => Some(emoji.clone()),
            _ => None,
        };
        let mut grid = v_flex().gap_1();
        for (row, chunk) in EMOJI.chunks(8).enumerate() {
            grid = grid.child(h_flex().gap_1().children(chunk.iter().enumerate().map(
                |(col, emoji)| {
                    let on = current.as_deref() == Some(*emoji);
                    let emoji = (*emoji).to_owned();
                    div()
                        .id(("icon-emoji", row * 8 + col))
                        .role(Role::RadioButton)
                        .aria_label(emoji.clone())
                        .size(px(38.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(8.))
                        .cursor_pointer()
                        .tab_index(0)
                        .text_size(px(20.))
                        .map(|cell| {
                            if on {
                                cell.bg(theme.primary.opacity(0.2))
                            } else {
                                cell.hover(|s| s.bg(theme.foreground.opacity(0.06)))
                            }
                        })
                        .child(emoji.clone())
                        .on_click(cx.listener(move |editor, _, _, cx| {
                            editor._pick = None;
                            editor.draft = ServerIcon::Emoji {
                                emoji: emoji.clone(),
                                hue,
                            };
                            cx.notify();
                        }))
                },
            )));
        }
        v_flex()
            .gap_3()
            .child(grid)
            .child(
                h_flex()
                    .items_start()
                    .gap_2()
                    .child(
                        div()
                            .w(px(56.))
                            .flex_none()
                            .pt_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Backdrop"),
                    )
                    .child(self.render_swatches(0.30, 0.30, cx)),
            )
            .into_any_element()
    }

    fn render_picture(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        v_flex()
            .id("icon-picture")
            .role(Role::Button)
            .aria_label("Choose an image")
            .items_center()
            .justify_center()
            .gap_2()
            .h(px(180.))
            .rounded(px(12.))
            .border_1()
            .border_dashed()
            .border_color(theme.border)
            .cursor_pointer()
            .tab_index(0)
            .text_color(theme.muted_foreground)
            .hover(|s| s.border_color(theme.primary).text_color(theme.foreground))
            .child(Icon::new(IconName::ImagePlus).size_6())
            .child(div().text_sm().child("Choose an image…"))
            .child(
                div()
                    .text_xs()
                    .child("PNG, JPEG, GIF or WebP. Cropped to a square."),
            )
            .on_click(cx.listener(|editor, _, window, cx| editor.pick_picture(window, cx)))
            .into_any_element()
    }
}

impl Render for IconEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let fill = rail_background(&theme);
        let preview = v_flex()
            .flex_none()
            .w(px(176.))
            .items_center()
            .gap_4()
            .py_5()
            .rounded(px(12.))
            .bg(fill)
            .child(self.icon(&self.draft, 96., cx))
            .child(
                h_flex()
                    .items_end()
                    .gap_3()
                    .child(self.icon(&self.draft, 40., cx))
                    .child(self.icon(&self.draft, 28., cx))
                    .child(self.icon(&self.draft, 20., cx)),
            )
            .child(
                div()
                    .max_w(px(LABEL_WIDTH * 2.))
                    .truncate()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(self.name.clone()),
            );
        let tab = |id: &'static str, label: &'static str, which: Tab, cx: &mut Context<Self>| {
            Button::new(id)
                .control(Tier::Ghost, Scale::Compact, false, cx)
                .flex_1()
                .selected(self.tab == which)
                .label(label)
                .on_click(cx.listener(move |editor, _, _, cx| {
                    editor.tab = which;
                    cx.notify();
                }))
        };
        let tabs = h_flex()
            .gap_1()
            .p(px(3.))
            .rounded(px(9.))
            .bg(theme.foreground.opacity(0.04))
            .child(tab("icon-tab-generated", "Generated", Tab::Generated, cx))
            .child(tab("icon-tab-emoji", "Emoji", Tab::Emoji, cx))
            .child(tab("icon-tab-picture", "Image", Tab::Picture, cx));
        let pane = match self.tab {
            Tab::Generated => self.render_generated(cx),
            Tab::Emoji => self.render_emoji(cx),
            Tab::Picture => self.render_picture(cx),
        };
        let footer = h_flex()
            .gap_2()
            .pt_2()
            .child(
                Button::new("icon-default")
                    .control(Tier::Ghost, Scale::Standard, false, cx)
                    .icon(IconName::Undo2)
                    .label("Use generated icon")
                    .on_click(cx.listener(|editor, _, window, cx| {
                        editor
                            .letters
                            .update(cx, |input, cx| input.set_value("", window, cx));
                        editor._pick = None;
                        editor.draft = ServerIcon::Default;
                        editor.error = None;
                        editor.tab = Tab::Generated;
                        cx.notify();
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("icon-cancel")
                    .control(Tier::Ghost, Scale::Standard, false, cx)
                    .label("Cancel")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("icon-save")
                    .control(Tier::Primary, Scale::Standard, false, cx)
                    .label("Save")
                    .on_click(cx.listener(|editor, _, window, cx| editor.save(window, cx))),
            );
        v_flex()
            .id("server-icon-editor")
            .tab_group()
            .gap_4()
            .child(
                h_flex()
                    .items_start()
                    .gap_5()
                    .child(preview)
                    .child(v_flex().flex_1().min_w_0().gap_3().child(tabs).child(pane)),
            )
            .when_some(self.error.clone(), |body, error| {
                body.child(div().text_xs().text_color(theme.danger).child(error))
            })
            .child(footer)
    }
}
