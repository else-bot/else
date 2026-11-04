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
    let shape = Shape::system();
    let light = Rc::new(QUIET_LIGHT.config("Quiet Light", shape));
    let dark = Rc::new(QUIET_DARK.config("Quiet Dark", shape));
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
    let config = Rc::new(palette.config("Omarchy", Shape::OMARCHY));
    state.source = Source::Omarchy(palette);
    Theme::update(cx, |theme| theme.apply_config(&config));
}

#[derive(Debug, Clone, Copy)]
struct Shape {
    radius: u8,
    radius_lg: u8,
    shadow: bool,
}

impl Shape {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    const OMARCHY: Self = Self {
        radius: 3,
        radius_lg: 4,
        shadow: false,
    };

    fn system() -> Self {
        if cfg!(target_os = "macos") {
            Self {
                radius: 7,
                radius_lg: 12,
                shadow: true,
            }
        } else {
            Self {
                radius: 6,
                radius_lg: 10,
                shadow: true,
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rgb(u32);

impl Rgb {
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
    fn config(&self, name: &str, shape: Shape) -> ThemeConfig {
        let p = self;
        let dark = p.mode.is_dark();
        let hover = |base: Rgb| base.mix(p.foreground, if dark { 0.08 } else { 0.05 });
        let on = |fill: Rgb| text_on(fill, p.background, p.foreground);
        let popover = if dark { p.raised } else { p.background };
        let roles = [
            ("background", p.background),
            ("foreground", p.foreground),
            ("border", p.border),
            ("input.border", p.border),
            ("window.border", p.border),
            ("muted.background", p.raised),
            ("muted.foreground", p.muted_foreground),
            ("secondary.background", p.raised),
            ("secondary.foreground", p.foreground),
            ("secondary.hover.background", hover(p.raised)),
            ("secondary.active.background", hover(hover(p.raised))),
            ("accent.background", hover(p.background)),
            ("accent.foreground", p.foreground),
            ("primary.background", p.accent),
            ("primary.foreground", p.on_accent),
            ("primary.hover.background", p.accent.mix(p.background, 0.12)),
            (
                "primary.active.background",
                p.accent.mix(p.foreground, 0.12),
            ),
            ("ring", p.accent),
            ("caret", p.accent),
            ("link", p.accent),
            ("link.hover", p.accent.mix(p.foreground, 0.2)),
            ("selection.background", p.selection),
            ("list.background", p.background),
            ("list.hover.background", hover(p.surface)),
            ("list.active.background", p.selection),
            ("list.active.border", p.accent),
            ("popover.background", popover),
            ("popover.foreground", p.foreground),
            ("sidebar.background", p.surface),
            ("sidebar.foreground", p.foreground),
            ("sidebar.border", p.border),
            ("sidebar.accent.background", hover(p.surface)),
            ("sidebar.accent.foreground", p.foreground),
            ("sidebar.primary.background", p.accent),
            ("sidebar.primary.foreground", p.on_accent),
            ("title_bar.background", p.surface),
            ("title_bar.border", p.border),
            ("status_bar.background", p.surface),
            ("status_bar.border", p.border),
            ("scrollbar.thumb.background", p.border),
            ("scrollbar.thumb.hover.background", p.muted_foreground),
            ("switch.background", p.border),
            ("switch.thumb.background", p.background),
            ("tab_bar.background", p.surface),
            ("danger.background", p.red),
            ("danger.foreground", on(p.red)),
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
            "radius": shape.radius,
            "radius.lg": shape.radius_lg,
            "shadow": shape.shadow,
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

    #[test]
    fn a_palette_projects_its_mode_shape_and_roles() {
        let config = QUIET_DARK.config("Quiet Dark", Shape::OMARCHY);
        assert_eq!(config.mode, ThemeMode::Dark);
        assert_eq!((config.radius, config.radius_lg), (Some(3), Some(4)));
        assert_eq!(config.shadow, Some(false));
        let colors = &config.colors;
        assert_eq!(colors.primary.as_deref(), Some("#69a9ff"));
        assert_eq!(colors.sidebar.as_deref(), Some("#28282b"));
        assert_eq!(colors.list_active.as_deref(), Some("#35445a"));
        assert_eq!(colors.danger.as_deref(), Some("#ff9b9b"));
    }
}
