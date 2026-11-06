use super::{ImageState, MessageImages};
use crate::app::ElseApp;
use gpui_kit::assets::IconName;
use gpui_kit::base::FocusTrapElement as _;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

pub(in crate::app) struct ImageViewer {
    gallery: Entity<MessageImages>,
    selected: usize,
    owner: WeakEntity<ElseApp>,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    _observe: Subscription,
}

impl ImageViewer {
    pub(super) fn new(
        gallery: Entity<MessageImages>,
        selected: usize,
        owner: WeakEntity<ElseApp>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let previous_focus = window.focused(cx);
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let observe = cx.observe(&gallery, |_, _, cx| cx.notify());
        Self {
            gallery,
            selected,
            owner,
            focus,
            previous_focus,
            _observe: observe,
        }
    }

    fn close(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(focus) = &self.previous_focus {
            focus.focus(window, cx);
        }
        let _ = self.owner.update(cx, |app, cx| {
            app.image_viewer = None;
            cx.notify();
        });
    }

    fn step(&mut self, next: bool, cx: &mut Context<Self>) {
        let count = self.gallery.read(cx).files.len();
        if count == 0 {
            return;
        }
        self.selected = if next {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
        cx.notify();
    }
}

impl Render for ImageViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let gallery = self.gallery.read(cx);
        if gallery.files.is_empty() {
            self.close(window, cx);
            return div().into_any_element();
        }
        self.selected = self.selected.min(gallery.files.len() - 1);
        let current = &gallery.files[self.selected];
        let ready = matches!(current.state, ImageState::Ready(_));
        let theme = cx.theme().clone();
        let index = self.selected;
        let image_menu = self.gallery.downgrade();
        let stage = div()
            .id("full-size-image")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .when_some(
                match &current.state {
                    ImageState::Ready(loaded) => Some(loaded.display.clone()),
                    _ => None,
                },
                |el, image| el.child(img(image).size_full().object_fit(ObjectFit::Contain)),
            )
            .when(matches!(current.state, ImageState::Loading), |el| {
                el.child("Loading image…")
            })
            .when_some(
                match &current.state {
                    ImageState::Failed(error) => Some(error.clone()),
                    _ => None,
                },
                |el, error| {
                    el.child(
                        v_flex()
                            .gap_3()
                            .items_center()
                            .max_w(px(400.))
                            .child("Could not load this image")
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(error),
                            )
                            .child(
                                Button::new("retry-image")
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .label("Retry")
                                    .on_click(cx.listener(move |viewer, _, window, cx| {
                                        viewer.gallery.update(cx, |gallery, cx| {
                                            gallery.load(index, window, cx)
                                        });
                                    })),
                            ),
                    )
                },
            )
            .context_menu(move |menu, _, _| {
                MessageImages::menu(image_menu.clone(), index, ready, menu)
            });
        let strip = h_flex()
            .id("image-filmstrip")
            .w_full()
            .overflow_x_scroll()
            .justify_center()
            .gap_2()
            .px_4()
            .py_3()
            .children(gallery.files.iter().enumerate().map(|(i, file)| {
                Button::new(SharedString::from(format!("filmstrip-{}", file.file.id)))
                    .ghost()
                    .focus_ring(false)
                    .border_0()
                    .accessibility_label(format!(
                        "View image {} of {}: {}",
                        i + 1,
                        gallery.files.len(),
                        file.file.name
                    ))
                    .cursor_pointer()
                    .flex_shrink_0()
                    .size(px(48.))
                    .p_1()
                    .rounded(theme.radius)
                    .when(i == self.selected, |el| {
                        el.bg(theme.selection)
                            .border_1()
                            .border_color(theme.primary)
                    })
                    .when_some(
                        match &file.state {
                            ImageState::Ready(loaded) => Some(loaded.thumbnail.clone()),
                            _ => None,
                        },
                        |el, image| {
                            el.child(
                                img(image)
                                    .size_full()
                                    .object_fit(ObjectFit::Contain)
                                    .rounded(px(3.)),
                            )
                        },
                    )
                    .on_click(cx.listener(move |viewer, _, _, cx| {
                        viewer.selected = i;
                        cx.notify();
                    }))
            }));
        let detail = match &current.state {
            ImageState::Ready(loaded) => {
                let (w, h) = loaded.size;
                format!(
                    "{} of {} · {w} × {h} · {}",
                    index + 1,
                    gallery.files.len(),
                    current.file.media_type
                )
            }
            _ => format!("{} of {}", index + 1, gallery.files.len()),
        };
        let height = (window.viewport_size().height - px(188.)).max(px(120.));
        div()
            .id("image-viewer")
            .role(Role::Dialog)
            .aria_label("Image viewer")
            .absolute()
            .inset_0()
            .size_full()
            .bg(black().opacity(0.9))
            .text_color(theme.foreground)
            .track_focus(&self.focus)
            .focus_trap("image-viewer", &self.focus)
            .on_key_down(cx.listener(|viewer, event: &KeyDownEvent, window, cx| {
                match event.keystroke.key.as_str() {
                    "escape" => viewer.close(window, cx),
                    "left" => viewer.step(false, cx),
                    "right" => viewer.step(true, cx),
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .flex()
            .flex_col()
            .child(
                h_flex()
                    .h(px(50.))
                    .px_4()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(current.file.name.clone()),
                    )
                    .child(
                        Button::new("close-image-viewer")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .icon(IconName::Close)
                            .label("Close  Esc")
                            .accessibility_label("Close image viewer")
                            .on_click(
                                cx.listener(|viewer, _, window, cx| viewer.close(window, cx)),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .h(height)
                    .w_full()
                    .gap_2()
                    .px_3()
                    .items_center()
                    .children((gallery.files.len() > 1).then(|| {
                        Button::new("previous-image")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .icon(IconName::ChevronLeft)
                            .accessibility_label("Previous image")
                            .on_click(cx.listener(|viewer, _, _, cx| viewer.step(false, cx)))
                    }))
                    .child(stage)
                    .children((gallery.files.len() > 1).then(|| {
                        Button::new("next-image")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .icon(IconName::ChevronRight)
                            .accessibility_label("Next image")
                            .on_click(cx.listener(|viewer, _, _, cx| viewer.step(true, cx)))
                    })),
            )
            .children((gallery.files.len() > 1).then_some(strip))
            .children(
                gallery
                    .notice
                    .as_ref()
                    .map(|notice| div().px_4().py_1().text_xs().child(notice.clone())),
            )
            .child(
                h_flex()
                    .px_4()
                    .py_3()
                    .gap_2()
                    .bg(theme.sidebar)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(div().text_sm().truncate().child(current.file.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(detail),
                            ),
                    )
                    .child(
                        Button::new("save-viewer-image")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .disabled(!ready)
                            .icon(IconName::Download)
                            .label("Save image")
                            .on_click(cx.listener(move |viewer, _, window, cx| {
                                viewer
                                    .gallery
                                    .update(cx, |gallery, cx| gallery.save(index, window, cx));
                            })),
                    )
                    .child(
                        Button::new("copy-viewer-image")
                            .focus_ring(false)
                            .cursor_pointer()
                            .ghost()
                            .disabled(!ready)
                            .icon(IconName::Copy)
                            .label("Copy image")
                            .on_click(cx.listener(move |viewer, _, window, cx| {
                                viewer
                                    .gallery
                                    .update(cx, |gallery, cx| gallery.copy(index, window, cx));
                            })),
                    ),
            )
            .into_any_element()
    }
}
