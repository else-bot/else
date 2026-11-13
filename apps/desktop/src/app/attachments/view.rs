use super::{DraftFile, UploadState};
use crate::app::ElseApp;
use crate::attachments::{Attachment, byte_size};
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, InlineTokenContext};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::sync::Arc;
use uuid::Uuid;

pub(super) enum Preview {
    Image(Arc<Image>),
    Text(String),
    Binary,
}

impl Preview {
    pub(super) fn from_bytes(file: &Attachment, bytes: Vec<u8>) -> Self {
        if let Some(format) = ImageFormat::from_mime_type(&file.media_type) {
            return Self::Image(Arc::new(Image::from_bytes(format, bytes)));
        }
        match std::str::from_utf8(&bytes) {
            Ok(text) if !text.contains('\0') => Self::Text(text.chars().take(64 * 1024).collect()),
            _ => Self::Binary,
        }
    }
}

pub(super) fn show_preview(
    file: Attachment,
    preview: Preview,
    bytes: Option<Arc<Vec<u8>>>,
    window: &mut Window,
    cx: &mut App,
) {
    let code = match &preview {
        Preview::Text(text) => {
            let language = std::path::Path::new(&file.name)
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or("text")
                .to_ascii_lowercase();
            let text = text.clone();
            Some(cx.new(|cx| {
                EditorState::new(window, cx)
                    .language(language)
                    .default_value(text)
            }))
        }
        _ => None,
    };
    window.open_dialog(cx, move |dialog, _, _| {
        let content = match (&preview, &code) {
            (_, Some(code)) => Editor::new(code)
                .readonly(true)
                .h(px(520.))
                .aria_label(file.name.clone())
                .into_any_element(),
            (Preview::Image(image), _) => div()
                .w_full()
                .child(
                    img(image.clone())
                        .w_full()
                        .max_h(px(440.))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            _ => div()
                .text_sm()
                .child("Preview is not available for this file type.")
                .into_any_element(),
        };
        dialog
            .when(code.is_some(), |dialog| dialog.w(px(880.)))
            .title(file.name.clone())
            .child(
                div()
                    .text_xs()
                    .child(format!("{} · {}", file.media_type, byte_size(file.size))),
            )
            .child(content)
            .when_some(bytes.clone(), |dialog, bytes| {
                let file = file.clone();
                dialog.footer(
                    h_flex().child(
                        Button::new("save-file")
                            .focus_ring(false)
                            .cursor_pointer()
                            .label("Save to disk")
                            .on_click(move |_, window, cx| {
                                save(&file.name, bytes.clone(), window, cx)
                            }),
                    ),
                )
            })
    });
}

fn save(name: &str, bytes: Arc<Vec<u8>>, window: &mut Window, cx: &mut App) {
    let directory = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let prompt = cx.prompt_for_new_path(&directory, Some(name));
    let name = name.to_owned();
    window
        .spawn(cx, async move |cx| {
            let result = match prompt.await {
                Ok(Ok(Some(path))) => {
                    cx.background_executor()
                        .spawn(async move {
                            std::fs::write(path, bytes.as_slice()).map_err(|e| e.to_string())
                        })
                        .await
                }
                Ok(Ok(None)) => return,
                Ok(Err(error)) => Err(error.to_string()),
                Err(error) => Err(error.to_string()),
            };
            if let Err(error) = result {
                let _ = cx.update(|window, cx| {
                    window.push_notification(format!("Could not save {name}: {error}"), cx)
                });
            }
        })
        .detach();
}

pub(in crate::app) fn chip(
    token: &InlineTokenContext,
    file: &Attachment,
    image: Option<Arc<Image>>,
    detail: String,
    owner: Option<WeakEntity<ElseApp>>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let id = file.id;
    let range = token.range();
    let mut chip = h_flex()
        .id(SharedString::from(format!("file-{id}-{}", range.start)))
        .cursor_pointer()
        .gap_1()
        .px_1()
        .h(token.line_height())
        .max_w(token.available_width())
        .rounded(theme.radius)
        .border_1()
        .border_color(if token.is_selected() {
            theme.ring
        } else {
            theme.border
        })
        .bg(if token.is_selected() {
            theme.selection
        } else {
            theme.secondary
        })
        .text_xs();
    chip = match image {
        Some(image) => chip.child(
            img(image)
                .size_4()
                .rounded(px(3.))
                .object_fit(ObjectFit::Cover),
        ),
        None => chip.child(
            Icon::new(IconName::FileText)
                .size_3()
                .text_color(theme.muted_foreground),
        ),
    };
    chip = chip
        .child(
            div()
                .min_w_0()
                .max_w(rems(14.))
                .truncate()
                .child(file.name.clone()),
        )
        .child(div().text_color(theme.muted_foreground).child(detail));
    if let Some(owner) = owner {
        chip = chip.child(
            Button::new("remove-file")
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .accessibility_label(format!("Remove {}", file.name))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    let _ = owner.update(cx, |app, cx| {
                        app.composer.update(cx, |state, cx| {
                            let value = state.value();
                            let current = state
                                .tokens()
                                .iter()
                                .find(|span| {
                                    span.range() == range
                                        && crate::attachments::file_id(span.token().id())
                                            == Some(id)
                                })
                                .map(|span| span.range());
                            if let Some(range) = current {
                                let start = value[..range.start].encode_utf16().count();
                                let end = value[..range.end].encode_utf16().count();
                                state.replace_text_in_range(Some(start..end), "", window, cx);
                            }
                        });
                        cx.notify();
                    });
                }),
        );
    }
    chip.into_any_element()
}

pub(in crate::app) fn draft_chip(
    token: &InlineTokenContext,
    files: &std::collections::HashMap<Uuid, DraftFile>,
    owner: WeakEntity<ElseApp>,
    cx: &App,
) -> AnyElement {
    let Some((id, file)) = crate::attachments::file_id(token.token().id())
        .and_then(|id| files.get(&id).map(|file| (id, file)))
    else {
        return div()
            .child(token.token().label().clone())
            .into_any_element();
    };
    let detail = match &file.state {
        UploadState::Ready { file } => byte_size(file.size),
        UploadState::Preparing { .. } | UploadState::Uploading { .. } => "Uploading…".into(),
        UploadState::Failed { .. } => "Failed".into(),
        UploadState::Unconfirmed { .. } => "Unconfirmed".into(),
    };
    chip(
        token,
        &file.preview_metadata(id),
        file.image.clone(),
        detail,
        Some(owner),
        cx,
    )
}
