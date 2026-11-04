use super::bot_mention::{BotMention, MentionDisplay, MentionPlugin};
use super::link_target::LinkTarget;
use super::{ElseApp, RowKey};
use crate::api::Bot;
use gpui_kit::base::text::{MarkdownExtensions, TextView};
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
    mentions: Vec<BotMention>,
    display: MentionDisplay,
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
        bots: &[Bot],
        window: &mut Window,
        cx: &mut Context<ElseApp>,
    ) -> Self {
        let mentions = match format {
            RowFormat::Plain => BotMention::find(&source, bots),
            RowFormat::Markdown => BotMention::in_markdown(&source, bots),
        };
        let display = MentionDisplay::new(&source, &mentions);
        let widget = match format {
            RowFormat::Plain => Widget::Plain(cx.new(|cx| {
                let mut state = TextareaState::new(window, cx)
                    .auto_grow(1, 100_000)
                    .context_menu(!matches!(key, RowKey::Item(_)));
                state.set_value(BotMention::content(&source, &mentions), window, cx);
                state
            })),
            RowFormat::Markdown => {
                let state = cx.new(|cx| TextViewState::markdown(&display.text, cx));
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
        Self {
            source,
            mentions,
            display,
            widget,
        }
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
                self.display
                    .text
                    .starts_with(rendered.source())
                    .then(|| self.display.source_bytes(rendered.source().len()))
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

    pub(super) fn sync(
        &mut self,
        source: String,
        bots: &[Bot],
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let mentions = match self.format() {
            RowFormat::Plain => BotMention::find(&source, bots),
            RowFormat::Markdown => BotMention::in_markdown(&source, bots),
        };
        if self.source == source && self.mentions == mentions {
            return false;
        }
        let display = MentionDisplay::new(&source, &mentions);
        match &self.widget {
            Widget::Plain(state) => state.update(cx, |state, cx| {
                state.set_value(BotMention::content(&source, &mentions), window, cx)
            }),
            Widget::Markdown { state, .. } => state.update(cx, |state, cx| {
                state.set_text(&display.text, cx);
                state.invalidate_inline_layout(cx);
            }),
        }
        self.source = source;
        self.mentions = mentions;
        self.display = display;
        true
    }

    pub(super) fn render(&self) -> AnyElement {
        let mentions = self.mentions.clone();
        match &self.widget {
            Widget::Plain(state) => Textarea::new(state)
                .readonly(true)
                .appearance(false)
                .bordered(false)
                .token(move |token, _, cx| {
                    mentions
                        .iter()
                        .find(|mention| mention.token_id() == token.token().id().as_ref())
                        .expect("row token belongs to its matched bot")
                        .token_element(token.line_height(), token.is_selected(), cx)
                })
                .into_any_element(),
            Widget::Markdown { state, .. } => TextView::new(state)
                .markdown_extensions(MarkdownExtensions::default().plugin(MentionPlugin(mentions)))
                .w_full()
                .text_sm()
                .line_height(rems(1.375))
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
