#[cfg(target_os = "linux")]
mod omarchy;

use std::rc::Rc;

use gpui_kit::component::dialog::Dialog;
use gpui_kit::component::{ActiveTheme as _, Theme, ThemeConfig, ThemeMode};
use gpui_kit::{
    App, Div, FontWeight, Global, ParentElement as _, SharedString, Styled, Task, Window, div,
};

enum Source {
    System,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Omarchy(Palette),
}

struct Appearance {
    source: Source,
    _follow: Option<Task<()>>,
}

impl Global for Appearance {}

/// Installs the Quiet palettes, then the Omarchy palette when one is
/// readable. Without one, the system appearance applies until a valid
/// palette appears.
pub fn init(cx: &mut App) {
    let light = Rc::new(QUIET_LIGHT.config("Quiet Light", Style::SYSTEM));
    let dark = Rc::new(QUIET_DARK.config("Quiet Dark", Style::SYSTEM));
    Theme::update(cx, |theme| {
        theme.light_theme = light;
        theme.dark_theme = dark;
    });
    cx.set_global(Appearance {
        source: Source::System,
        _follow: None,
    });
    #[cfg(target_os = "linux")]
    omarchy::follow(cx);
    if matches!(cx.global::<Appearance>().source, Source::System) {
        Theme::sync_system_appearance(None, cx);
    }
}

/// Follows the window's light or dark appearance while no Omarchy palette
/// is installed.
pub fn observe_window(window: &mut Window, cx: &mut App) {
    sync_system(window, cx);
    window.observe_window_appearance(sync_system).detach();
}

fn sync_system(window: &mut Window, cx: &mut App) {
    if matches!(cx.global::<Appearance>().source, Source::System) {
        Theme::sync_system_appearance(Some(window), cx);
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn install_omarchy(palette: Palette, cx: &mut App) {
    let state = cx.global_mut::<Appearance>();
    if matches!(&state.source, Source::Omarchy(current) if *current == palette) {
        return;
    }
    let config = Rc::new(palette.config("Omarchy", Style::OMARCHY));
    state.source = Source::Omarchy(palette);
    Theme::update(cx, |theme| theme.apply_config(&config));
}

/// A section heading inside a page, panel or dialog: sentence case, small
/// and quiet, so it groups without competing with titles.
pub fn section_label(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground.opacity(0.9))
        .child(text.into())
}

/// The surface every modal sheet shares: one step above the page with a
/// visible edge, so it lifts off the scrim in both light and dark.
pub fn modal_surface(dialog: Dialog, cx: &App) -> Dialog {
    let theme = cx.theme();
    let edge = if theme.mode.is_dark() {
        theme.foreground.opacity(0.12)
    } else {
        theme.border
    };
    dialog.bg(theme.popover).border_color(edge)
}

#[derive(Debug, Clone, Copy)]
struct Style {
    radius: u8,
    radius_lg: u8,
    shadow: bool,
    border_strength: f32,
}

impl Style {
    const SYSTEM: Self = Self {
        radius: 8,
        radius_lg: 12,
        shadow: true,
        border_strength: 1.,
    };

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    const OMARCHY: Self = Self {
        border_strength: 0.5,
        ..Self::SYSTEM
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb(u32);

impl Rgb {
    // Only the Linux Omarchy theme reader parses hex colors outside tests.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(hex, 16).ok().map(Self)
    }

    fn channels(self) -> [u8; 3] {
        let [_, r, g, b] = self.0.to_be_bytes();
        [r, g, b]
    }

    fn hex(self) -> String {
        format!("#{:06x}", self.0)
    }

    fn mix(self, other: Self, t: f32) -> Self {
        let (a, b) = (self.channels(), other.channels());
        let channel = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u32;
        Self(channel(0) << 16 | channel(1) << 8 | channel(2))
    }

    /// WCAG relative luminance.
    fn luminance(self) -> f32 {
        let linear = |c: u8| {
            let c = c as f32 / 255.;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let [r, g, b] = self.channels();
        0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
    }

    /// WCAG contrast ratio, from 1 to 21.
    fn contrast(self, other: Self) -> f32 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    fn hsl(self) -> [f32; 3] {
        let [r, g, b] = self.channels().map(|c| c as f32 / 255.);
        let (max, min) = (r.max(g).max(b), r.min(g).min(b));
        let l = (max + min) / 2.;
        let d = max - min;
        if d == 0. {
            return [0., 0., l];
        }
        let s = d / (1. - (2. * l - 1.).abs());
        let h = if max == r {
            ((g - b) / d).rem_euclid(6.)
        } else if max == g {
            (b - r) / d + 2.
        } else {
            (r - g) / d + 4.
        };
        [h * 60., s, l]
    }

    fn from_hsl([h, s, l]: [f32; 3]) -> Self {
        let c = (1. - (2. * l - 1.).abs()) * s;
        let x = c * (1. - ((h / 60.).rem_euclid(2.) - 1.).abs());
        let (r, g, b) = match (h / 60.) as u32 % 6 {
            0 => (c, x, 0.),
            1 => (x, c, 0.),
            2 => (0., c, x),
            3 => (0., x, c),
            4 => (x, 0., c),
            _ => (c, 0., x),
        };
        let m = l - c / 2.;
        let channel = |v: f32| ((v + m).clamp(0., 1.) * 255.).round() as u32;
        Self(channel(r) << 16 | channel(g) << 8 | channel(b))
    }

    fn most_legible(self, candidates: &[Self]) -> Self {
        candidates
            .iter()
            .copied()
            .max_by(|a, b| self.contrast(*a).total_cmp(&self.contrast(*b)))
            .expect("at least one candidate")
    }
}

const WHITE: Rgb = Rgb(0xffffff);
const BLACK: Rgb = Rgb(0x000000);

fn text_on(fill: Rgb, background: Rgb, foreground: Rgb) -> Rgb {
    let native = fill.most_legible(&[background, foreground]);
    if fill.contrast(native) >= 4.5 {
        native
    } else {
        fill.most_legible(&[BLACK, WHITE])
    }
}

/// The fill for primary actions and selected controls: the accent's hue
/// at full strength, deep enough to carry white text. A theme whose accent
/// is pale or greyed (a soft blue on a dark theme) still gets a solid,
/// unmistakable primary instead of a fill that reads as disabled.
fn strong_fill(accent: Rgb) -> Rgb {
    let [h, s, l] = accent.hsl();
    let s = if s < 0.12 { s } else { s.max(0.68) };
    (0..=60)
        .map(|step| Rgb::from_hsl([h, s, l.min(0.62) - step as f32 / 100.]))
        .find(|fill| fill.contrast(WHITE) >= 4.5)
        .unwrap_or(BLACK)
}

#[derive(Debug, Clone, PartialEq)]
struct Palette {
    mode: ThemeMode,
    background: Rgb,
    surface: Rgb,
    raised: Rgb,
    foreground: Rgb,
    muted_foreground: Rgb,
    border: Rgb,
    accent: Rgb,
    selection: Rgb,
    red: Rgb,
    yellow: Rgb,
    green: Rgb,
    blue: Rgb,
    magenta: Rgb,
    cyan: Rgb,
}

const QUIET_LIGHT: Palette = Palette {
    mode: ThemeMode::Light,
    background: Rgb(0xffffff),
    surface: Rgb(0xf4f4f6),
    raised: Rgb(0xf2f2f5),
    foreground: Rgb(0x252529),
    muted_foreground: Rgb(0x66666d),
    border: Rgb(0xdfdfe4),
    accent: Rgb(0x0a6fdb),
    selection: Rgb(0xe1e9f6),
    red: Rgb(0xc93c3c),
    yellow: Rgb(0x9b6500),
    green: Rgb(0x287d42),
    blue: Rgb(0x0a6fdb),
    magenta: Rgb(0x8e4ec6),
    cyan: Rgb(0x16808e),
};

const QUIET_DARK: Palette = Palette {
    mode: ThemeMode::Dark,
    background: Rgb(0x202022),
    surface: Rgb(0x28282b),
    raised: Rgb(0x2e2e32),
    foreground: Rgb(0xededf0),
    muted_foreground: Rgb(0xa0a0a8),
    border: Rgb(0x3a3a3e),
    accent: Rgb(0x69a9ff),
    selection: Rgb(0x35445a),
    red: Rgb(0xff9b9b),
    yellow: Rgb(0xe7be75),
    green: Rgb(0x79bd8f),
    blue: Rgb(0x69a9ff),
    magenta: Rgb(0xc9a2f0),
    cyan: Rgb(0x7cc8d4),
};

impl Palette {
    /// The component theme for this palette. Every role a view or control
    /// paints is set here, because a theme file resets whatever it leaves
    /// out to the library's defaults.
    fn config(&self, name: &str, style: Style) -> ThemeConfig {
        let p = self;
        let dark = p.mode.is_dark();
        let highlight = |base: Rgb| base.mix(p.foreground, 0.15);
        let raised = if dark {
            p.raised
        } else {
            p.background.mix(p.foreground, 0.05)
        };
        let hover = highlight(raised);
        let active = highlight(hover);
        let on = |fill: Rgb| text_on(fill, p.background, p.foreground);
        let button_state = |fill: Rgb, text: Rgb, amount| {
            fill.mix(
                if fill.luminance() > text.luminance() {
                    WHITE
                } else {
                    BLACK
                },
                amount,
            )
        };
        let readable = |text: Rgb| {
            // Leave a margin for the component library's color conversions.
            (0..=20)
                .map(|step| text.mix(p.foreground, step as f32 / 20.))
                .find(|color| hover.contrast(*color) >= 4.6)
                .unwrap_or_else(|| on(hover))
        };
        // Code keeps its hues: in light, a pale palette color deepens
        // toward black instead of fading into the text color.
        let syntax_color = |color: Rgb| {
            if dark {
                return readable(color);
            }
            (0..=20)
                .map(|step| color.mix(BLACK, step as f32 / 40.))
                .find(|shade| hover.contrast(*shade) >= 4.6)
                .unwrap_or_else(|| readable(color))
        };
        let danger = readable(p.red);
        let warning = (0..=20)
            .map(|step| {
                p.yellow
                    .mix(if dark { p.foreground } else { BLACK }, step as f32 / 20.)
            })
            .find(|color| hover.contrast(*color) >= 4.6)
            .unwrap_or_else(|| on(hover));
        let on_danger = on(danger);
        // Dialogs and menus sit one step above the page: lighter than it in
        // dark, near white in light.
        let popover = if dark {
            p.raised.mix(p.foreground, 0.03)
        } else {
            p.background.mix(WHITE, 0.75)
        };
        // In light the conversation is the lightest surface: the sidebar
        // and panels sit a visible step below it.
        let sidebar = if dark {
            p.surface
        } else {
            p.background.mix(p.foreground, 0.07)
        };
        // A selected row reads as one firm step: lighter in dark, about 8%
        // black over the sidebar in light.
        let selected = if dark {
            p.background.mix(p.foreground, 0.14)
        } else {
            sidebar.mix(BLACK, 0.08)
        };
        let border = p.background.mix(p.border, style.border_strength);
        let (primary, on_primary) = (strong_fill(p.accent), WHITE);
        let roles = [
            ("background", p.background),
            ("foreground", p.foreground),
            ("border", border),
            ("input.border", border),
            ("window.border", border),
            ("muted.background", raised),
            ("muted.foreground", readable(p.muted_foreground)),
            ("secondary.background", raised),
            ("secondary.foreground", p.foreground),
            ("secondary.hover.background", hover),
            ("secondary.active.background", selected),
            ("button.background", raised),
            ("button.foreground", p.foreground),
            ("button.hover.background", hover),
            ("button.active.background", active),
            ("accent.background", hover),
            ("accent.foreground", p.foreground),
            ("primary.background", primary),
            ("primary.foreground", on_primary),
            ("button.primary.background", primary),
            ("button.primary.foreground", on_primary),
            (
                "button.primary.hover.background",
                button_state(primary, on_primary, 0.12),
            ),
            (
                "button.primary.active.background",
                button_state(primary, on_primary, 0.20),
            ),
            ("tab.active.background", selected),
            ("tab.active.foreground", p.foreground),
            (
                "primary.hover.background",
                button_state(primary, on_primary, 0.12),
            ),
            (
                "primary.active.background",
                button_state(primary, on_primary, 0.20),
            ),
            ("ring", p.accent),
            ("caret", p.accent),
            ("link", p.accent),
            ("link.hover", p.accent.mix(p.foreground, 0.2)),
            ("selection.background", p.accent),
            ("list.background", p.background),
            ("list.hover.background", hover),
            ("list.active.background", selected),
            ("list.active.border", p.accent),
            ("popover.background", popover),
            ("popover.foreground", p.foreground),
            ("sidebar.background", sidebar),
            ("sidebar.foreground", p.foreground),
            ("sidebar.border", border),
            ("sidebar.accent.background", hover),
            ("sidebar.accent.foreground", p.foreground),
            ("sidebar.primary.background", primary),
            ("sidebar.primary.foreground", on_primary),
            (
                "title_bar.background",
                if dark { p.surface } else { sidebar },
            ),
            ("title_bar.border", border),
            ("status_bar.background", p.surface),
            ("status_bar.border", border),
            ("scrollbar.thumb.background", p.border),
            ("scrollbar.thumb.hover.background", p.muted_foreground),
            // A solid white knob on the accent, and a neutral track when off.
            ("switch.background", p.background.mix(p.foreground, 0.28)),
            ("switch.thumb.background", WHITE),
            ("tab_bar.background", p.surface),
            ("danger.background", danger),
            ("danger.foreground", on_danger),
            ("button.danger.background", danger),
            ("button.danger.foreground", on_danger),
            (
                "button.danger.hover.background",
                button_state(danger, on_danger, 0.12),
            ),
            (
                "button.danger.active.background",
                button_state(danger, on_danger, 0.20),
            ),
            ("warning.background", warning),
            ("warning.foreground", on(warning)),
            ("success.background", p.green),
            ("success.foreground", on(p.green)),
            ("info.background", p.cyan),
            ("info.foreground", on(p.cyan)),
            ("base.red", p.red),
            ("base.yellow", p.yellow),
            ("base.green", p.green),
            ("base.blue", p.blue),
            ("base.magenta", p.magenta),
            ("base.cyan", p.cyan),
        ];
        let mut colors: serde_json::Map<_, _> = roles
            .into_iter()
            .map(|(key, color)| (key.to_owned(), color.hex().into()))
            .collect();
        colors.insert(
            "overlay".to_owned(),
            if dark { "#0000008c" } else { "#0000004d" }.into(),
        );
        let syntax: serde_json::Map<_, _> = [
            ("keyword", p.magenta),
            ("string", p.green),
            ("string.escape", p.cyan),
            ("string.regex", p.cyan),
            ("number", p.yellow),
            ("boolean", p.yellow),
            ("constant", p.yellow),
            ("function", p.blue),
            ("type", p.cyan),
            ("constructor", p.cyan),
            ("attribute", p.cyan),
            ("tag", p.red),
            ("variable.special", p.red),
            ("title", p.blue),
            ("link_uri", p.accent),
            ("comment", p.muted_foreground),
        ]
        .into_iter()
        .map(|(key, color)| {
            (
                key.to_owned(),
                serde_json::json!({ "color": syntax_color(color).hex() }),
            )
        })
        .collect();
        let mut config: ThemeConfig = serde_json::from_value(serde_json::json!({
            "name": name,
            "mode": if dark { "dark" } else { "light" },
            "radius": style.radius,
            "radius.lg": style.radius_lg,
            "shadow": style.shadow,
            "colors": colors,
            "highlight": { "syntax": syntax },
        }))
        .expect("every palette projects onto a theme");
        config.font_family = crate::ui_font::family();
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_parse_only_as_six_hex_digits() {
        assert_eq!(Rgb::parse("#1A1b26"), Some(Rgb(0x1a1b26)));
        for bad in [
            "1a1b26",
            "#abc",
            "#1a1b2",
            "#1a1b26ff",
            "#gggggg",
            "",
            "rgba(0,0,0,1)",
        ] {
            assert_eq!(Rgb::parse(bad), None, "{bad}");
        }
        assert_eq!(Rgb(0x0a0b0c).hex(), "#0a0b0c");
    }

    #[test]
    fn text_on_an_accent_stays_legible() {
        let (dark_bg, light_fg) = (Rgb(0x1a1b26), Rgb(0xc0caf5));
        let pale = Rgb(0xf1e05a);
        assert!(pale.contrast(text_on(pale, dark_bg, light_fg)) >= 4.5);
        let deep = Rgb(0x1f3a93);
        assert!(deep.contrast(text_on(deep, dark_bg, light_fg)) >= 4.5);
        let mid = Rgb(0x777777);
        assert!([BLACK, WHITE].contains(&text_on(mid, Rgb(0x808080), Rgb(0x707070))));
    }

    #[test]
    fn quiet_palettes_keep_text_readable() {
        for palette in [&QUIET_LIGHT, &QUIET_DARK] {
            assert!(palette.background.contrast(palette.foreground) >= 7.);
            assert!(palette.background.contrast(palette.muted_foreground) >= 4.5);
            assert!(palette.raised.contrast(palette.foreground) >= 7.);
            assert!(strong_fill(palette.accent).contrast(WHITE) >= 4.5);
        }
    }

    #[test]
    fn a_pale_accent_becomes_a_saturated_fill_of_the_same_hue() {
        for accent in [Rgb(0x81a1c1), Rgb(0x69a9ff), Rgb(0x1e66f5), Rgb(0xf1e05a)] {
            let fill = strong_fill(accent);
            let ([hue, _, _], [fill_hue, saturation, _]) = (accent.hsl(), fill.hsl());
            assert!(
                (hue - fill_hue).abs() < 4.,
                "{} keeps its hue",
                accent.hex()
            );
            assert!(saturation >= 0.6, "{} becomes saturated", accent.hex());
            assert!(
                fill.contrast(WHITE) >= 4.5,
                "{} carries white text",
                accent.hex()
            );
        }
        let strong = Rgb(0x1e66f5);
        for (fill, accent) in strong_fill(strong)
            .channels()
            .into_iter()
            .zip(strong.channels())
        {
            assert!(
                fill.abs_diff(accent) <= 1,
                "an already strong accent is kept"
            );
        }
        let grey = strong_fill(Rgb(0x9a9a9a)).hsl();
        assert!(grey[1] < 0.05, "a neutral accent stays neutral");
    }

    fn cursor_dark() -> Palette {
        Palette {
            background: Rgb(0x141414),
            surface: Rgb(0x0f0f0f),
            raised: Rgb(0x242424),
            foreground: Rgb(0xd4d4d4),
            accent: Rgb(0x81a1c1),
            selection: Rgb(0x2c2c2c),
            ..QUIET_DARK
        }
    }

    fn latte() -> Palette {
        Palette {
            background: Rgb(0xeff1f5),
            surface: Rgb(0xe3e4e8),
            raised: Rgb(0xdce0e8),
            foreground: Rgb(0x4c4f69),
            muted_foreground: Rgb(0x7b7e8f),
            accent: Rgb(0x1e66f5),
            yellow: Rgb(0xdf8e1d),
            ..QUIET_LIGHT
        }
    }

    #[test]
    fn primary_buttons_are_solid_and_selections_are_neutral() {
        let rgb = |color: gpui_kit::Hsla| Rgb(u32::from(gpui_kit::Rgba::from(color)) >> 8);
        for palette in [QUIET_LIGHT, QUIET_DARK, cursor_dark(), latte()] {
            let mut theme = Theme::default();
            theme.apply_config(&Rc::new(palette.config("test", Style::OMARCHY)));
            for (actual, expected) in rgb(theme.button_primary)
                .channels()
                .into_iter()
                .zip(strong_fill(palette.accent).channels())
            {
                assert!(actual.abs_diff(expected) <= 1);
            }
            assert_eq!(theme.button_primary.a, 1.);
            assert!(
                rgb(theme.button_primary).contrast(rgb(theme.button_primary_foreground)) >= 4.5
            );
            assert_eq!(rgb(theme.button_primary_foreground), WHITE);
            assert_ne!(rgb(theme.secondary_active), palette.accent);
            assert!(rgb(theme.secondary_active).contrast(rgb(theme.secondary_foreground)) >= 4.5);
            if !palette.mode.is_dark() {
                let [red, green, blue] = rgb(theme.warning).channels();
                assert!(red > green && green > blue, "warnings keep their amber hue");
                assert!(rgb(theme.warning).contrast(rgb(theme.accent)) >= 4.5);
            }
            assert!(theme.overlay.a >= 0.2 && theme.overlay.a <= 0.55);
        }
    }

    #[test]
    fn input_selection_is_distinct_and_keeps_text_readable() {
        for (name, palette) in [
            ("Quiet Light", QUIET_LIGHT),
            ("Quiet Dark", QUIET_DARK),
            ("Cursor Dark", cursor_dark()),
        ] {
            let mut theme = Theme::default();
            theme.apply_config(&Rc::new(palette.config(name, Style::SYSTEM)));
            for background in [palette.background, palette.surface, palette.raised] {
                let selected = gpui_kit::rgb(background.0).blend(theme.selection.into());
                let selected = Rgb(u32::from(selected) >> 8);
                let contrast = background.contrast(selected);
                assert!(
                    contrast >= 1.5,
                    "{name}: selected input contrast {contrast:.2}"
                );
                assert!(
                    palette.foreground.contrast(selected) >= 4.5,
                    "{name}: selected text must stay readable"
                );
            }
        }
    }

    #[test]
    fn a_palette_projects_its_mode_style_and_roles() {
        let config = QUIET_DARK.config("Quiet Dark", Style::SYSTEM);
        assert_eq!(config.mode, ThemeMode::Dark);
        assert_eq!((config.radius, config.radius_lg), (Some(8), Some(12)));
        assert_eq!(config.shadow, Some(true));
        let colors = &config.colors;
        assert_eq!(colors.border.as_deref(), Some("#3a3a3e"));
        assert_eq!(
            colors.primary.as_deref(),
            Some(strong_fill(QUIET_DARK.accent).hex().as_str())
        );
        assert_eq!(colors.sidebar.as_deref(), Some("#28282b"));
        assert_eq!(colors.list_active.as_deref(), Some("#3d3d3f"));
    }

    #[test]
    fn hover_states_are_visible_and_keep_control_text_readable() {
        let rgb = |color: gpui_kit::Rgba| Rgb(u32::from(color) >> 8);
        let mut failures = Vec::new();
        for (name, palette) in [
            ("Quiet Light", QUIET_LIGHT),
            ("Quiet Dark", QUIET_DARK),
            ("Cursor Dark", cursor_dark()),
        ] {
            let mut theme = Theme::default();
            theme.apply_config(&Rc::new(palette.config(name, Style::SYSTEM)));
            let paint = |color: gpui_kit::Hsla| {
                rgb(gpui_kit::rgb(palette.background.0).blend(color.into()))
            };
            let mut check = |role, contrast: f32, minimum| {
                if contrast < minimum {
                    failures.push(format!("{name}: {role} contrast {contrast:.2} < {minimum}"));
                }
            };
            for (role, surface, fill, foreground) in [
                (
                    "menu hover",
                    theme.popover,
                    theme.accent,
                    theme.accent_foreground,
                ),
                (
                    "list hover",
                    theme.colors.list,
                    theme.list_hover,
                    theme.foreground,
                ),
                (
                    "sidebar hover",
                    theme.sidebar,
                    theme.sidebar_accent,
                    theme.sidebar_foreground,
                ),
                (
                    "button hover",
                    theme.button,
                    theme.button_hover,
                    theme.button_foreground,
                ),
                (
                    "secondary hover",
                    theme.secondary,
                    theme.secondary_hover,
                    theme.secondary_foreground,
                ),
            ] {
                check(role, paint(surface).contrast(paint(fill)), 1.2);
                check(role, paint(foreground).contrast(paint(fill)), 4.5);
            }
            check(
                "destructive menu text",
                paint(theme.danger).contrast(paint(theme.accent)),
                4.5,
            );
            check(
                "secondary hover text",
                paint(theme.muted_foreground).contrast(paint(theme.accent)),
                4.5,
            );
            for background in [palette.background, palette.surface, palette.raised] {
                let hover = theme
                    .accent
                    .opacity(if palette.mode.is_dark() { 0.5 } else { 1. });
                let fill = rgb(gpui_kit::rgb(background.0).blend(hover.into()));
                check("ghost hover", background.contrast(fill), 1.15);
                check("ghost text", palette.foreground.contrast(fill), 4.5);
            }
            for (role, foreground, fill) in [
                (
                    "primary",
                    theme.button_primary_foreground,
                    theme.button_primary,
                ),
                (
                    "primary hover",
                    theme.button_primary_foreground,
                    theme.button_primary_hover,
                ),
                (
                    "primary pressed",
                    theme.button_primary_foreground,
                    theme.button_primary_active,
                ),
                (
                    "danger",
                    theme.button_danger_foreground,
                    theme.button_danger,
                ),
                (
                    "danger hover",
                    theme.button_danger_foreground,
                    theme.button_danger_hover,
                ),
                (
                    "danger pressed",
                    theme.button_danger_foreground,
                    theme.button_danger_active,
                ),
                (
                    "secondary pressed",
                    theme.secondary_foreground,
                    theme.secondary_active,
                ),
            ] {
                check(role, paint(foreground).contrast(paint(fill)), 4.5);
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
