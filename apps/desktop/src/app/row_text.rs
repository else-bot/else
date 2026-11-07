use super::bot_mention::{BotMention, MentionDisplay, MentionPlugin, MentionRegistry};
use super::images::{MessageImages, UnavailableImage, without_images};
use super::link_target::LinkTarget;
use super::reply::{Author, QuotedReply};
use super::{ElseApp, RowKey};
use crate::attachments::{Attachment, MessagePart, file_id, file_size, input_content};
use gpui_kit::assets::IconName;
use gpui_kit::base::FocusableExt;
use gpui_kit::base::text::{MarkdownExtensions, TextView, TextViewStyle};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::InputContent;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, Sizable, WindowExt, h_flex};
use gpui_kit::*;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RowFormat {
    Plain,
    Markdown,
    OwnerMarkdown,
    Quoted { chips: bool },
}

impl RowFormat {
    pub(super) fn for_owner(text: &str, parts: &[MessagePart]) -> Self {
        if super::reply::QuotedReply::parse(text).is_some() {
            Self::Quoted {
                chips: Self::owner_body(parts) == Self::Plain,
            }
        } else {
            Self::owner_body(parts)
        }
    }

    // File chips are textarea tokens, so a message attaching a document
    // keeps the plain widget instead of losing its clickable chips.
    fn owner_body(parts: &[MessagePart]) -> Self {
        if parts.iter().any(
            |part| matches!(part, MessagePart::File { file } if !super::images::is_image(file)),
        ) {
            Self::Plain
        } else {
            Self::OwnerMarkdown
        }
    }

    pub(super) fn for_item(item: &crate::model::Item) -> Self {
        if item.kind == "assistant" || item.bot_reply.is_some() {
            Self::Markdown
        } else if item.kind == "user" && item.bot_task.is_none() {
            Self::for_owner(&item.text, &item.parts)
        } else {
            Self::Plain
        }
    }

    fn mentions(self, source: &str, bots: &MentionRegistry) -> Vec<BotMention> {
        match self {
            Self::Plain | Self::Quoted { .. } => BotMention::find(source, bots),
            Self::Markdown | Self::OwnerMarkdown => BotMention::in_markdown(source, bots),
        }
    }

    fn hard_breaks(self, source: &str) -> Vec<usize> {
        if self == Self::OwnerMarkdown {
            soft_line_breaks(source)
        } else {
            Vec::new()
        }
    }
}

pub(super) struct RowSource {
    pub text: String,
    pub parts: Vec<MessagePart>,
    pub format: RowFormat,
}

impl RowSource {
    fn split_quote(mut self) -> (Option<Self>, Self) {
        let RowFormat::Quoted { chips } = self.format else {
            return (None, self);
        };
        let reply = super::reply::QuotedReply::parse(&self.text)
            .expect("quoted row format comes from a complete reply");
        let prefix = &self.text[..self.text.len() - reply.body.len()];
        if let Some(MessagePart::Text { text }) = self.parts.first_mut()
            && let Some(body) = text.strip_prefix(prefix)
        {
            *text = body.to_owned();
        }
        let quote = Self {
            text: reply.quote.to_owned(),
            parts: Vec::new(),
            format: RowFormat::Markdown,
        };
        self.text = reply.body.to_owned();
        self.format = if chips {
            RowFormat::Plain
        } else {
            RowFormat::OwnerMarkdown
        };
        (Some(quote), self)
    }
}

pub(super) struct RowText {
    source: String,
    mentions: Vec<BotMention>,
    bots: Arc<MentionRegistry>,
    display: MentionDisplay,
    format: RowFormat,
    widget: Widget,
    quote: Option<Box<RowText>>,
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
        let (quote, body) = RowSource {
            text: source.clone(),
            parts: parts.clone(),
            format,
        }
        .split_quote();
        let quote = quote
            .map(|quote| Box::new(Self::new(key, quote, client.clone(), bots, bot, window, cx)));
        let (visible, visible_parts) = without_images(&body.text, &body.parts);
        let format = body.format;
        let mut mentions = format.mentions(&visible, bots);
        let content = plain_content(&visible, &visible_parts, &mut mentions);
        let display = MentionDisplay::new(&visible, &mentions, &format.hard_breaks(&visible));
        let widget = match format {
            RowFormat::Plain | RowFormat::Quoted { .. } => Widget::Plain(cx.new(|cx| {
                let mut state = TextareaState::new(window, cx)
                    .auto_grow(1, 100_000)
                    .context_menu(!matches!(key, RowKey::Item(_)));
                state.set_value(content, window, cx);
                state
            })),
            RowFormat::Markdown | RowFormat::OwnerMarkdown => {
                let state = cx.new(|cx| TextViewState::markdown(&display.text, cx));
                // Markdown parses in the background, so the row's height is
                // only known once the state notifies.
                let _observe = cx.observe_in(&state, window, move |app, state, window, cx| {
                    let Some(view) = app.view.as_ref() else {
                        return;
                    };
                    let current = view.texts.get(&key).and_then(Self::markdown_state);
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
            format,
            widget,
            quote,
            files: file_map(&parts),
            parts,
            owner: cx.entity().downgrade(),
            bot,
            images,
            _observe_images: observe_images,
        }
    }

    pub(super) fn format(&self) -> RowFormat {
        if self.quote.is_some() {
            RowFormat::Quoted {
                chips: self.format == RowFormat::Plain,
            }
        } else {
            self.format
        }
    }

    fn markdown_state(&self) -> Option<&Entity<TextViewState>> {
        match &self.widget {
            Widget::Markdown { state, .. } => Some(state),
            Widget::Plain(_) => self.quote.as_deref().and_then(Self::markdown_state),
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
            format,
        } = source;
        if self.source == source && self.parts == parts && Arc::ptr_eq(&self.bots, bots) {
            return false;
        }
        let (quote, body) = RowSource {
            text: source.clone(),
            parts: parts.clone(),
            format,
        }
        .split_quote();
        let quote_changed = match (&mut self.quote, quote) {
            (Some(view), Some(quote)) => view.sync(quote, bots, window, cx),
            _ => false,
        };
        let (visible, visible_parts) = without_images(&body.text, &body.parts);
        self.images.update(cx, |images, cx| images.sync(&parts, cx));
        let mut mentions = self.format.mentions(&visible, bots);
        self.bots = bots.clone();
        let content = plain_content(&visible, &visible_parts, &mut mentions);
        if self.source == source
            && self.parts == parts
            && self.mentions == mentions
            && !quote_changed
        {
            return false;
        }
        let display = MentionDisplay::new(&visible, &mentions, &self.format.hard_breaks(&visible));
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

    pub(super) fn render(&self, cx: &App) -> AnyElement {
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
                .style(
                    TextViewStyle::from_theme(&gpui_kit::base::Theme::global(cx))
                        .with_code_background(cx.theme().muted)
                        .with_link(cx.theme().link)
                        .with_code_block(
                            StyleRefinement::default()
                                .rounded(cx.theme().radius)
                                .pt(px(36.)),
                        )
                        .with_table(StyleRefinement::default().rounded(cx.theme().radius))
                        .with_table_head(
                            StyleRefinement::default()
                                .bg(cx.theme().table_head)
                                .text_color(cx.theme().table_head_foreground),
                        ),
                )
                .code_block_actions(|block, _, cx| {
                    let code = block.code();
                    h_flex()
                        .gap_2()
                        .font_family(cx.theme().font_family.clone())
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .children(block.lang())
                        .child(
                            Button::new("copy-code")
                                .ghost()
                                .xsmall()
                                .focus_ring(false)
                                .cursor_pointer()
                                .icon(IconName::Copy)
                                .accessibility_label("Copy code")
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        code.to_string(),
                                    ));
                                    window.push_notification("Code copied to clipboard.", cx);
                                }),
                        )
                })
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
            .children(self.render_quote_preview(cx))
            .when(!self.display.text.trim().is_empty(), |column| column.child(body))
            .when(self.parts.iter().any(|part| matches!(part, MessagePart::File { file } if super::images::is_image(file))), |column| column.child(self.images.clone()))
            .into_any_element()
    }

    fn render_quote_preview(&self, cx: &App) -> Option<AnyElement> {
        let quote = self.quote.as_ref()?;
        let reply = QuotedReply::parse(&self.source)?;
        let state = quote.markdown_state()?.clone();
        let mentions = quote.mentions.clone();
        let author = match reply.author {
            Author::Owner => "You".to_owned(),
            Author::Bot => self.bots.name(self.bot)?.to_owned(),
        };
        let theme = cx.theme();
        Some(
            Button::new("quoted-reply-preview")
                .ghost()
                .xsmall()
                .focus_ring(false)
                .cursor_pointer()
                .w_full()
                .min_w_0()
                .p_0()
                .text_color(theme.muted_foreground)
                .accessibility_label(format!("Show original message from {author}"))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(Icon::new(IconName::Reply).xsmall().flex_none())
                        .child(
                            div()
                                .flex_none()
                                .max_w(relative(0.4))
                                .truncate()
                                .text_color(theme.foreground)
                                .child(author.clone()),
                        )
                        .child(div().flex_none().child("·"))
                        .child(div().flex_1().min_w_0().truncate().child(reply.excerpt())),
                )
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    let state = state.clone();
                    let author = author.clone();
                    let mentions = mentions.clone();
                    window.open_dialog(cx, move |dialog, _, cx| {
                        dialog.title(format!("Message from {author}")).child(
                            div()
                                .id("original-message")
                                .w_full()
                                .max_h(px(440.))
                                .overflow_y_scroll()
                                .child(
                                    TextView::new(&state)
                                        .style(TextViewStyle::from_theme(
                                            &gpui_kit::base::Theme::global(cx),
                                        ))
                                        .w_full()
                                        .scrollable(false)
                                        .selectable(true)
                                        .markdown_extensions(
                                            MarkdownExtensions::default()
                                                .plugin(MentionPlugin(mentions.clone()))
                                                .plugin(UnavailableImage),
                                        )
                                        .image_source(|_| {
                                            ImageSource::Custom(Arc::new(|_, _| None))
                                        })
                                        .on_link_click(|url, event, _, cx| {
                                            if activates_link(event)
                                                && let Some(target) = LinkTarget::parse(url)
                                            {
                                                target.open(cx);
                                            }
                                        }),
                                ),
                        )
                    });
                })
                .into_any_element(),
        )
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

fn soft_line_breaks(source: &str) -> Vec<usize> {
    fn visit(node: &markdown::mdast::Node, source: &str, breaks: &mut Vec<usize>) {
        if let markdown::mdast::Node::Text(text) = node
            && let Some(position) = &text.position
        {
            let start = position.start.offset;
            let run = &source[start..position.end.offset];
            breaks.extend(
                run.char_indices()
                    .filter(|&(at, ch)| ch == '\r' || (ch == '\n' && !run[..at].ends_with('\r')))
                    .map(|(at, _)| start + at),
            );
        }
        for child in node.children().into_iter().flatten() {
            visit(child, source, breaks);
        }
    }
    let mut breaks = Vec::new();
    if (source.contains('\n') || source.contains('\r'))
        && let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm())
    {
        visit(&root, source, &mut breaks);
    }
    breaks
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
    use super::{RowFormat, RowSource, plain_content};
    use crate::api::Bot;
    use crate::app::bot_mention::MentionDisplay;
    use crate::app::bot_mention::{BotMention, MentionRegistry};
    use crate::app::reply::{Author, Draft, MessageReference};
    use crate::attachments::{Attachment, MessagePart, file_id};
    use gpui_kit::component::input::InputContent;
    use uuid::Uuid;

    #[test]
    fn owner_messages_render_markdown_unless_they_carry_file_chips() {
        let item = |kind: &str, text: &str| {
            serde_json::from_value(serde_json::json!({
                "id": Uuid::nil(), "seq": 1, "turn_id": Uuid::nil(),
                "kind": kind, "text": text
            }))
            .unwrap()
        };
        for text in [
            "Keep **this** and `that` as typed.",
            "Replying to your earlier message:\nThis is ordinary text.",
            "Replying to your earlier message:\n> quoted\nnot quoted\n\nAnswer.",
        ] {
            assert_eq!(
                RowFormat::for_item(&item("user", text)),
                RowFormat::OwnerMarkdown
            );
            assert_eq!(RowFormat::for_owner(text, &[]), RowFormat::OwnerMarkdown);
        }
        let attached = |media_type: &str| {
            let file = Attachment {
                id: Uuid::new_v4(),
                name: "attached".into(),
                media_type: media_type.into(),
                size: 1,
            };
            vec![
                MessagePart::Text {
                    text: "See **this** ".into(),
                },
                MessagePart::Text {
                    text: file.fallback(),
                },
                MessagePart::File { file },
            ]
        };
        assert_eq!(
            RowFormat::for_owner("See **this**", &attached("image/png")),
            RowFormat::OwnerMarkdown
        );
        assert_eq!(
            RowFormat::for_owner("See **this**", &attached("text/plain")),
            RowFormat::Plain
        );
        for author in [Author::Owner, Author::Bot] {
            let draft = Draft {
                content: InputContent::new("You can already mark it as always approved."),
                reply_to: Some(MessageReference {
                    item: Uuid::nil(), author,
                    text: "Created the routine for **04:00 Europe/Paris**.\n\nPreserve `main`.\n> An earlier quote.".into(),
                }),
                ..Draft::default()
            };
            let text = draft.message().unwrap();
            let quoted = RowFormat::Quoted { chips: false };
            assert_eq!(RowFormat::for_item(&item("user", &text)), quoted);
            assert_eq!(RowFormat::for_owner(&text, &[]), quoted);
            assert_eq!(
                RowFormat::for_owner(&text, &attached("text/plain")),
                RowFormat::Quoted { chips: true }
            );
        }
        assert_eq!(
            RowFormat::for_item(&item("assistant", "**Done**")),
            RowFormat::Markdown
        );
    }

    #[test]
    fn owner_line_breaks_survive_markdown_without_touching_code_or_offsets() {
        let source = "What should I do:\n\n**Open.**\nFirst line\r\nsecond @Scout\n\n```\na\nb\n```\n> quoted\n> more";
        let scout: Bot =
            serde_json::from_value(serde_json::json!({"id": Uuid::from_u128(1), "name": "Scout"}))
                .unwrap();
        let bots = MentionRegistry::new(&[scout]);
        let mentions = RowFormat::OwnerMarkdown.mentions(source, &bots);
        let breaks = RowFormat::OwnerMarkdown.hard_breaks(source);
        let display = MentionDisplay::new(source, &mentions, &breaks);
        assert_eq!(
            display.text,
            format!(
                "What should I do:\n\n**Open.**  \nFirst line  \r\nsecond [bot](bot-mention:{}:{})\n\n```\na\nb\n```\n> quoted  \n> more",
                Uuid::from_u128(1),
                source.find('@').unwrap()
            )
        );
        assert_eq!(display.source_bytes(display.text.len()), source.len());
        let first = display.text.find("  ").unwrap();
        assert_eq!(display.source_bytes(first), source.find("\nFirst").unwrap());
        assert_eq!(display.source_bytes(first + 2), first);
        assert_eq!(kinds(&display.text), ["strong", "break", "break", "break"]);
        assert!(RowFormat::Markdown.hard_breaks(source).is_empty());
        for (typed, expected) in [
            ("hello **\nworld**", vec!["break"]),
            (
                "[a\nb]\n\n[a b]: https://example.com",
                vec!["link", "break"],
            ),
        ] {
            let breaks = RowFormat::OwnerMarkdown.hard_breaks(typed);
            assert_eq!(
                kinds(&MentionDisplay::new(typed, &[], &breaks).text),
                expected,
                "{typed:?}"
            );
        }
    }

    fn kinds(display: &str) -> Vec<&'static str> {
        fn collect(node: &markdown::mdast::Node, kinds: &mut Vec<&'static str>) {
            match node {
                markdown::mdast::Node::Strong(_) => kinds.push("strong"),
                markdown::mdast::Node::Break(_) => kinds.push("break"),
                markdown::mdast::Node::LinkReference(_) => kinds.push("link"),
                _ => {}
            }
            for child in node.children().into_iter().flatten() {
                collect(child, kinds);
            }
        }
        let mut kinds = Vec::new();
        collect(
            &markdown::to_mdast(display, &markdown::ParseOptions::gfm()).unwrap(),
            &mut kinds,
        );
        kinds
    }

    #[test]
    fn quoted_replies_keep_the_owners_literal_text_and_file_tokens() {
        let file = Attachment {
            id: Uuid::new_v4(),
            name: "report.txt".into(),
            media_type: "text/plain".into(),
            size: 6,
        };
        let lead = "Keep **literal** text. ";
        let body = format!("{lead}{}", file.fallback());
        let message = Draft {
            content: InputContent::new(body.clone()),
            reply_to: Some(MessageReference {
                item: Uuid::nil(),
                author: Author::Bot,
                text: "**Résumé**\n\nPreserve `main`.".into(),
            }),
            ..Draft::default()
        }
        .message()
        .unwrap();
        let prefix = &message[..message.len() - body.len()];
        for coalesced in [false, true] {
            let mut parts = vec![MessagePart::Text {
                text: if coalesced {
                    format!("{prefix}{lead}")
                } else {
                    prefix.to_owned()
                },
            }];
            if !coalesced {
                parts.push(MessagePart::Text { text: lead.into() });
            }
            parts.push(MessagePart::File { file: file.clone() });
            let (quote, body_source) = RowSource {
                text: message.clone(),
                format: RowFormat::for_owner(&message, &parts),
                parts,
            }
            .split_quote();
            let quote = quote.unwrap();
            assert_eq!(quote.format, RowFormat::Markdown);
            assert_eq!(body_source.format, RowFormat::Plain);
            assert_eq!(body_source.text, body);
            assert_eq!(format!("{}\n\n{}", quote.text, body_source.text), message);
            let content = plain_content(&body_source.text, &body_source.parts, &mut Vec::new());
            assert_eq!(content.text().as_ref(), body);
            assert_eq!(content.tokens().len(), 1);
            assert_eq!(file_id(content.tokens()[0].token().id()), Some(file.id));
            assert_eq!(&body[content.tokens()[0].range()], file.fallback());
        }
    }

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
