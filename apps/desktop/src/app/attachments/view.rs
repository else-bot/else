use super::{DraftFile, UploadState};
use crate::app::ElseApp;
use crate::attachments::{Attachment, file_size};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::InlineTokenContext;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _, h_flex};
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

pub(super) fn show_preview(file: Attachment, preview: Preview, window: &mut Window, cx: &mut App) {
    window.open_dialog(cx, move |dialog, _, _| {
        let content = match &preview {
            Preview::Image(image) => div()
                .w_full()
                .child(
                    img(image.clone())
                        .w_full()
                        .max_h(px(440.))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element(),
            Preview::Text(text) => div()
                .id("file-preview-text")
                .max_h(px(440.))
                .overflow_y_scroll()
                .text_sm()
                .child(text.clone())
                .into_any_element(),
            Preview::Binary => div()
                .text_sm()
                .child("Preview is not available for this file type.")
                .into_any_element(),
        };
        dialog
            .title(file.name.clone())
            .child(
                div()
                    .text_xs()
                    .child(format!("{} · {}", file.media_type, file_size(file.size))),
            )
            .child(content)
    });
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
    let Some(file) = crate::attachments::file_id(token.token().id()).and_then(|id| files.get(&id))
    else {
        return div()
            .child(token.token().label().clone())
            .into_any_element();
    };
    let detail = match &file.state {
        UploadState::Ready => file_size(file.file.size),
        UploadState::Uploading => "Uploading…".into(),
        UploadState::Failed(_) => "Failed".into(),
    };
    chip(
        token,
        &file.file,
        file.image.clone(),
        detail,
        Some(owner),
        cx,
    )
}
