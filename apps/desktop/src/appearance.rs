#[cfg(target_os = "linux")]
mod omarchy;

use std::rc::Rc;

use gpui_kit::component::{Theme, ThemeConfig, ThemeMode};
use gpui_kit::{App, Global, Task, Window};

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

#[derive(Debug, Clone, Copy)]
struct Style {
    radius: u8,
    radius_lg: u8,
    shadow: bool,
    border_strength: f32,
}

impl Style {
    const SYSTEM: Self = Self {
        radius: 7,
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
    on_accent: Rgb,
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
    on_accent: Rgb(0xffffff),
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
    on_accent: Rgb(0x101f36),
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
        let highlight = |base: Rgb| base.mix(p.foreground, if dark { 0.15 } else { 0.10 });
        let hover = highlight(p.raised);
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
        let danger = readable(p.red);
        let on_danger = on(danger);
        let popover = if dark { p.raised } else { p.background };
        let border = p.background.mix(p.border, style.border_strength);
        let roles = [
            ("background", p.background),
            ("foreground", p.foreground),
            ("border", border),
            ("input.border", border),
            ("window.border", border),
            ("muted.background", p.raised),
            ("muted.foreground", readable(p.muted_foreground)),
            ("secondary.background", p.raised),
            ("secondary.foreground", p.foreground),
            ("secondary.hover.background", hover),
            ("secondary.active.background", active),
            ("button.background", p.raised),
            ("button.foreground", p.foreground),
            ("button.hover.background", hover),
            ("button.active.background", active),
            ("accent.background", hover),
            ("accent.foreground", p.foreground),
            ("primary.background", p.accent),
            ("primary.foreground", p.on_accent),
            (
                "primary.hover.background",
                button_state(p.accent, p.on_accent, 0.12),
            ),
            (
                "primary.active.background",
                button_state(p.accent, p.on_accent, 0.20),
            ),
            ("ring", p.accent),
            ("caret", p.accent),
            ("link", p.accent),
            ("link.hover", p.accent.mix(p.foreground, 0.2)),
            ("selection.background", p.accent),
            ("list.background", p.background),
            ("list.hover.background", hover),
            ("list.active.background", p.selection),
            ("list.active.border", p.accent),
            ("popover.background", popover),
            ("popover.foreground", p.foreground),
            ("sidebar.background", p.surface),
            ("sidebar.foreground", p.foreground),
            ("sidebar.border", border),
            ("sidebar.accent.background", hover),
            ("sidebar.accent.foreground", p.foreground),
            ("sidebar.primary.background", p.accent),
            ("sidebar.primary.foreground", p.on_accent),
            ("title_bar.background", p.surface),
            ("title_bar.border", border),
            ("status_bar.background", p.surface),
            ("status_bar.border", border),
            ("scrollbar.thumb.background", p.border),
            ("scrollbar.thumb.hover.background", p.muted_foreground),
            ("switch.background", p.border),
            ("switch.thumb.background", p.background),
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
            ("warning.background", p.yellow),
            ("warning.foreground", on(p.yellow)),
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
        let colors: serde_json::Map<_, _> = roles
            .into_iter()
            .map(|(key, color)| (key.to_owned(), color.hex().into()))
            .collect();
        serde_json::from_value(serde_json::json!({
            "name": name,
            "mode": if dark { "dark" } else { "light" },
            "radius": style.radius,
            "radius.lg": style.radius_lg,
            "shadow": style.shadow,
            "colors": colors,
        }))
        .expect("every palette projects onto a theme")
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
            assert!(palette.accent.contrast(palette.on_accent) >= 4.5);
        }
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
        assert_eq!((config.radius, config.radius_lg), (Some(7), Some(12)));
        assert_eq!(config.shadow, Some(true));
        let colors = &config.colors;
        assert_eq!(colors.border.as_deref(), Some("#3a3a3e"));
        assert_eq!(colors.primary.as_deref(), Some("#69a9ff"));
        assert_eq!(colors.sidebar.as_deref(), Some("#28282b"));
        assert_eq!(colors.list_active.as_deref(), Some("#35445a"));
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
