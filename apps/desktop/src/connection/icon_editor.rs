//! The dialog that changes a server's icon in the rail: generated letters
//! or a pattern in a chosen colour, an emoji, or a picture.

use super::server_rail::{LABEL_WIDTH, rail_background};
use super::*;
use crate::controls::{Control as _, Scale, Tier, text_field};
use crate::server_icon::{self, EMOJI, HUES, LetterStyle, ServerIcon};
use gpui_kit::assets::IconName;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::Input;
use gpui_kit::component::{Icon, Selectable as _, h_flex};

impl Shell {
    fn save_server_icon(&mut self, id: Uuid, icon: ServerIcon) -> Result<(), String> {
        let store = self.launch.store.as_ref().map_err(Clone::clone)?;
        self.icons.set(store, id, icon)
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
        let icon = self.icons.get(id).clone();
        let shell = cx.weak_entity();
        let editor = cx.new(|cx| IconEditor::new(shell, id, name, device, &icon, window, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog
                .w(px(600.))
                .title("Server icon")
                .child(editor.clone())
        });
    }
}

/// The draft's kind of icon. Its colour lives beside it in [`Draft`] and
/// its letters in the editor's field, so switching looks keeps both.
#[derive(Clone, PartialEq)]
enum Look {
    /// This computer's default.
    Laptop,
    Letters(LetterStyle),
    Pattern,
    Emoji(String),
    Picture(Arc<Image>),
}

impl Look {
    fn takes_hue(&self) -> bool {
        matches!(self, Self::Letters(_) | Self::Pattern | Self::Emoji(_))
    }
}

#[derive(Clone, PartialEq)]
struct Draft {
    look: Look,
    hue: Option<u16>,
}

impl Draft {
    fn generated(device: bool) -> Self {
        Self {
            look: if device {
                Look::Laptop
            } else {
                Look::Letters(LetterStyle::Gradient)
            },
            hue: None,
        }
    }

    /// The draft for a saved icon, and the letters it overrides.
    fn of(icon: &ServerIcon, device: bool) -> (Self, Option<String>) {
        let draft = |look, hue| Self { look, hue };
        match icon.clone() {
            ServerIcon::Default => (Self::generated(device), None),
            ServerIcon::Letters {
                style,
                letters,
                hue,
            } => (draft(Look::Letters(style), hue), letters),
            ServerIcon::Pattern { hue } => (draft(Look::Pattern, hue), None),
            ServerIcon::Emoji { emoji, hue } => (draft(Look::Emoji(emoji), hue), None),
            ServerIcon::Picture(image) => (draft(Look::Picture(image), None), None),
        }
    }

    /// The draft in `hue`, switched to `tab`'s own look when its current
    /// look would not show the colour there.
    fn tinted(self, hue: Option<u16>, tab: Tab) -> Self {
        let look = if Tab::of(&self.look) != tab || !self.look.takes_hue() {
            tab.tinted()
        } else {
            self.look
        };
        Self { look, hue }
    }

    /// The icon to save. Plain generated letters stay the default, so the
    /// icon keeps following the server's name.
    fn icon(&self, letters: Option<String>, device: bool) -> ServerIcon {
        let hue = self.hue;
        match self.look.clone() {
            Look::Laptop => ServerIcon::Default,
            Look::Letters(LetterStyle::Gradient)
                if !device && letters.is_none() && hue.is_none() =>
            {
                ServerIcon::Default
            }
            Look::Letters(style) => ServerIcon::Letters {
                style,
                letters,
                hue,
            },
            Look::Pattern => ServerIcon::Pattern { hue },
            Look::Emoji(emoji) => ServerIcon::Emoji { emoji, hue },
            Look::Picture(image) => ServerIcon::Picture(image),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Generated,
    Emoji,
    Picture,
}

impl Tab {
    fn of(look: &Look) -> Self {
        match look {
            Look::Laptop | Look::Letters(_) | Look::Pattern => Self::Generated,
            Look::Emoji(_) => Self::Emoji,
            Look::Picture(_) => Self::Picture,
        }
    }

    /// The look a colour choice switches to when the draft's look cannot
    /// take that colour on this tab.
    fn tinted(self) -> Look {
        match self {
            Self::Emoji => Look::Emoji(EMOJI[0].to_owned()),
            Self::Generated | Self::Picture => Look::Letters(LetterStyle::Gradient),
        }
    }
}

const LETTERS_ERROR: &str = "Use one or two characters.";

struct IconEditor {
    shell: WeakEntity<Shell>,
    id: Uuid,
    name: String,
    device: bool,
    draft: Draft,
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
        icon: &ServerIcon,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (draft, chosen) = Draft::of(icon, device);
        let placeholder = server_icon::initials(&name);
        let letters = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(placeholder)
                .default_value(chosen.unwrap_or_default())
        });
        let subscription = cx.subscribe_in(&letters, window, |editor, input, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                let typed = !input.read(cx).value().trim().is_empty();
                if typed && !matches!(editor.draft.look, Look::Letters(_)) {
                    editor.choose(Look::Letters(LetterStyle::Gradient));
                }
                cx.notify();
            }
        });
        Self {
            shell,
            id,
            name,
            device,
            tab: Tab::of(&draft.look),
            draft,
            letters,
            error: None,
            _letters: subscription,
            _pick: None,
        }
    }

    /// The letters field: blank follows the name, anything longer than two
    /// characters is refused.
    fn letters(&self, cx: &App) -> Result<Option<String>, &'static str> {
        let typed = self.letters.read(cx).value();
        match server_icon::letters(&typed) {
            None if !typed.trim().is_empty() => Err(LETTERS_ERROR),
            letters => Ok(letters),
        }
    }

    /// Switch looks, dropping an image still being read so it cannot
    /// replace this choice when it finishes.
    fn choose(&mut self, look: Look) {
        self._pick = None;
        self.error = None;
        self.draft.look = look;
    }

    fn set_hue(&mut self, hue: Option<u16>) {
        let tinted = self.draft.clone().tinted(hue, self.tab);
        if tinted.look != self.draft.look {
            self.choose(tinted.look.clone());
        }
        self.draft = tinted;
    }

    fn reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.letters
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.choose(Draft::generated(self.device).look);
        self.draft.hue = None;
        self.tab = Tab::Generated;
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
                        editor.draft.look = Look::Picture(image);
                        editor.error = None;
                    }
                    Err(err) => editor.error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let letters = match self.letters(cx) {
            Ok(letters) => letters,
            Err(_) if !matches!(self.draft.look, Look::Letters(_)) => None,
            Err(_) => return,
        };
        let (id, icon) = (self.id, self.draft.icon(letters, self.device));
        let saved = self
            .shell
            .update(cx, |shell, cx| {
                let saved = shell.save_server_icon(id, icon);
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

    /// `draft` drawn at `size`, with the letters typed so far.
    fn preview(&self, draft: &Draft, size: f32, cx: &App) -> AnyElement {
        let icon = draft.icon(self.letters(cx).unwrap_or_default(), self.device);
        server_icon::render(&icon, &self.name, self.device, px(size), cx)
    }

    fn render_generated(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let mut looks = vec![
            ("Gradient", Look::Letters(LetterStyle::Gradient)),
            ("Monogram", Look::Letters(LetterStyle::Monogram)),
            ("Pattern", Look::Pattern),
        ];
        if self.device {
            looks.insert(0, ("Laptop", Look::Laptop));
        }
        let row =
            h_flex()
                .gap_2()
                .children(looks.into_iter().enumerate().map(|(i, (label, look))| {
                    let on = self.draft.look == look;
                    let draft = Draft {
                        look: look.clone(),
                        hue: self.draft.hue,
                    };
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
                        .child(self.preview(&draft, 40., cx))
                        .child(label)
                        .on_click(cx.listener(move |editor, _, _, cx| {
                            editor.choose(look.clone());
                            cx.notify();
                        }))
                }));
        v_flex()
            .gap_3()
            .child(row)
            .child(field("Colour", self.render_swatches(0.65, 0.52, cx), cx))
            .child(
                field(
                    "Letters",
                    h_flex()
                        .items_center()
                        .gap_2()
                        .child(div().w(px(84.)).child(
                            text_field(Input::new(&self.letters), cx).aria_label("Icon letters"),
                        ))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("Blank uses the name's initials"),
                        ),
                    cx,
                )
                .items_center(),
            )
            .into_any_element()
    }

    /// The colour choices: the name's own hue first, then [`HUES`], drawn
    /// at the saturation and lightness the tab's icons use.
    fn render_swatches(&self, saturation: f32, lightness: f32, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let hue = self.draft.hue;
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
                .on_click(cx.listener(move |editor, _, _, cx| {
                    editor.set_hue(hue);
                    cx.notify();
                }))
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
        let mut grid = v_flex().gap_1();
        for (row, chunk) in EMOJI.chunks(8).enumerate() {
            grid = grid.child(h_flex().gap_1().children(chunk.iter().enumerate().map(
                |(col, emoji)| {
                    let look = Look::Emoji((*emoji).to_owned());
                    let on = self.draft.look == look;
                    div()
                        .id(("icon-emoji", row * 8 + col))
                        .role(Role::RadioButton)
                        .aria_label(*emoji)
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
                        .child(*emoji)
                        .on_click(cx.listener(move |editor, _, _, cx| {
                            editor.choose(look.clone());
                            cx.notify();
                        }))
                },
            )));
        }
        v_flex()
            .gap_3()
            .child(grid)
            .child(field("Backdrop", self.render_swatches(0.30, 0.30, cx), cx))
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

/// A labelled row in the editor's pane.
fn field(label: &'static str, control: impl IntoElement, cx: &App) -> Div {
    h_flex()
        .items_start()
        .gap_2()
        .child(
            div()
                .w(px(56.))
                .flex_none()
                .pt_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(control)
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
            .child(self.preview(&self.draft, 96., cx))
            .child(
                h_flex()
                    .items_end()
                    .gap_3()
                    .child(self.preview(&self.draft, 40., cx))
                    .child(self.preview(&self.draft, 28., cx))
                    .child(self.preview(&self.draft, 20., cx)),
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
        let letters_error = matches!(self.draft.look, Look::Letters(_))
            .then(|| self.letters(cx).err())
            .flatten();
        let error = letters_error.map(str::to_owned).or(self.error.clone());
        let footer = h_flex()
            .gap_2()
            .pt_2()
            .child(
                Button::new("icon-default")
                    .control(Tier::Ghost, Scale::Standard, false, cx)
                    .icon(IconName::Undo2)
                    .label("Use generated icon")
                    .on_click(cx.listener(|editor, _, window, cx| {
                        editor.reset(window, cx);
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
                    .control(Tier::Primary, Scale::Standard, letters_error.is_some(), cx)
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
            .when_some(error, |body, error| {
                body.child(div().text_xs().text_color(theme.danger).child(error))
            })
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::core::prelude::v1::test]
    fn drafts_round_trip_and_plain_letters_stay_the_default() {
        let plain = Draft::generated(false);
        assert_eq!(plain.icon(None, false), ServerIcon::Default);
        let laptop = Draft::generated(true);
        assert!(laptop.look == Look::Laptop);
        assert_eq!(laptop.icon(None, true), ServerIcon::Default);
        let tinted = Draft {
            hue: Some(40),
            ..plain.clone()
        };
        let saved = tinted.icon(Some("Q".into()), false);
        assert_eq!(
            saved,
            ServerIcon::Letters {
                style: LetterStyle::Gradient,
                letters: Some("Q".into()),
                hue: Some(40),
            }
        );
        let (back, letters) = Draft::of(&saved, false);
        assert!(back == tinted);
        assert_eq!(letters.as_deref(), Some("Q"));
        let on_device = Draft {
            look: Look::Letters(LetterStyle::Gradient),
            hue: None,
        };
        assert!(
            matches!(on_device.icon(None, true), ServerIcon::Letters { .. }),
            "this computer's default is the laptop, so its plain letters are a choice"
        );
    }

    #[::core::prelude::v1::test]
    fn a_colour_switches_to_a_look_that_can_show_it_on_the_open_tab() {
        let tinted = |look: Look, tab: Tab| Draft { look, hue: None }.tinted(Some(145), tab).look;
        assert!(tinted(Look::Laptop, Tab::Generated) == Look::Letters(LetterStyle::Gradient));
        assert!(tinted(Look::Pattern, Tab::Generated) == Look::Pattern);
        assert!(tinted(Look::Pattern, Tab::Emoji) == Look::Emoji(EMOJI[0].to_owned()));
        assert!(tinted(Look::Emoji("🐙".into()), Tab::Emoji) == Look::Emoji("🐙".into()));
    }
}
