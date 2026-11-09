//! Bot-authored HTML+CSS visuals, rendered offline with Takumi. The API
//! preview tool and the desktop chat share this one render, so what a bot
//! checks is what the owner sees. No scripts run and nothing is fetched:
//! only bundled fonts and `data:` images load.

pub mod isolate;

use std::sync::{Arc, OnceLock};

use takumi::prelude::{FontResource, Fonts, FromHtmlOptions, RenderOptions, StyleSheet, Viewport};
use takumi::unstable::html::parse_html;

/// The attachment type of a visual. A linked `.html` file stays a plain
/// download; only a file shown with image syntax becomes a visual.
pub const MEDIA_TYPE: &str = "application/vnd.else.visual+html";
/// Largest HTML source a visual may have.
pub const MAX_BYTES: usize = 512 * 1024;
/// The chat column width bots design for, in CSS pixels.
pub const REFERENCE_WIDTH: u32 = 680;
pub const MIN_WIDTH: u32 = 240;
pub const MAX_WIDTH: u32 = 1600;
/// Content below this many CSS pixels is cut off.
pub const MAX_HEIGHT: u32 = 4000;
const MAX_SCALE: f32 = 3.0;
/// Element nesting a visual may use; dashboards need a dozen or so levels.
const MAX_DEPTH: usize = 128;
/// Device pixels one render may allocate (64 MiB of RGBA). A larger visual
/// renders at a lower scale instead.
const MAX_PIXELS: f32 = 16.0 * 1024.0 * 1024.0;

pub const SANS: &str = "Inter";
pub const MONO: &str = "JetBrains Mono";

/// The theme colors a visual reads as `var(--name)`. The host fills them
/// from its current theme, so one visual follows light and dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Background,
    Foreground,
    Muted,
    MutedForeground,
    Card,
    Border,
    Primary,
    PrimaryForeground,
    Accent,
    Success,
    Warning,
    Danger,
    Info,
    Chart1,
    Chart2,
    Chart3,
    Chart4,
    Chart5,
}

impl Color {
    pub const ALL: [Color; 18] = [
        Color::Background,
        Color::Foreground,
        Color::Muted,
        Color::MutedForeground,
        Color::Card,
        Color::Border,
        Color::Primary,
        Color::PrimaryForeground,
        Color::Accent,
        Color::Success,
        Color::Warning,
        Color::Danger,
        Color::Info,
        Color::Chart1,
        Color::Chart2,
        Color::Chart3,
        Color::Chart4,
        Color::Chart5,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Color::Background => "background",
            Color::Foreground => "foreground",
            Color::Muted => "muted",
            Color::MutedForeground => "muted-foreground",
            Color::Card => "card",
            Color::Border => "border",
            Color::Primary => "primary",
            Color::PrimaryForeground => "primary-foreground",
            Color::Accent => "accent",
            Color::Success => "success",
            Color::Warning => "warning",
            Color::Danger => "danger",
            Color::Info => "info",
            Color::Chart1 => "chart-1",
            Color::Chart2 => "chart-2",
            Color::Chart3 => "chart-3",
            Color::Chart4 => "chart-4",
            Color::Chart5 => "chart-5",
        }
    }
}

/// Straight (not premultiplied) 8-bit RGBA.
pub type Rgba = [u8; 4];

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Theme {
    pub dark: bool,
    pub radius: f32,
    colors: [Rgba; Color::ALL.len()],
}

impl Theme {
    pub fn new(dark: bool, radius: f32, color: impl Fn(Color) -> Rgba) -> Self {
        Self {
            dark,
            radius,
            colors: Color::ALL.map(color),
        }
    }

    /// The fallback palette, used where no app theme is at hand.
    pub fn standard(dark: bool) -> Self {
        let hex = |value: u32| {
            let [_, r, g, b] = value.to_be_bytes();
            [r, g, b, 255]
        };
        Self::new(dark, 8.0, |color| {
            hex(match (color, dark) {
                (Color::Background, true) => 0x18181b,
                (Color::Background, false) => 0xffffff,
                (Color::Foreground, true) => 0xfafafa,
                (Color::Foreground, false) => 0x09090b,
                (Color::Muted, true) => 0x27272a,
                (Color::Muted, false) => 0xf4f4f5,
                (Color::MutedForeground, true) => 0xa1a1aa,
                (Color::MutedForeground, false) => 0x71717a,
                (Color::Card, true) => 0x202024,
                (Color::Card, false) => 0xfafafa,
                (Color::Border, true) => 0x3f3f46,
                (Color::Border, false) => 0xe4e4e7,
                (Color::Primary, true) => 0x3b82f6,
                (Color::Primary, false) => 0x2563eb,
                (Color::PrimaryForeground, _) => 0xffffff,
                (Color::Accent, true) => 0x27272a,
                (Color::Accent, false) => 0xf4f4f5,
                (Color::Success, true) => 0x22c55e,
                (Color::Success, false) => 0x16a34a,
                (Color::Warning, true) => 0xf59e0b,
                (Color::Warning, false) => 0xd97706,
                (Color::Danger, true) => 0xef4444,
                (Color::Danger, false) => 0xdc2626,
                (Color::Info, true) => 0x0ea5e9,
                (Color::Info, false) => 0x0284c7,
                (Color::Chart1, _) => 0x3b82f6,
                (Color::Chart2, _) => 0x22c55e,
                (Color::Chart3, _) => 0xf59e0b,
                (Color::Chart4, _) => 0xa855f7,
                (Color::Chart5, _) => 0xec4899,
            })
        })
    }

    pub fn color(&self, color: Color) -> Rgba {
        let index = Color::ALL.iter().position(|c| *c == color).unwrap_or(0);
        self.colors[index]
    }

    /// The sheet applied before the visual's own CSS: theme variables,
    /// bundled fonts, and a transparent page so the bubble shows through.
    fn base_css(&self) -> String {
        let mut css = String::from(":root {\n");
        for color in Color::ALL {
            let [r, g, b, a] = self.color(color);
            let alpha = f32::from(a) / 255.0;
            css.push_str(&format!(
                "  --{}: rgba({r}, {g}, {b}, {alpha:.3});\n",
                color.name()
            ));
        }
        css.push_str(&format!(
            "  --radius: {}px;\n  --font-sans: \"{SANS}\", sans-serif;\n  --font-mono: \"{MONO}\", monospace;\n  color-scheme: {};\n}}\n",
            self.radius,
            if self.dark { "dark" } else { "light" },
        ));
        css.push_str(
            ":root, html, body { margin: 0; padding: 0; background: transparent; \
             color: var(--foreground); font-family: var(--font-sans); font-size: 14px; \
             line-height: 1.45; }\n\
             code, pre, kbd, samp { font-family: var(--font-mono); }\n",
        );
        css
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Error {
    #[error("The visual is {0} bytes; the limit is {MAX_BYTES}.")]
    TooLarge(usize),
    #[error("The HTML could not be parsed: {0}")]
    Html(String),
    #[error("The visual could not be rendered: {0}")]
    Render(String),
    #[error("The visual rendered nothing visible.")]
    Empty,
    #[error("The visual is too heavy to render: {0}.")]
    Limit(String),
}

/// One render of a visual, in device pixels.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    /// Straight RGBA, row-major.
    pub rgba: Vec<u8>,
    /// The content's full height in CSS pixels, before any cut.
    pub content_height: u32,
    /// Whether content past [`MAX_HEIGHT`] was cut off.
    pub clipped: bool,
    /// Remote images and fonts the visual names, none of which load.
    pub blocked: Vec<String>,
}

impl Rendered {
    /// This render laid over an opaque `backdrop`, as it looks in the chat.
    pub fn over(mut self, backdrop: Rgba) -> Self {
        for pixel in self.rgba.as_chunks_mut::<4>().0 {
            let alpha = u32::from(pixel[3]);
            for channel in 0..3 {
                let blended = u32::from(pixel[channel]) * alpha
                    + u32::from(backdrop[channel]) * (255 - alpha);
                pixel[channel] = (blended / 255) as u8;
            }
            pixel[3] = 255;
        }
        self
    }

    pub fn png(&self) -> Result<Vec<u8>, Error> {
        let bitmap = takumi::prelude::Bitmap::from_raw(self.width, self.height, self.rgba.clone())
            .ok_or_else(|| Error::Render("pixel buffer size mismatch".into()))?;
        let mut png = Vec::new();
        takumi::write_image(&bitmap, &mut png, takumi::prelude::OutputFormat::Png)
            .map_err(|e| Error::Render(e.to_string()))?;
        Ok(png)
    }
}

/// Render `html` at `width` CSS pixels (clamped to the supported range)
/// and `scale` device pixels per CSS pixel, with the height its content
/// needs.
pub fn render(html: &str, width: u32, scale: f32, theme: &Theme) -> Result<Rendered, Error> {
    if html.len() > MAX_BYTES {
        return Err(Error::TooLarge(html.len()));
    }
    let width = width.clamp(MIN_WIDTH, MAX_WIDTH);
    let scale = if scale.is_finite() {
        scale.clamp(1.0, MAX_SCALE)
    } else {
        1.0
    };
    let parsed = parse_html(
        html,
        FromHtmlOptions::builder().max_depth(MAX_DEPTH).build(),
    )
    .map_err(|e| Error::Html(e.to_string()))?;
    let mut sheets = vec![theme.base_css()];
    sheets.extend(parsed.css);
    let stylesheet = Arc::new(StyleSheet::parse_list_loosy(&sheets));
    let fonts = fonts();
    let options = |viewport: Viewport| {
        RenderOptions::builder()
            .viewport(viewport)
            .node(parsed.node.clone())
            .fonts(fonts)
            .stylesheet(stylesheet.clone())
            .build()
    };

    let needs = takumi::needs(options(Viewport::new((width, None))));
    let blocked: Vec<String> = needs.images.into_iter().chain(needs.font_files).collect();

    let measured = takumi::measure(options(Viewport::new((width, None))))
        .map_err(|e| Error::Render(e.to_string()))?;
    let content_height = measured.height.max(0.0).ceil() as u32;
    if content_height == 0 {
        return Err(Error::Empty);
    }
    let height = content_height.min(MAX_HEIGHT);
    let scale = scale.min((MAX_PIXELS / (width * height) as f32).sqrt());
    let device = |css: u32| (css as f32 * scale).floor().max(1.0) as u32;
    let bitmap = takumi::render(options(
        Viewport::new((device(width), device(height))).with_device_pixel_ratio(scale),
    ))
    .map_err(|e| Error::Render(e.to_string()))?;
    Ok(Rendered {
        width: bitmap.width(),
        height: bitmap.height(),
        rgba: bitmap.into_raw(),
        content_height,
        clipped: content_height > MAX_HEIGHT,
        blocked,
    })
}

fn fonts() -> &'static Fonts {
    static FONTS: OnceLock<Fonts> = OnceLock::new();
    FONTS.get_or_init(|| {
        let mut fonts = Fonts::default();
        for data in [
            &include_bytes!("../fonts/InterVariable.woff2")[..],
            &include_bytes!("../fonts/JetBrainsMono.ttf")[..],
        ] {
            fonts
                .register(FontResource::new(data.to_vec()))
                .expect("bundled fonts are valid");
        }
        fonts
    })
}
