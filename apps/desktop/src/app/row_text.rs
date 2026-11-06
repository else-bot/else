use super::bot_mention::{BotMention, MentionDisplay, MentionPlugin, MentionRegistry};
use super::images::{MessageImages, UnavailableImage, without_images};
use super::link_target::LinkTarget;
use super::{ElseApp, RowKey};
use crate::attachments::{Attachment, MessagePart, file_id, file_size, input_content};
use gpui_kit::base::text::{MarkdownExtensions, TextView};
use gpui_kit::component::input::InputContent;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowFormat {
    Plain,
    Markdown,
}

pub(super) struct RowSource {
    pub text: String,
    pub parts: Vec<MessagePart>,
    pub format: RowFormat,
}

pub(super) struct RowText {
    source: String,
    mentions: Vec<BotMention>,
    bots: Arc<MentionRegistry>,
    display: MentionDisplay,
    widget: Widget,
    parts: Vec<MessagePart>,
    files: HashMap<Uuid, Attachment>,
    owner: WeakEntity<ElseApp>,
    bot: Uuid,
    images: Entity<MessageImages>,
    _observe_images: Subscription,
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
        source: RowSource,
        client: Arc<crate::api::Client>,
        bots: &Arc<MentionRegistry>,
        bot: Uuid,
        window: &mut Window,
        cx: &mut Context<ElseApp>,
    ) -> Self {
        let RowSource {
            text: source,
            parts,
            format,
        } = source;
        let (visible, visible_parts) = without_images(&source, &parts);
        let mut mentions = match format {
            RowFormat::Plain => BotMention::find(&visible, bots),
            RowFormat::Markdown => BotMention::in_markdown(&visible, bots),
        };
        let content = plain_content(&visible, &visible_parts, &mut mentions);
        let display = MentionDisplay::new(&visible, &mentions);
        let widget = match format {
            RowFormat::Plain => Widget::Plain(cx.new(|cx| {
                let mut state = TextareaState::new(window, cx)
                    .auto_grow(1, 100_000)
                    .context_menu(!matches!(key, RowKey::Item(_)));
                state.set_value(content, window, cx);
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
                    if let Some(ix) = view.row_index(key) {
                        view.list.remeasure_items(ix..ix + 1);
                        app.check_reading(window, cx);
                        cx.notify();
                    }
                });
                Widget::Markdown { state, _observe }
            }
        };
        let (images, observe_images) =
            ElseApp::message_images(client, key, bot, &parts, window, cx);
        Self {
            source,
            mentions,
            bots: bots.clone(),
            display,
            widget,
            files: file_map(&parts),
            parts,
            owner: cx.entity().downgrade(),
            bot,
            images,
            _observe_images: observe_images,
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
                if rendered.source() == self.display.text {
                    return Some(self.source.len());
                }
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
        source: RowSource,
        bots: &Arc<MentionRegistry>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        let RowSource {
            text: source,
            parts,
            ..
        } = source;
        if self.source == source && self.parts == parts && Arc::ptr_eq(&self.bots, bots) {
            return false;
        }
        let (visible, visible_parts) = without_images(&source, &parts);
        self.images.update(cx, |images, cx| images.sync(&parts, cx));
        let mut mentions = match self.format() {
            RowFormat::Plain => BotMention::find(&visible, bots),
            RowFormat::Markdown => BotMention::in_markdown(&visible, bots),
        };
        self.bots = bots.clone();
        let content = plain_content(&visible, &visible_parts, &mut mentions);
        if self.source == source && self.parts == parts && self.mentions == mentions {
            return false;
        }
        let display = MentionDisplay::new(&visible, &mentions);
        match &self.widget {
            Widget::Plain(state) => {
                state.update(cx, |state, cx| state.set_value(content, window, cx))
            }
            Widget::Markdown { state, .. } => state.update(cx, |state, cx| {
                state.set_text(&display.text, cx);
                state.invalidate_inline_layout(cx);
            }),
        }
        self.source = source;
        self.files = file_map(&parts);
        self.parts = parts;
        self.mentions = mentions;
        self.display = display;
        true
    }

    pub(super) fn render(&self) -> AnyElement {
        let mentions = self.mentions.clone();
        let files = self.files.clone();
        let preview_files = files.clone();
        let owner = self.owner.clone();
        let bot = self.bot;
        let body = match &self.widget {
            Widget::Plain(state) => Textarea::new(state)
                .readonly(true)
                .appearance(false)
                .bordered(false)
                .token(move |token, _, cx| {
                    if let Some(file) = file_id(token.token().id()).and_then(|id| files.get(&id)) {
                        return super::attachments::chip(
                            token,
                            file,
                            None,
                            file_size(file.size),
                            None,
                            cx,
                        );
                    }
                    mentions
                        .iter()
                        .find(|mention| mention.token_id() == token.token().id().as_ref())
                        .expect("row token belongs to its matched bot")
                        .token_element(token.line_height(), token.is_selected(), cx)
                })
                .on_token_click(move |event, window, cx| {
                    if let Some(file) =
                        file_id(event.token().id()).and_then(|id| preview_files.get(&id))
                    {
                        let _ = owner.update(cx, |app, cx| {
                            app.preview_attachment(bot, file.clone(), window, cx)
                        });
                    }
                })
                .into_any_element(),
            Widget::Markdown { state, .. } => TextView::new(state)
                .markdown_extensions(
                    MarkdownExtensions::default()
                        .plugin(MentionPlugin(mentions))
                        .plugin(UnavailableImage),
                )
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
        };
        use gpui_kit::prelude::FluentBuilder as _;
        gpui_kit::component::v_flex()
            .w_full()
            .gap_2()
            .when(!self.display.text.trim().is_empty(), |column| column.child(body))
            .when(self.parts.iter().any(|part| matches!(part, MessagePart::File { file } if super::images::is_image(file))), |column| column.child(self.images.clone()))
            .into_any_element()
    }
}

fn file_map(parts: &[MessagePart]) -> HashMap<Uuid, Attachment> {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::File { file } => Some((file.id, file.clone())),
            MessagePart::Text { .. } => None,
        })
        .collect()
}

fn plain_content(
    source: &str,
    parts: &[MessagePart],
    mentions: &mut Vec<BotMention>,
) -> InputContent {
    let mut content = input_content(source, parts);
    mentions.retain(|mention| {
        content.tokens().iter().all(|file| {
            let range = file.range();
            mention.range.end <= range.start || mention.range.start >= range.end
        })
    });
    for span in BotMention::content(source, mentions).tokens() {
        content = content
            .with_token(span.range(), span.token().clone())
            .expect("mention and file spans are disjoint");
    }
    content
}

fn activates_link(event: &ClickEvent) -> bool {
    match event {
        ClickEvent::Mouse(click) => {
            matches!(click.up.button, MouseButton::Left | MouseButton::Middle)
        }
        ClickEvent::Keyboard(_) => true,
        ClickEvent::Touch(click) => !click.long_press,
    }
}

#[cfg(test)]
mod tests {
    use super::plain_content;
    use crate::api::Bot;
    use crate::app::bot_mention::{BotMention, MentionRegistry};
    use crate::attachments::{Attachment, MessagePart, file_id};
    use uuid::Uuid;

    #[test]
    fn mentions_and_files_share_a_row_without_overlap_or_id_collisions() {
        let id = Uuid::new_v4();
        let bot: Bot =
            serde_json::from_value(serde_json::json!({"id": id, "name": "Helper"})).unwrap();
        let registry = MentionRegistry::new(&[bot]);
        let file = Attachment {
            id,
            name: "@Helper".into(),
            media_type: "text/plain".into(),
            size: 3,
        };
        let text = format!("@Helper see {} after @Helper", file.fallback());
        let parts = vec![
            MessagePart::Text {
                text: "@Helper see ".into(),
            },
            MessagePart::File { file },
            MessagePart::Text {
                text: " after @Helper".into(),
            },
        ];
        let mut mentions = BotMention::find(&text, &registry);
        assert_eq!(mentions.len(), 3);
        let content = plain_content(&text, &parts, &mut mentions);
        assert_eq!(content.text().as_ref(), text);
        assert_eq!(mentions.len(), 2);
        assert_eq!(content.tokens().len(), 3);
        assert_eq!(file_id(content.tokens()[0].token().id()), None);
        assert_eq!(file_id(content.tokens()[1].token().id()), Some(id));
        assert_eq!(
            content.tokens()[1].token().text().as_ref(),
            "[File: @Helper]"
        );
        assert_eq!(file_id(content.tokens()[2].token().id()), None);
    }
}
