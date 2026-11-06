use super::ElseApp;
use gpui_kit::base::{HandleEdge, InteractiveElementExt as _, resize_handle};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::*;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) enum SidebarEdge {
    Left,
    Right,
}

#[derive(Clone)]
pub(super) struct SidebarResize {
    edge: SidebarEdge,
}

impl Render for SidebarResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

#[derive(Clone)]
pub(super) struct CategoryDrag {
    pub id: Uuid,
    pub name: SharedString,
}

impl Render for CategoryDrag {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .shadow_md()
            .child(self.name.clone())
    }
}

impl ElseApp {
    pub(super) fn sidebar_layout(&self) -> crate::sidebar::Layout<'_> {
        crate::sidebar::Layout::new(&self.categories, &self.bots, self.default_bot_id)
            .with_preferences(&self.preferences)
    }

    pub(super) fn toggle_category(&mut self, id: Option<Uuid>, cx: &mut Context<Self>) {
        if !self.preferences.collapsed.remove(&id) {
            self.preferences.collapsed.insert(id);
        }
        self.save_preferences(cx);
    }

    pub(super) fn reorder_category(&mut self, dragged: Uuid, target: Uuid, cx: &mut Context<Self>) {
        let ids = self
            .sidebar_layout()
            .groups
            .iter()
            .filter_map(|g| g.category.map(|c| c.id))
            .collect();
        self.preferences.reorder(ids, dragged, target);
        self.save_preferences(cx);
    }

    pub(super) fn sidebar_handle(
        &self,
        edge: SidebarEdge,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (id, inside) = match edge {
            SidebarEdge::Left => ("resize-bots", HandleEdge::Trailing),
            SidebarEdge::Right => ("resize-details", HandleEdge::Leading),
        };
        resize_handle(id, Axis::Horizontal)
            .inside(inside)
            .on_drag(SidebarResize { edge }, |drag, _, _, cx| {
                cx.new(|_| drag.clone())
            })
            .on_double_click(cx.listener(move |app, _, _, cx| {
                match edge {
                    SidebarEdge::Left => app.preferences.sidebar_width = 240.,
                    SidebarEdge::Right => app.preferences.details_width = 336.,
                }
                app.save_preferences(cx);
            }))
    }

    pub(super) fn resize_sidebar(
        &mut self,
        event: &DragMoveEvent<SidebarResize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let width = f32::from(window.viewport_size().width);
        let x = f32::from(event.event.position.x);
        match event.drag(cx).edge {
            SidebarEdge::Left => {
                let right = if self.settings_shown() {
                    self.preferences.details_width
                } else {
                    0.
                };
                self.preferences.sidebar_width =
                    x.clamp(180., (width - right - 360.).clamp(180., 420.));
            }
            SidebarEdge::Right => {
                let left = if width
                    < self.preferences.sidebar_width + self.preferences.details_width + 360.
                {
                    0.
                } else {
                    self.preferences.sidebar_width
                };
                self.preferences.details_width =
                    (width - x).clamp(280., (width - left - 360.).clamp(280., 560.));
            }
        }
        self.save_preferences(cx);
    }

    pub(crate) fn panel_widths(&self) -> (f32, f32) {
        (
            self.preferences.sidebar_width,
            self.preferences.details_width,
        )
    }

    pub(crate) fn set_panel_widths(&mut self, widths: (f32, f32), cx: &mut Context<Self>) {
        if self.panel_widths() != widths {
            self.preferences.sidebar_width = widths.0;
            self.preferences.details_width = widths.1;
            self.save_preferences(cx);
        }
    }

    fn save_preferences(&mut self, cx: &mut Context<Self>) {
        self._save_preferences = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            let _ = this.update(cx, |app, _| {
                if let Err(err) = app.preferences.save() {
                    eprintln!("{err}");
                }
            });
        }));
        cx.notify();
    }
}

impl Drop for ElseApp {
    fn drop(&mut self) {
        if let Err(err) = self.preferences.save() {
            eprintln!("{err}");
        }
    }
}
