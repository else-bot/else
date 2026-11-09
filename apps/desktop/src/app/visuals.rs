//! HTML visuals a bot attached, drawn inline at the bubble's width in the
//! current theme. Each visual re-renders off the main thread when the width
//! or theme changes and keeps showing its last render meanwhile.

use super::{ElseApp, RowKey};
use crate::api::Client;
use crate::attachments::{Attachment, MessagePart};
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Theme, v_flex};
use gpui_kit::*;
use std::sync::Arc;
use uuid::Uuid;

pub(super) fn is_visual(file: &Attachment) -> bool {
    file.media_type == else_visual::MEDIA_TYPE
}

/// What a render depends on. A visual renders again when it changes.
#[derive(Clone, PartialEq)]
struct Key {
    width: u32,
    scale: u32,
    theme: else_visual::Theme,
}

enum Source {
    Loading { _task: Task<()> },
    Ready(Arc<str>),
    Failed(String),
}

struct Shown {
    key: Key,
    image: Arc<RenderImage>,
    height: u32,
}

struct Visual {
    file: Attachment,
    source: Source,
    shown: Option<Result<Shown, String>>,
    rendering: Option<(Key, Task<()>)>,
}

pub(super) struct MessageVisuals {
    client: Arc<Client>,
    bot: Uuid,
    files: Vec<Visual>,
    /// The column width in CSS pixels, known after the first layout.
    width: Option<u32>,
}

impl MessageVisuals {
    fn new(client: Arc<Client>, bot: Uuid) -> Self {
        Self {
            client,
            bot,
            files: Vec::new(),
            width: None,
        }
    }

    pub(super) fn sync(&mut self, parts: &[MessagePart], cx: &mut Context<Self>) {
        let mut files: Vec<&Attachment> = Vec::new();
        for part in parts {
            if let MessagePart::File { file } = part
                && is_visual(file)
                && !files.iter().any(|saved| saved.id == file.id)
            {
                files.push(file);
            }
        }
        if self
            .files
            .iter()
            .map(|visual| &visual.file)
            .eq(files.iter().copied())
        {
            return;
        }
        let mut previous = std::mem::take(&mut self.files);
        self.files = files
            .into_iter()
            .map(
                |file| match previous.iter().position(|saved| saved.file == *file) {
                    Some(index) => previous.remove(index),
                    None => self.load(file.clone(), cx),
                },
            )
            .collect();
        cx.notify();
    }

    fn load(&self, file: Attachment, cx: &mut Context<Self>) -> Visual {
        let (client, bot, id) = (self.client.clone(), self.bot, file.id);
        let task = cx.spawn(async move |this, cx| {
            let source = match client.attachment_bytes(bot, id).await {
                Ok(bytes) => String::from_utf8(bytes)
                    .map(Arc::from)
                    .map_err(|_| "The visual is not UTF-8 text.".to_owned()),
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(visual) = this.files.iter_mut().find(|v| v.file.id == id) {
                    visual.source = match source {
                        Ok(html) => Source::Ready(html),
                        Err(error) => Source::Failed(error),
                    };
                    cx.notify();
                }
            });
        });
        Visual {
            file,
            source: Source::Loading { _task: task },
            shown: None,
            rendering: None,
        }
    }

    /// Start a render for each visual whose last one used another key and
    /// that is not already rendering for this key.
    fn render_stale(&mut self, key: &Key, cx: &mut Context<Self>) {
        for visual in &mut self.files {
            let Source::Ready(html) = &visual.source else {
                continue;
            };
            let current = match &visual.shown {
                Some(Ok(shown)) => shown.key == *key,
                Some(Err(_)) => visual.rendering.is_none(),
                None => false,
            };
            if current || visual.rendering.as_ref().is_some_and(|(k, _)| k == key) {
                continue;
            }
            let (html, render_key, id) = (html.clone(), key.clone(), visual.file.id);
            let task = cx.spawn(async move |this, cx| {
                let job_key = render_key.clone();
                let result = cx
                    .background_executor()
                    .spawn(async move { render(&html, &job_key) })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    let Some(visual) = this.files.iter_mut().find(|v| v.file.id == id) else {
                        return;
                    };
                    if visual
                        .rendering
                        .as_ref()
                        .is_some_and(|(k, _)| *k == render_key)
                    {
                        visual.rendering = None;
                    }
                    visual.shown = Some(result.map(|(image, height)| Shown {
                        key: render_key,
                        image,
                        height,
                    }));
                    cx.notify();
                });
            });
            visual.rendering = Some((key.clone(), task));
        }
    }
}

fn render(html: &str, key: &Key) -> Result<(Arc<RenderImage>, u32), String> {
    let scale = key.scale as f32 / 100.0;
    let rendered =
        else_visual::render(html, key.width, scale, &key.theme).map_err(|e| e.to_string())?;
    let height = rendered.content_height.min(else_visual::MAX_HEIGHT);
    let mut bgra = image::RgbaImage::from_raw(rendered.width, rendered.height, rendered.rgba)
        .ok_or("The render has the wrong size.")?;
    for pixel in bgra.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Ok((
        Arc::new(RenderImage::new([image::Frame::new(bgra)])),
        height,
    ))
}

/// The app theme as the colors a visual reads.
fn visual_theme(theme: &Theme) -> else_visual::Theme {
    use else_visual::Color;
    let rgba = |color: Hsla| {
        let Rgba { r, g, b, a } = color.into();
        [r, g, b, a].map(|channel| (channel.clamp(0.0, 1.0) * 255.0).round() as u8)
    };
    else_visual::Theme::new(theme.mode.is_dark(), f32::from(theme.radius), |color| {
        rgba(match color {
            Color::Background => theme.background,
            Color::Foreground => theme.foreground,
            Color::Muted => theme.muted,
            Color::MutedForeground => theme.muted_foreground,
            Color::Card => theme.secondary,
            Color::Border => theme.border,
            Color::Primary => theme.primary,
            Color::PrimaryForeground => theme.primary_foreground,
            Color::Accent => theme.accent,
            Color::Success => theme.success,
            Color::Warning => theme.warning,
            Color::Danger => theme.danger,
            Color::Info => theme.info,
            Color::Chart1 => theme.chart_1,
            Color::Chart2 => theme.chart_2,
            Color::Chart3 => theme.chart_3,
            Color::Chart4 => theme.chart_4,
            Color::Chart5 => theme.chart_5,
        })
    })
}

impl Render for MessageVisuals {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        if let Some(width) = self.width {
            let key = Key {
                width,
                scale: (window.scale_factor() * 100.0).round() as u32,
                theme: visual_theme(&theme),
            };
            self.render_stale(&key, cx);
        }
        let entity = cx.entity().downgrade();
        let measure = canvas(
            move |bounds, _, cx| {
                let width = f32::from(bounds.size.width).floor() as u32;
                let _ = entity.update(cx, |this, cx| {
                    if width > 0 && this.width != Some(width) {
                        this.width = Some(width);
                        cx.notify();
                    }
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        v_flex()
            .w_full()
            .relative()
            .gap_2()
            .child(measure)
            .children(self.files.iter().map(|visual| {
                let frame = div()
                    .id(SharedString::from(format!("visual-{}", visual.file.id)))
                    .w_full()
                    .overflow_hidden();
                match (&visual.shown, &visual.source) {
                    (Some(Ok(shown)), _) => frame
                        .h(px(shown.height as f32))
                        .child(
                            img(ImageSource::Render(shown.image.clone()))
                                .w(px(shown.key.width as f32))
                                .h(px(shown.height as f32)),
                        )
                        .into_any_element(),
                    (Some(Err(error)), _) | (None, Source::Failed(error)) => frame
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("Could not show {}: {error}", visual.file.name))
                        .into_any_element(),
                    _ => frame
                        .h(px(120.))
                        .rounded(theme.radius_lg)
                        .bg(theme.secondary)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(IconName::LoaderCircle).small())
                        .into_any_element(),
                }
            }))
    }
}

impl ElseApp {
    pub(super) fn message_visuals(
        client: Arc<Client>,
        key: RowKey,
        bot: Uuid,
        parts: &[MessagePart],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<MessageVisuals>, Subscription) {
        let visuals = cx.new(|cx| {
            let mut visuals = MessageVisuals::new(client, bot);
            visuals.sync(parts, cx);
            visuals
        });
        let observe = cx.observe_in(&visuals, window, move |app, _, _, cx| {
            app.remeasure_row(key, cx)
        });
        (visuals, observe)
    }
}
