mod viewer;

pub(super) use viewer::ImageViewer;

use super::{ElseApp, RowKey};
use crate::api::Client;
use crate::attachments::{Attachment, MessagePart};
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::io::Cursor;
use std::sync::Arc;
use uuid::Uuid;

pub(super) fn is_image(file: &Attachment) -> bool {
    matches!(
        file.media_type.as_str(),
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    )
}

pub(super) fn image_files(parts: &[MessagePart]) -> Vec<Attachment> {
    let mut files = Vec::new();
    for part in parts {
        if let MessagePart::File { file } = part
            && is_image(file)
            && !files.iter().any(|saved: &Attachment| saved.id == file.id)
        {
            files.push(file.clone());
        }
    }
    files
}

pub(super) fn without_images(source: &str, parts: &[MessagePart]) -> (String, Vec<MessagePart>) {
    if !parts
        .iter()
        .any(|part| matches!(part, MessagePart::File { file } if is_image(file)))
    {
        return (source.to_owned(), parts.to_vec());
    }
    let parts: Vec<_> = parts
        .iter()
        .filter(|part| !matches!(part, MessagePart::File { file } if is_image(file)))
        .cloned()
        .collect();
    let text = parts
        .iter()
        .map(|part| match part {
            MessagePart::Text { text } => text.clone(),
            MessagePart::File { file } => file.fallback(),
        })
        .collect();
    (text, parts)
}

enum ImageState {
    Loading,
    Ready(LoadedImage),
    Failed(String),
}

struct LoadedImage {
    original: Arc<Image>,
    display: Arc<Image>,
    thumbnail: Arc<Image>,
    size: (u32, u32),
}

struct GalleryImage {
    file: Attachment,
    state: ImageState,
    _load: Option<Task<()>>,
}

pub(super) struct MessageImages {
    client: Arc<Client>,
    bot: Uuid,
    owner: WeakEntity<ElseApp>,
    files: Vec<GalleryImage>,
    notice: Option<String>,
}

impl MessageImages {
    pub(super) fn new(client: Arc<Client>, bot: Uuid, owner: WeakEntity<ElseApp>) -> Self {
        Self {
            client,
            bot,
            owner,
            files: Vec::new(),
            notice: None,
        }
    }

    pub(super) fn sync(&mut self, parts: &[MessagePart], cx: &mut Context<Self>) {
        let files = image_files(parts);
        if self.files.iter().map(|image| &image.file).eq(files.iter()) {
            return;
        }
        let mut previous = std::mem::take(&mut self.files);
        self.files = files
            .into_iter()
            .map(|file| {
                if let Some(index) = previous.iter().position(|saved| saved.file == file) {
                    previous.remove(index)
                } else {
                    GalleryImage {
                        file,
                        state: ImageState::Loading,
                        _load: None,
                    }
                }
            })
            .collect();
        cx.notify();
    }

    fn load(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let file = self.files[index].file.clone();
        let (client, bot) = (self.client.clone(), self.bot);
        self.files[index].state = ImageState::Loading;
        self.files[index]._load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = match client.attachment_bytes(bot, file.id).await {
                Ok(bytes) => {
                    cx.background_executor()
                        .spawn(async move { load_image(bytes) })
                        .await
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = this.update(cx, |gallery, cx| {
                if let Some(image) = gallery
                    .files
                    .iter_mut()
                    .find(|image| image.file.id == file.id)
                {
                    image.state = match result {
                        Ok(image) => ImageState::Ready(image),
                        Err(error) => ImageState::Failed(error),
                    };
                    image._load = None;
                    cx.notify();
                }
            });
        }));
    }

    fn open(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.notice = None;
        cx.notify();
        let gallery = cx.entity();
        let _ = self.owner.update(cx, |app, cx| {
            let owner = cx.entity().downgrade();
            app.image_viewer =
                Some(cx.new(|cx| ImageViewer::new(gallery, index, owner, window, cx)));
            cx.notify();
        });
    }

    fn menu(gallery: WeakEntity<Self>, index: usize, ready: bool, menu: PopupMenu) -> PopupMenu {
        let copy = gallery.clone();
        menu.item(
            PopupMenuItem::new("Save image to disk")
                .disabled(!ready)
                .icon(IconName::Download)
                .on_click(move |_, window, cx| {
                    let _ = gallery.update(cx, |gallery, cx| gallery.save(index, window, cx));
                }),
        )
        .item(
            PopupMenuItem::new("Copy image to clipboard")
                .disabled(!ready)
                .icon(IconName::Copy)
                .on_click(move |_, window, cx| {
                    let _ = copy.update(cx, |gallery, cx| gallery.copy(index, window, cx));
                }),
        )
    }

    fn save(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let ImageState::Ready(loaded) = &self.files[index].state else {
            return;
        };
        let image = loaded.original.clone();
        let directory = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let prompt = cx.prompt_for_new_path(&directory, Some(&self.files[index].file.name));
        self.notice = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = match prompt.await {
                Ok(Ok(Some(path))) => {
                    cx.background_executor()
                        .spawn(async move {
                            std::fs::write(path, &image.bytes).map_err(|error| error.to_string())
                        })
                        .await
                }
                Ok(Ok(None)) => return,
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            if let Err(error) = result {
                let _ = this.update(cx, |gallery, cx| {
                    gallery.notice = Some(format!("Could not save image: {error}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn copy(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let ImageState::Ready(loaded) = &self.files[index].state else {
            return;
        };
        #[cfg(not(target_os = "linux"))]
        {
            let image = loaded.display.clone();
            self.notice = None;
            cx.notify();
            cx.spawn_in(window, async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move { clipboard_png(&image) })
                    .await;
                let _ = this.update(cx, |gallery, cx| {
                    gallery.notice = Some(match result {
                        Ok(bytes) => {
                            cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(
                                ImageFormat::Png,
                                bytes,
                            )));
                            "Image copied to clipboard.".into()
                        }
                        Err(error) => format!("Could not copy image: {error}"),
                    });
                    cx.notify();
                });
            })
            .detach();
        }
        #[cfg(target_os = "linux")]
        {
            let image = loaded.original.clone();
            self.notice = None;
            cx.notify();
            cx.spawn_in(window, async move |this, cx| {
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};
                        let mut sources = vec![MimeSource {
                            source: Source::Bytes(image.bytes.clone().into_boxed_slice()),
                            mime_type: MimeType::Specific(image.format.mime_type().to_owned()),
                        }];
                        if image.format != ImageFormat::Png {
                            sources.push(MimeSource {
                                source: Source::Bytes(clipboard_png(&image)?.into_boxed_slice()),
                                mime_type: MimeType::Specific("image/png".into()),
                            });
                        }
                        Options::new()
                            .copy_multi(sources)
                            .map_err(|error| error.to_string())
                    })
                    .await;
                let _ = this.update(cx, |gallery, cx| {
                    gallery.notice = Some(match result {
                        Ok(()) => "Image copied to clipboard.".into(),
                        Err(error) => format!("Could not copy image: {error}"),
                    });
                    cx.notify();
                });
            })
            .detach();
        }
    }
}

fn clipboard_png(image: &Image) -> Result<Vec<u8>, String> {
    if image.format == ImageFormat::Png {
        return Ok(image.bytes.clone());
    }
    let decoded = decode_image(&image.bytes)?;
    let mut png = Cursor::new(Vec::new());
    decoded
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(png.into_inner())
}

fn decode_image(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    reader.decode().map_err(|error| error.to_string())
}

fn load_image(bytes: Vec<u8>) -> Result<LoadedImage, String> {
    let detected = image::guess_format(&bytes).map_err(|error| error.to_string())?;
    let format =
        ImageFormat::from_mime_type(detected.to_mime_type()).ok_or("Unsupported image format.")?;
    let decoded = decode_image(&bytes)?;
    let size = (decoded.width(), decoded.height());
    let mut thumbnail = Cursor::new(Vec::new());
    decoded
        .thumbnail(200, 156)
        .write_to(&mut thumbnail, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let original = Arc::new(Image::from_bytes(format, bytes));
    let display = if format == ImageFormat::Gif {
        let mut preview = Cursor::new(Vec::new());
        decoded
            .write_to(&mut preview, image::ImageFormat::Png)
            .map_err(|error| error.to_string())?;
        Arc::new(Image::from_bytes(ImageFormat::Png, preview.into_inner()))
    } else {
        original.clone()
    };
    Ok(LoadedImage {
        original,
        display,
        thumbnail: Arc::new(Image::from_bytes(ImageFormat::Png, thumbnail.into_inner())),
        size,
    })
}

impl Render for MessageImages {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        for index in 0..self.files.len() {
            if matches!(self.files[index].state, ImageState::Loading)
                && self.files[index]._load.is_none()
            {
                self.load(index, window, cx);
            }
        }
        let theme = cx.theme().clone();
        let thumbnails = h_flex()
            .w_full()
            .items_start()
            .flex_wrap()
            .gap_2()
            .children(self.files.iter().enumerate().map(|(index, image)| {
                let gallery = cx.entity().downgrade();
                let ready = matches!(image.state, ImageState::Ready(_));
                let thumbnail = Button::new(SharedString::from(format!("image-{}", image.file.id)))
                    .focus_ring(false)
                    .accessibility_label(format!("Open image {}", image.file.name))
                    .ghost()
                    .p_0()
                    .border_0()
                    .cursor_pointer()
                    .rounded(theme.radius_lg)
                    .overflow_hidden()
                    .bg(theme.secondary)
                    .when(!matches!(image.state, ImageState::Ready(_)), |el| {
                        el.w(px(120.)).h(px(120.))
                    })
                    .when_some(
                        match &image.state {
                            ImageState::Ready(loaded) => {
                                Some((loaded.thumbnail.clone(), loaded.size.0, loaded.size.1))
                            }
                            _ => None,
                        },
                        |el, (image, width, height)| {
                            let scale = (156. / height as f32).min(200. / width as f32).min(1.);
                            el.w(px(width as f32 * scale))
                                .h(px(height as f32 * scale))
                                .child(img(image).size_full().object_fit(ObjectFit::Contain))
                        },
                    )
                    .when(matches!(image.state, ImageState::Loading), |el| {
                        el.flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::LoaderCircle).small())
                    })
                    .when(matches!(image.state, ImageState::Failed(_)), |el| {
                        el.flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::TriangleAlert).small())
                    })
                    .on_click(cx.listener(move |gallery, _, window, cx| {
                        cx.stop_propagation();
                        gallery.open(index, window, cx);
                    }));
                thumbnail
                    .context_menu(move |menu, _, _| Self::menu(gallery.clone(), index, ready, menu))
            }));
        v_flex()
            .w_full()
            .gap_1()
            .child(thumbnails)
            .children(self.notice.as_ref().map(|notice| {
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(notice.clone())
            }))
    }
}

impl ElseApp {
    pub(super) fn message_images(
        client: Arc<Client>,
        key: RowKey,
        bot: Uuid,
        parts: &[MessagePart],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> (Entity<MessageImages>, Subscription) {
        let owner = cx.entity().downgrade();
        let images = cx.new(|cx| {
            let mut gallery = MessageImages::new(client, bot, owner);
            gallery.sync(parts, cx);
            gallery
        });
        let observe = cx.observe_in(&images, window, move |app, _, _, cx| {
            app.remeasure_row(key, cx)
        });
        (images, observe)
    }
}

#[cfg(test)]
mod tests {
    use super::{image_files, load_image, without_images};
    use crate::attachments::{Attachment, MessagePart};
    use uuid::Uuid;

    #[test]
    fn gallery_preserves_order_and_text_and_keeps_other_files() {
        let file = |name: &str, mime: &str| Attachment {
            id: Uuid::new_v4(),
            name: name.into(),
            media_type: mime.into(),
            size: 3,
        };
        let first = file("one.png", "image/png");
        let second = file("two.jpg", "image/jpeg");
        let pdf = file("report.pdf", "application/pdf");
        let parts = vec![
            MessagePart::Text {
                text: "Before ".into(),
            },
            MessagePart::File {
                file: first.clone(),
            },
            MessagePart::File { file: pdf.clone() },
            MessagePart::File {
                file: second.clone(),
            },
            MessagePart::File {
                file: first.clone(),
            },
            MessagePart::Text {
                text: " after".into(),
            },
        ];
        assert_eq!(image_files(&parts), [first, second]);
        let (text, remaining) = without_images("unused canonical source", &parts);
        assert_eq!(text, "Before [File: report.pdf] after");
        assert_eq!(remaining.len(), 3);
        assert_eq!(
            crate::attachments::input_content(&text, &remaining)
                .tokens()
                .len(),
            1
        );
    }

    #[test]
    fn previews_keep_original_bytes_for_saving_and_copying() {
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(800, 600)
            .write_to(&mut encoded, image::ImageFormat::Jpeg)
            .unwrap();
        let original = encoded.into_inner();
        let loaded = load_image(original.clone()).unwrap();
        assert_eq!(loaded.original.bytes, original);
        assert_eq!(loaded.size, (800, 600));
        assert_eq!(loaded.original.format, gpui_kit::ImageFormat::Jpeg);
        let thumbnail = image::load_from_memory(&loaded.thumbnail.bytes).unwrap();
        assert_eq!((thumbnail.width(), thumbnail.height()), (200, 150));
    }

    #[test]
    fn animated_gifs_preview_the_first_frame_and_keep_the_original_file() {
        let first = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 255]));
        let second = image::RgbaImage::from_pixel(2, 2, image::Rgba([0, 0, 255, 255]));
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            encoder
                .encode_frames([image::Frame::new(first.clone()), image::Frame::new(second)])
                .unwrap();
        }
        let loaded = load_image(bytes.clone()).unwrap();
        assert_eq!(loaded.original.bytes, bytes);
        assert_eq!(loaded.display.format, gpui_kit::ImageFormat::Png);
        assert_eq!(
            image::load_from_memory(&loaded.display.bytes)
                .unwrap()
                .into_rgba8(),
            first
        );
    }

    #[test]
    fn ordinary_camera_photos_can_be_previewed() {
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(6000, 4000)
            .write_to(&mut encoded, image::ImageFormat::Jpeg)
            .unwrap();
        let loaded = load_image(encoded.into_inner()).unwrap();
        assert_eq!(loaded.size, (6000, 4000));
    }

    #[test]
    fn corrupt_images_report_an_error_instead_of_an_empty_preview() {
        assert!(load_image(b"not an image".to_vec()).is_err());
    }
}
