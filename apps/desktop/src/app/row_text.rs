use super::link_target::LinkTarget;
use super::{ElseApp, RowKey};
use gpui_kit::base::text::TextView;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowFormat {
    Plain,
    Markdown,
}

pub(super) struct RowText {
    source: String,
    widget: Widget,
}

enum Widget {
    Plain(Entity<TextareaState>),
    Markdown {
        state: Entity<TextViewState>,
        _observe: Subscription,
    },
}

impl RowText {
    pub(super) fn new(
        key: RowKey,
        source: String,
        format: RowFormat,
        window: &mut Window,
        cx: &mut Context<ElseApp>,
    ) -> Self {
        let widget = match format {
            RowFormat::Plain => Widget::Plain(cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(1, 100_000)
                    .default_value(source.clone())
                    .context_menu(!matches!(key, RowKey::Item(_)))
            })),
            RowFormat::Markdown => {
                let state = cx.new(|cx| TextViewState::markdown(&source, cx));
                // Markdown parses in the background, so the row's height is
                // only known once the state notifies.
                let _observe = cx.observe_in(&state, window, move |app, state, window, cx| {
                    let Some(view) = app.view.as_ref() else {
                        return;
                    };
                    let current = view.texts.get(&key).and_then(|row| match &row.widget {
                        Widget::Markdown { state, .. } => Some(state),
                        Widget::Plain(_) => None,
                    });
                    if current != Some(&state) {
                        return;
                    }
                    if let Some(ix) = view.rows.iter().position(|r| r.key == key) {
                        view.list.remeasure_items(ix..ix + 1);
                        app.check_reading(window, cx);
                        cx.notify();
                    }
                });
                Widget::Markdown { state, _observe }
            }
        };
        Self { source, widget }
    }

    pub(super) fn format(&self) -> RowFormat {
        match self.widget {
            Widget::Plain(_) => RowFormat::Plain,
            Widget::Markdown { .. } => RowFormat::Markdown,
        }
    }

    pub(super) fn rendered_bytes(&self, cx: &App) -> Option<usize> {
        match &self.widget {
            Widget::Plain(_) => Some(self.source.len()),
            Widget::Markdown { state, .. } => {
                let rendered = state.read(cx).rendered_text();
                self.source
                    .starts_with(rendered.source())
                    .then_some(rendered.source().len())
            }
        }
    }

    pub(super) fn copy_text(&self, cx: &App) -> String {
        let selected = match &self.widget {
            Widget::Plain(state) => state.read(cx).selected_value().to_string(),
            Widget::Markdown { state, .. } => state.read(cx).selected_text(),
        };
        if selected.is_empty() {
            self.source.clone()
        } else {
            selected
        }
    }

    pub(super) fn sync(&mut self, source: String, window: &mut Window, cx: &mut App) -> bool {
        if self.source == source {
            return false;
        }
        match &self.widget {
            Widget::Plain(state) => {
                state.update(cx, |state, cx| state.set_value(source.clone(), window, cx))
            }
            Widget::Markdown { state, .. } => {
                state.update(cx, |state, cx| state.set_text(&source, cx))
            }
        }
        self.source = source;
        true
    }

    pub(super) fn render(&self) -> AnyElement {
        match &self.widget {
            Widget::Plain(state) => Textarea::new(state)
                .readonly(true)
                .appearance(false)
                .bordered(false)
                .into_any_element(),
            Widget::Markdown { state, .. } => TextView::new(state)
                .w_full()
                .text_sm()
                .line_height(rems(1.25))
                .scrollable(false)
                .selectable(true)
                .image_source(|_| ImageSource::Custom(Arc::new(|_, _| None)))
                .on_link_click(|url, event, _, cx| {
                    if activates_link(event)
                        && let Some(target) = LinkTarget::parse(url)
                    {
                        target.open(cx);
                    }
                })
                .link_tooltip(|url, window, cx| {
                    let destination = LinkTarget::parse(url)
                        .map_or_else(|| url.to_string(), |target| target.destination());
                    Tooltip::element(move |_, _| div().max_w(rems(32.)).child(destination.clone()))
                        .build(window, cx)
                })
                .into_any_element(),
        }
    }
}

/// The toolkit's default link gestures.
fn activates_link(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(click) => {
            matches!(click.up.button, MouseButton::Left | MouseButton::Middle)
        }
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(click) => !click.long_press,
    }
}
