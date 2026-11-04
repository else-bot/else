use super::{ElseApp, RowKey};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::text::{TextView, TextViewState};
use gpui_kit::*;
use std::sync::Arc;
use url::Url;

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
            })),
            RowFormat::Markdown => {
                let state = cx.new(|cx| TextViewState::markdown(&source, cx));
                // Markdown parses in the background, so the row's height is
                // only known once the state notifies.
                let _observe = cx.observe(&state, move |app, state, cx| {
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
                    if let Some(ix) = view.rows.iter().position(|r| *r == key) {
                        view.list.remeasure_items(ix..ix + 1);
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
                .scrollable(false)
                .selectable(true)
                .image_source(|_| ImageSource::Custom(Arc::new(|_, _| None)))
                .on_link_click(|url, event, _, cx| {
                    if activates_link(event) && is_web_link(url) {
                        cx.open_url(url);
                    }
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

fn is_web_link(url: &str) -> bool {
    Url::parse(url).is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}
