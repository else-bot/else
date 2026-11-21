use super::bot_mention::{BotMention, MentionDisplay, MentionPlugin, MentionRegistry};
use super::conversation::ConversationView;
use super::images::{MessageImages, UnavailableImage, without_images};
use super::link_target::LinkTarget;
use super::reply::{Author, Passage, QuotedReply};
use super::{ElseApp, RowKey};
use crate::attachments::{byte_size, file_id, input_content};
use else_wire::items::Attachment;
use else_wire::items::ItemKind;
use else_wire::items::MessagePart;
use gpui_kit::assets::IconName;
use gpui_kit::base::FocusableExt;
use gpui_kit::base::text::{
    CodeBlock, MarkdownExtensions, RangeHighlight, RenderedText, TextView, TextViewStyle,
};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::InputContent;
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::text::TextViewState;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, Sizable, WindowExt, h_flex};
use gpui_kit::*;
use std::collections::HashMap;
use std::ops::Range;
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

    pub(super) fn for_item(item: &else_wire::items::Item) -> Self {
        if item.kind == ItemKind::Assistant || item.bot_reply.is_some() {
            Self::Markdown
        } else if item.kind == ItemKind::User && item.bot_task.is_none() {
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

    /// Display edits that fold each quoted passage of a multi-passage reply
    /// into one linked line, with the line breaks to keep around them.
    /// Mentions inside the folded quotes are dropped.
    fn passages(
        self,
        source: &str,
        mentions: &mut Vec<BotMention>,
    ) -> (Vec<(Range<usize>, String)>, Vec<usize>) {
        let breaks = self.hard_breaks(source);
        let Some((header, passages)) = (self == Self::OwnerMarkdown)
            .then(|| Passage::parse_all(source))
            .flatten()
        else {
            return (Vec::new(), breaks);
        };
        let mut edits = vec![(header, String::new())];
        edits.extend(passages.iter().enumerate().map(|(ix, passage)| {
            let mut excerpt: String = passage.excerpt().chars().take(PASSAGE_EXCERPT).collect();
            if excerpt.len() < passage.excerpt().len() {
                excerpt.push('…');
            }
            (
                passage.block.clone(),
                format!(
                    "> **{}** · [{}]({QUOTE_LINK}{})",
                    ix + 1,
                    escape_markdown(&excerpt),
                    ix + 1
                ),
            )
        }));
        let folded = |at: usize| {
            edits
                .iter()
                .any(|(range, _)| range.start <= at && at <= range.end)
        };
        mentions.retain(|mention| !folded(mention.range.start) && !folded(mention.range.end));
        let breaks = breaks.into_iter().filter(|at| !folded(*at)).collect();
        (edits, breaks)
    }
}

const PASSAGE_EXCERPT: usize = 110;
const TABLE_CELL_GUTTER: Pixels = px(24.);
/// The space the text view keeps on each side of inline code.
const INLINE_CODE_PADDING: Pixels = px(4.);

fn escape_markdown(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            c.is_ascii_punctuation()
                .then_some('\\')
                .into_iter()
                .chain([c])
        })
        .collect()
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

/// Backgrounds painted behind ranges of a row's rendered text.
pub(super) type Highlights = Vec<(Range<usize>, Hsla)>;

pub(super) struct RowText {
    source: String,
    /// The highlights last painted, and the rendered text they fit.
    painted: Option<(RenderedText, Highlights)>,
    mentions: Vec<BotMention>,
    bots: Arc<MentionRegistry>,
    display: MentionDisplay,
    format: RowFormat,
    widget: Widget,
    quote: Option<Box<RowText>>,
    parts: Vec<MessagePart>,
    files: HashMap<Uuid, Attachment>,
    owner: WeakEntity<ElseApp>,
    view: WeakEntity<ConversationView>,
    bot: Uuid,
    key: RowKey,
    images: Entity<MessageImages>,
    _observe_images: Subscription,
    visuals: Entity<super::visuals::MessageVisuals>,
    _observe_visuals: Subscription,
}

enum Widget {
    Plain(Entity<TextareaState>),
    Markdown {
        state: Entity<TextViewState>,
        _observe: Subscription,
    },
}

#[derive(Clone)]
pub(super) struct RowHost {
    pub(super) client: Arc<crate::api::Client>,
    pub(super) bot: Uuid,
    pub(super) app: WeakEntity<ElseApp>,
}

impl RowText {
    pub(super) fn new(
        key: RowKey,
        source: RowSource,
        host: &RowHost,
        bots: &Arc<MentionRegistry>,
        window: &mut Window,
        cx: &mut Context<ConversationView>,
    ) -> Self {
        let RowHost { client, bot, app } = host;
        let (client, bot, owner) = (client.clone(), *bot, app.clone());
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
        let quote = quote.map(|quote| Box::new(Self::new(key, quote, host, bots, window, cx)));
        let (visible, visible_parts) = without_images(&body.text, &body.parts);
        let format = body.format;
        let mut mentions = format.mentions(&visible, bots);
        let (passages, breaks) = format.passages(&visible, &mut mentions);
        let content = plain_content(&visible, &visible_parts, &mut mentions);
        let files = file_map(&parts);
        let display = MentionDisplay::new(
            &visible,
            &mentions,
            &[file_links(&content, &files), passages].concat(),
            &breaks,
        );
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
                let _observe = cx.observe_in(&state, window, move |view, state, window, cx| {
                    let current = view.texts.get(&key).and_then(Self::markdown_state);
                    if current != Some(&state) {
                        return;
                    }
                    view.paint_row_quotes(key, cx);
                    if let Some(ix) = view.row_index(key) {
                        view.list.remeasure_items(ix..ix + 1);
                        view.check_reading(window, cx);
                        cx.notify();
                    }
                });
                Widget::Markdown { state, _observe }
            }
        };
        let (images, observe_images) = ConversationView::message_images(
            client.clone(),
            key,
            bot,
            owner.clone(),
            &parts,
            window,
            cx,
        );
        let (visuals, observe_visuals) =
            ConversationView::message_visuals(client, key, bot, &parts, window, cx);
        Self {
            source,
            painted: None,
            mentions,
            bots: bots.clone(),
            display,
            format,
            widget,
            quote,
            files,
            parts,
            owner,
            view: cx.entity().downgrade(),
            bot,
            key,
            images,
            _observe_images: observe_images,
            visuals,
            _observe_visuals: observe_visuals,
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

    /// The width an owner message needs with one line per paragraph, so its
    /// bubble can hug the text. Rich text and text carrying attachment chips
    /// report only their narrowest layout, so this measures their widest line;
    /// plain text without chips sizes itself and returns `None`.
    pub(super) fn natural_width(&self, window: &Window, cx: &App) -> Option<Pixels> {
        let (text, rendered) = match &self.widget {
            Widget::Markdown { state, .. } => {
                let rendered = state.read(cx).rendered_text();
                (rendered.as_str().to_owned(), Some(rendered))
            }
            Widget::Plain(_) if !self.files.is_empty() => (self.source.clone(), None),
            Widget::Plain(_) if self.quote.is_some() => (self.display.text.clone(), None),
            Widget::Plain(_) => return None,
        };
        // Shape each line whole, as the text view does before it decides a
        // line fits. Summed glyph advances leave out the platform's shaping,
        // which on macOS draws wider and wrapped the last word.
        let measure_in = |line: &str, font: Font| {
            let run = TextRun {
                len: line.len(),
                font,
                color: Hsla::default(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            window
                .text_system()
                .layout_line(line, px(14.), &[run], None)
                .width
        };
        let body = font(cx.theme().font_family.clone());
        let measure = |line: &str| measure_in(line, body.clone());
        // An attachment chip adds its icon, padding and size label to the file name.
        let chips = self
            .files
            .values()
            .map(|file| measure(&file.name) + px(64.));
        // A list item hangs its marker in a column 1.4 em wide, indents its
        // later blocks by that column, and each nesting level shifts it by one
        // more; none of that is in the rendered text.
        let mut indents = vec![px(0.); text.lines().count()];
        // A table lays its columns side by side, so the rendered text, which
        // joins a row's cells with spaces, says nothing of its width. The text
        // view gives each column its widest cell plus the 24 px gutter, at
        // least 48 px, and a 1 px divider before the next; the table adds 2 px
        // for its frame even without a border. Cells are measured at body size
        // in the weight or mono face they draw in, which never falls short of
        // the smaller semibold head or code.
        let bold = Font {
            weight: FontWeight::BOLD,
            ..body.clone()
        };
        let mono = font(cx.theme().mono_font_family.clone());
        let measure_cell = |cell: &[(CellRun, String)]| {
            cell.iter()
                .map(|(run, text)| match run {
                    CellRun::Text => measure_in(text, body.clone()),
                    CellRun::Strong => measure_in(text, bold.clone()),
                    CellRun::Code => measure_in(text, mono.clone()) + INLINE_CODE_PADDING * 2.,
                })
                .sum::<Pixels>()
        };
        let tables: Vec<Pixels> = rendered
            .as_ref()
            .map(|rendered| table_columns(rendered.source()))
            .unwrap_or_default()
            .iter()
            .map(|columns| {
                let cells: Pixels = columns
                    .iter()
                    .map(|cells| {
                        let widest = cells
                            .iter()
                            .map(|cell| measure_cell(cell))
                            .fold(px(0.), Pixels::max);
                        (widest + TABLE_CELL_GUTTER).max(px(48.))
                    })
                    .sum();
                cells + px(columns.len().saturating_sub(1) as f32) + px(2.)
            })
            .collect();
        if let Some(rendered) = rendered {
            let column = px(14.) * 1.4;
            for block in list_blocks(rendered.source()) {
                let Some(range) = rendered.range_for_source(block.content) else {
                    continue;
                };
                let first = text[..range.start.min(text.len())].matches('\n').count();
                let lines = text[range.start.min(text.len())..range.end.min(text.len())]
                    .matches('\n')
                    .count();
                let hang = block.marker.map_or(column, |marker| {
                    (measure(marker) + column * 0.3).max(column)
                });
                for indent in indents.iter_mut().skip(first).take(lines + 1) {
                    *indent = column * block.depth as f32 + hang;
                }
            }
        }
        let widest = text
            .lines()
            .zip(indents)
            .map(|(line, indent)| measure(line) + indent)
            .chain(chips)
            .chain(tables)
            .fold(px(0.), Pixels::max);
        // Plain text also needs its own inset and the textarea's 10 px caret margin.
        let (inset_x, _) = self.inset();
        let caret = if inset_x > px(0.) { px(10.) } else { px(0.) };
        // The quote line above the body: reply icon, author, "·" and the
        // excerpt, 8 px apart.
        let quote = self.quote_line().map_or(px(0.), |(author, excerpt)| {
            px(12.) + measure(&author) + measure("·") + measure(&excerpt) + px(24.)
        });
        Some((widest + inset_x * 2. + caret).max(quote) + px(4.))
    }

    fn quote_line(&self) -> Option<(String, String)> {
        let reply = QuotedReply::parse(&self.source)?;
        let author = match reply.author {
            Author::Owner => "You".to_owned(),
            Author::Bot => self.bots.name(self.bot)?.to_owned(),
        };
        Some((author, reply.excerpt()))
    }

    /// The horizontal and vertical padding the widget draws inside itself: a
    /// read-only textarea keeps the medium input's editor padding.
    pub(super) fn inset(&self) -> (Pixels, Pixels) {
        match &self.widget {
            Widget::Plain(_) => (px(10.), px(8.)),
            Widget::Markdown { .. } => (px(0.), px(0.)),
        }
    }

    /// Whether the row shows all of its text, so its height is final.
    pub(super) fn laid_out(&self, cx: &App) -> bool {
        self.rendered_bytes(cx) == Some(self.source.len())
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

    /// The selected text and its range in the rendered text, when the
    /// selection lies in this row's Markdown.
    pub(super) fn selection(&self, cx: &App) -> Option<(String, Option<Range<usize>>)> {
        let Widget::Markdown { state, .. } = &self.widget else {
            return None;
        };
        let state = state.read(cx);
        let text = state.selected_text();
        if text.trim().is_empty() {
            return None;
        }
        let range = state
            .selected_source_range()
            .and_then(|source| state.rendered_text().range_for_source(source));
        Some((text, range))
    }

    pub(super) fn is_markdown(&self) -> bool {
        matches!(self.widget, Widget::Markdown { .. })
    }

    /// Where `quote` shows in the rendered text, or all of the text for
    /// `None`; `None` until the Markdown has parsed.
    pub(super) fn passage(&self, quote: Option<&str>, cx: &App) -> Option<Range<usize>> {
        let Widget::Markdown { state, .. } = &self.widget else {
            return None;
        };
        let rendered = state.read(cx).rendered_text();
        match quote {
            _ if rendered.is_empty() => None,
            Some(quote) => super::reply::passage_range(rendered.as_str(), quote),
            None => Some(0..rendered.len()),
        }
    }

    /// Where `needle` shows in the rendered text; nothing for plain text or
    /// before the Markdown has parsed.
    pub(super) fn find(
        &self,
        needle: &else_plain_text::find::Needle,
        cx: &App,
    ) -> Vec<Range<usize>> {
        let Widget::Markdown { state, .. } = &self.widget else {
            return Vec::new();
        };
        needle.find(state.read(cx).rendered_text().as_str())
    }

    /// Asks the enclosing list to scroll to `range` of the rendered text.
    /// Returns whether the request was taken.
    pub(super) fn reveal(&self, range: &Range<usize>, cx: &mut App) -> bool {
        // Scroll to the passage's middle, but no more than 240 bytes in, so a
        // long passage still starts on screen.
        const MAX_LEAD: usize = 240;
        let Widget::Markdown { state, .. } = &self.widget else {
            return false;
        };
        state.update(cx, |state, cx| {
            let text = state.rendered_text();
            let mut at = range.start + (range.len() / 2).min(MAX_LEAD);
            while !text.as_str().is_char_boundary(at) {
                at -= 1;
            }
            state.reveal_range(at..at, cx).is_ok()
        })
    }

    /// Paints `highlights` behind ranges of the rendered text. Ranges that do
    /// not fit, as before the Markdown has parsed, are tried again on the
    /// next call.
    pub(super) fn highlight(&mut self, highlights: Highlights, cx: &mut App) {
        let Widget::Markdown { state, .. } = &self.widget else {
            return;
        };
        let rendered = state.read(cx).rendered_text();
        if self
            .painted
            .as_ref()
            .is_some_and(|(text, painted)| *text == rendered && *painted == highlights)
        {
            return;
        }
        let painted = state.update(cx, |state, cx| {
            let painted = highlights.is_empty()
                || state
                    .set_range_highlights(
                        highlights
                            .iter()
                            .map(|(range, color)| RangeHighlight::new(range.clone(), *color)),
                        cx,
                    )
                    .is_ok();
            if highlights.is_empty() || !painted {
                state.clear_range_highlights(cx);
            }
            painted
        });
        self.painted = painted.then_some((rendered, highlights));
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
        self.visuals
            .update(cx, |visuals, cx| visuals.sync(&parts, cx));
        let mut mentions = self.format.mentions(&visible, bots);
        let (passages, breaks) = self.format.passages(&visible, &mut mentions);
        self.bots = bots.clone();
        let content = plain_content(&visible, &visible_parts, &mut mentions);
        if self.source == source
            && self.parts == parts
            && self.mentions == mentions
            && !quote_changed
        {
            return false;
        }
        let files = file_map(&parts);
        let display = MentionDisplay::new(
            &visible,
            &mentions,
            &[file_links(&content, &files), passages].concat(),
            &breaks,
        );
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
        self.files = files;
        self.parts = parts;
        self.mentions = mentions;
        self.display = display;
        true
    }

    pub(super) fn render(&self, cx: &App) -> AnyElement {
        let mentions = self.mentions.clone();
        let clicked_mentions = mentions.clone();
        let files = self.files.clone();
        let preview_files = files.clone();
        let link_files = files.clone();
        let tooltip_files = files.clone();
        let link_source = self.display.text.clone();
        let owner = self.owner.clone();
        let link_owner = owner.clone();
        let link_view = self.view.clone();
        let bot = self.bot;
        let key = self.key;
        let passages = Passage::parse_all(&self.source)
            .map(|(_, passages)| passages)
            .unwrap_or_default();
        let body = match &self.widget {
            Widget::Plain(state) => Textarea::new(state)
                .readonly(true)
                .appearance(false)
                .bordered(false)
                .min_w_0()
                .text_size(px(14.))
                .token(move |token, _, cx| {
                    if let Some(file) = file_id(token.token().id()).and_then(|id| files.get(&id)) {
                        return super::attachments::chip(
                            token,
                            file,
                            None,
                            byte_size(file.size),
                            None,
                            cx,
                        );
                    }
                    mentions
                        .iter()
                        .find(|mention| mention.token_id() == token.token().id().as_ref())
                        .expect("row token belongs to its matched bot")
                        .token_element(token.line_height(), token.is_selected(), true, cx)
                })
                .on_token_click(move |event, window, cx| {
                    let id = event.token().id();
                    if let Some(file) = file_id(id).and_then(|id| preview_files.get(&id)) {
                        let _ = owner.update(cx, |app, cx| {
                            app.preview_attachment(bot, file.clone(), window, cx)
                        });
                    } else if let Some(mentioned) = clicked_mentions
                        .iter()
                        .find(|mention| mention.token_id() == id.as_ref())
                        .map(BotMention::bot)
                    {
                        let _ = owner.update(cx, |app, cx| app.open_bot(mentioned, window, cx));
                    }
                })
                .into_any_element(),
            Widget::Markdown { state, .. } => {
                let heading_ink = crate::appearance::tone(crate::appearance::Tone::Strong, cx);
                TextView::new(state)
                    .style(
                        code_style(cx.theme().secondary, cx)
                            .with_link(cx.theme().link)
                            .with_border(cx.theme().foreground.opacity(0.08))
                            // A reply's title (H1, H2) leads at 18–19 px with 20 px
                            // above it (8 px plus the paragraph gap) and 8 px below;
                            // sections (H3) at 16 px; deeper levels at body size.
                            .with_heading(move |level| {
                                let style = StyleRefinement::default()
                                    .text_size(px(match level {
                                        1 => 19.,
                                        2 => 18.,
                                        3 => 16.,
                                        _ => 14.,
                                    }))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(heading_ink);
                                if level <= 2 {
                                    style.pt(px(8.)).pb(px(8.))
                                } else {
                                    style
                                }
                            })
                            .with_table({
                                let mut table = StyleRefinement::default()
                                    .min_w_0()
                                    .border_0()
                                    .bg(cx.theme().background.opacity(0.));
                                table.overflow.x = Some(Overflow::Scroll);
                                table
                            })
                            .with_table_head(
                                StyleRefinement::default()
                                    .bg(cx.theme().background.opacity(0.))
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(cx.theme().muted_foreground),
                            )
                            // Each cell's whole 24 px gutter sits on its right,
                            // so the first column starts on the paragraph edge.
                            .with_table_cell(
                                StyleRefinement::default()
                                    .border_color(cx.theme().foreground.opacity(0.))
                                    .py_2()
                                    .pl_0()
                                    .pr(TABLE_CELL_GUTTER),
                            ),
                    )
                    .code_block_actions(copy_code)
                    .markdown_extensions(
                        MarkdownExtensions::default()
                            .plugin(MentionPlugin {
                                mentions,
                                owner: link_owner.clone(),
                            })
                            .plugin(UnavailableImage),
                    )
                    .w_full()
                    .min_w_0()
                    .text_size(px(14.))
                    .line_height(px(22.))
                    .scrollable(false)
                    .selectable(true)
                    .image_source(|_| ImageSource::Custom(Arc::new(|_, _| None)))
                    .on_link_click(move |url, event, window, cx| {
                        if !activates_link(event) {
                            return;
                        }
                        if let Some(passage) = quoted_passage(url).and_then(|n| passages.get(n)) {
                            let passage = passage.clone();
                            let _ = link_view.update(cx, |view, cx| {
                                view.reveal_quoted(
                                    key,
                                    passage.author,
                                    &passage.original,
                                    window,
                                    cx,
                                )
                            });
                        } else if let Some(file) =
                            linked_file(url).and_then(|id| link_files.get(&id))
                        {
                            let file = file.clone();
                            let _ = link_owner.update(cx, |app, cx| {
                                app.preview_attachment(bot, file, window, cx)
                            });
                        } else if let Some(target) = LinkTarget::parse(url) {
                            target.open(cx);
                        }
                    })
                    .link_tooltip(move |url, window, cx| {
                        if shows_its_destination(&link_source, url) {
                            return cx.new(|_| EmptyView).into();
                        }
                        let destination = if quoted_passage(url).is_some() {
                            "Go to the original message".to_owned()
                        } else {
                            match linked_file(url).and_then(|id| tooltip_files.get(&id)) {
                                Some(file) => format!("{} · {}", file.name, byte_size(file.size)),
                                None => LinkTarget::parse(url)
                                    .map_or_else(|| url.to_string(), |target| target.destination()),
                            }
                        };
                        Tooltip::element(move |_, _| {
                            div().max_w(rems(32.)).child(destination.clone())
                        })
                        .build(window, cx)
                    })
                    .into_any_element()
            }
        };
        use gpui_kit::prelude::FluentBuilder as _;
        gpui_kit::component::v_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .children(self.render_quote_preview(cx))
            .when(!self.display.text.trim().is_empty(), |column| column.child(body))
            .when(self.parts.iter().any(|part| matches!(part, MessagePart::File { file } if super::visuals::is_visual(file))), |column| column.child(self.visuals.clone()))
            .when(self.parts.iter().any(|part| matches!(part, MessagePart::File { file } if super::images::is_image(file))), |column| column.child(self.images.clone()))
            .into_any_element()
    }

    fn render_quote_preview(&self, cx: &App) -> Option<AnyElement> {
        let reply = QuotedReply::parse(&self.source)?;
        let original = reply.original();
        let view = self.view.clone();
        let from = self.key;
        let author_kind = reply.author;
        let (author, excerpt) = self.quote_line()?;
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
                .accessibility_label(format!("Go to original message from {author}"))
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
                        .child(div().flex_1().min_w_0().truncate().child(excerpt)),
                )
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    let original = original.clone();
                    let _ = view.update(cx, |view, cx| {
                        view.reveal_quoted(from, author_kind, &original, window, cx)
                    });
                })
                .into_any_element(),
        )
    }
}

/// The theme's text style with the code block look chat replies and cards
/// share, filled with `background`.
fn code_style(background: Hsla, cx: &App) -> TextViewStyle {
    TextViewStyle::from_theme(&gpui_kit::base::Theme::global(cx))
        .with_code_background(background)
        .with_code_block(
            StyleRefinement::default()
                .rounded(cx.theme().radius)
                .border_0()
                .pt(px(28.))
                .text_size(px(13.))
                .line_height(px(21.)),
        )
}

/// A code block's language and copy button.
fn copy_code(block: &CodeBlock, _: &mut Window, cx: &mut App) -> Div {
    let code = block.code();
    h_flex()
        .h(px(16.))
        .gap_2()
        .items_center()
        .font_family(cx.theme().font_family.clone())
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .children(block.lang())
        .child(
            Button::new("copy-code")
                .ghost()
                .xsmall()
                .size(px(16.))
                .focus_ring(false)
                .cursor_pointer()
                .icon(IconName::Copy)
                .accessibility_label("Copy code")
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    cx.write_to_clipboard(ClipboardItem::new_string(code.to_string()));
                    window.push_notification("Code copied to clipboard.", cx);
                }),
        )
}

/// A card body in the restricted markdown of [`super::card_markdown`]: no
/// links or images reach it, so the view needs no link handling.
pub(super) fn card_markdown_view(
    state: &Entity<TextViewState>,
    ink: Hsla,
    code_background: Hsla,
    cx: &App,
) -> TextView {
    TextView::new(state)
        .style(code_style(code_background, cx).with_foreground(ink))
        .code_block_actions(copy_code)
        .w_full()
        .min_w_0()
        .text_sm()
        .line_height(relative(1.5))
        .scrollable(false)
        .selectable(true)
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

const FILE_LINK: &str = "else-file:";
const QUOTE_LINK: &str = "else-quote:";

/// The index of the passage a folded quote links to.
fn quoted_passage(url: &str) -> Option<usize> {
    url.strip_prefix(QUOTE_LINK)?
        .parse::<usize>()
        .ok()?
        .checked_sub(1)
}

/// Files as Markdown links, since a Markdown row cannot show file chips.
fn file_links(
    content: &InputContent,
    files: &HashMap<Uuid, Attachment>,
) -> Vec<(std::ops::Range<usize>, String)> {
    content
        .tokens()
        .iter()
        .filter_map(|span| {
            let file = files.get(&file_id(span.token().id())?)?;
            let name: String = file
                .name
                .chars()
                .flat_map(|c| {
                    c.is_ascii_punctuation()
                        .then_some('\\')
                        .into_iter()
                        .chain([c])
                })
                .collect();
            Some((span.range(), format!("[{name}]({FILE_LINK}{})", file.id)))
        })
        .collect()
}

fn linked_file(url: &str) -> Option<Uuid> {
    url.strip_prefix(FILE_LINK)?.parse().ok()
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

/// Whether every link to `url` in Markdown `source` displays `url` itself, so
/// a destination tooltip would repeat the text under the pointer.
fn shows_its_destination(source: &str, url: &str) -> bool {
    use markdown::mdast::Node;
    fn definitions<'a>(node: &'a Node, out: &mut HashMap<&'a str, &'a str>) {
        if let Node::Definition(definition) = node {
            out.entry(&definition.identifier).or_insert(&definition.url);
        }
        for child in node.children().into_iter().flatten() {
            definitions(child, out);
        }
    }
    fn labels(node: &Node, url: &str, defined: &HashMap<&str, &str>, out: &mut Vec<String>) {
        let destination = match node {
            Node::Link(link) => Some(link.url.as_str()),
            Node::LinkReference(link) => defined.get(link.identifier.as_str()).copied(),
            _ => None,
        };
        if destination == Some(url) {
            out.push(node.to_string());
        }
        for child in node.children().into_iter().flatten() {
            labels(child, url, defined, out);
        }
    }
    let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) else {
        return false;
    };
    let mut defined = HashMap::new();
    definitions(&root, &mut defined);
    let mut found = Vec::new();
    labels(&root, url, &defined, &mut found);
    !found.is_empty() && found.iter().all(|label| label == url)
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

/// A block inside a Markdown list item: the source it renders from, how many
/// lists enclose its own, and the item's marker when it is the item's first block.
#[derive(Debug, PartialEq)]
struct ListBlock<'a> {
    content: Range<usize>,
    depth: usize,
    marker: Option<&'a str>,
}

/// The blocks of every list item in Markdown `source`, parsed the way the text
/// view parses it.
fn list_blocks(source: &str) -> Vec<ListBlock<'_>> {
    use markdown::mdast::Node;
    fn visit<'a>(node: &Node, source: &'a str, depth: usize, out: &mut Vec<ListBlock<'a>>) {
        let Some(children) = node.children() else {
            return;
        };
        if let Node::ListItem(item) = node {
            let marker = item.position.as_ref().and_then(|at| {
                let rest = &source[at.start.offset..];
                rest.split([' ', '\t', '\n']).next()
            });
            for (ix, child) in children.iter().enumerate() {
                match (child, child.position()) {
                    (Node::List(_), _) => visit(child, source, depth + 1, out),
                    (_, Some(at)) => out.push(ListBlock {
                        content: at.start.offset..at.end.offset,
                        depth,
                        marker: marker.filter(|_| ix == 0),
                    }),
                    (_, None) => {}
                }
            }
            return;
        }
        for child in children {
            visit(child, source, depth, out);
        }
    }
    let mut out = Vec::new();
    if let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        visit(&root, source, 0, &mut out);
    }
    out
}

/// How a run of table cell text is drawn, which sets the face it is measured in.
#[derive(Debug, Clone, Copy, PartialEq)]
enum CellRun {
    Text,
    Strong,
    Code,
}

/// A table cell's text, run by run.
type TableCell = Vec<(CellRun, String)>;

/// The cells of every Markdown table in `source`, column by column. A header
/// cell's runs are all [`CellRun::Strong`].
fn table_columns(source: &str) -> Vec<Vec<Vec<TableCell>>> {
    use markdown::mdast::Node;
    fn runs(node: &Node, strong: bool, out: &mut TableCell) {
        let text = if strong {
            CellRun::Strong
        } else {
            CellRun::Text
        };
        match node {
            Node::Text(node) => out.push((text, node.value.clone())),
            Node::InlineCode(node) => out.push((CellRun::Code, node.value.clone())),
            Node::Strong(node) => node.children.iter().for_each(|n| runs(n, true, out)),
            _ => match node.children() {
                Some(children) => children.iter().for_each(|n| runs(n, strong, out)),
                None => out.push((text, node.to_string())),
            },
        }
    }
    fn visit(node: &Node, out: &mut Vec<Vec<Vec<TableCell>>>) {
        let Node::Table(table) = node else {
            for child in node.children().into_iter().flatten() {
                visit(child, out);
            }
            return;
        };
        let mut columns: Vec<Vec<TableCell>> = Vec::new();
        for (row_ix, row) in table.children.iter().enumerate() {
            for (ix, cell) in row.children().into_iter().flatten().enumerate() {
                if columns.len() <= ix {
                    columns.push(Vec::new());
                }
                let mut cell_runs = Vec::new();
                runs(cell, row_ix == 0, &mut cell_runs);
                columns[ix].push(cell_runs);
            }
        }
        out.push(columns);
    }
    let mut out = Vec::new();
    if let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        visit(&root, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{
        CellRun, RowFormat, RowSource, file_links, file_map, linked_file, list_blocks,
        plain_content, shows_its_destination, table_columns,
    };
    use crate::app::bot_mention::MentionDisplay;
    use crate::app::bot_mention::{BotMention, MentionRegistry};
    use crate::app::reply::{Author, Draft, MessageReference, Quote};
    use crate::attachments::file_id;
    use else_wire::bots::Bot;
    use else_wire::items::Attachment;
    use else_wire::items::MessagePart;
    use gpui_kit::component::input::InputContent;
    use uuid::Uuid;

    #[test]
    fn destination_tooltips_skip_links_that_show_their_url() {
        let url = "https://example.com/a";
        for (source, expected) in [
            ("see https://example.com/a", true),
            ("see <https://example.com/a>", true),
            ("see [https://example.com/a](https://example.com/a)", true),
            ("see [the docs](https://example.com/a)", false),
            (
                "https://example.com/a or [the docs](https://example.com/a)",
                false,
            ),
            (
                "see [**https://example.com/a**](https://example.com/a)",
                true,
            ),
            ("see https://example.com/b", false),
            ("`https://example.com/a`", false),
            (
                "https://example.com/a and [docs][r]\n\n[r]: https://example.com/a",
                false,
            ),
            (
                "[https://example.com/a][r]\n\n[r]: https://example.com/a",
                true,
            ),
        ] {
            assert_eq!(shows_its_destination(source, url), expected, "{source}");
        }
    }

    #[test]
    fn list_blocks_follow_the_markdown_parse() {
        let source = "Intro\n\n- Forget it\n  1. first\n  lazy\n\n     more\n  10) tenth\n* back\n\n````\n```\n- code\n````\n-\ttab";
        let found: Vec<_> = list_blocks(source)
            .into_iter()
            .map(|block| (&source[block.content], block.depth, block.marker))
            .collect();
        assert_eq!(
            found,
            [
                ("Forget it", 0, Some("-")),
                ("first\n  lazy", 1, Some("1.")),
                ("more", 1, None),
                ("tenth", 1, Some("10)")),
                ("back", 0, Some("*")),
                ("tab", 0, Some("-")),
            ]
        );
    }

    #[test]
    fn table_columns_follow_the_markdown_parse() {
        let source = "Intro | not a table\n\n| | **Wide** head |\n|---|---|\n| a | `code` cell |\n| longer label |\n\n- | x |\n  |---|\n  | y |";
        let (text, strong, code) = (CellRun::Text, CellRun::Strong, CellRun::Code);
        let cell = |runs: &[(CellRun, &str)]| {
            runs.iter()
                .map(|(run, text)| (*run, text.to_string()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            table_columns(source),
            [
                vec![
                    vec![
                        cell(&[]),
                        cell(&[(text, "a")]),
                        cell(&[(text, "longer label")])
                    ],
                    vec![
                        cell(&[(strong, "Wide"), (strong, " head")]),
                        cell(&[(code, "code"), (text, " cell")]),
                    ],
                ],
                vec![vec![cell(&[(strong, "x")]), cell(&[(text, "y")])]],
            ]
        );
    }

    #[test]
    fn a_multi_passage_reply_folds_each_quote_into_one_linked_line() {
        let quote = |text: &str, comment: &str| Quote {
            comment: comment.to_owned(),
            ..Quote::passage(
                MessageReference {
                    item: Uuid::nil(),
                    author: Author::Bot,
                    text: text.to_owned(),
                },
                None,
            )
        };
        let long = "word ".repeat(60);
        let text = Draft {
            content: InputContent::new("Then ship it.\nThanks"),
            quotes: vec![quote(&long, "Too long."), quote("Use *tabs*.\nAlways.", "")],
            ..Draft::default()
        }
        .message()
        .unwrap();
        assert_eq!(RowFormat::for_owner(&text, &[]), RowFormat::OwnerMarkdown);
        let (edits, breaks) = RowFormat::OwnerMarkdown.passages(&text, &mut Vec::new());
        let display = MentionDisplay::new(&text, &[], &edits, &breaks);
        let first = display.text.lines().next().unwrap();
        assert!(first.starts_with("> **1** · [word word"));
        assert!(first.ends_with("…](else-quote:1)"));
        assert!(display.text.contains(
            "\n\nToo long.\n\n> **2** · [Use \\*tabs\\*\\. Always\\.](else-quote:2)\n\n---"
        ));
        assert!(display.text.ends_with("Then ship it.  \nThanks"));
        assert_eq!(super::quoted_passage("else-quote:2"), Some(1));
        assert_eq!(super::quoted_passage("else-quote:0"), None);
    }

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
                quotes: vec![Quote::reply(MessageReference {
                    item: Uuid::nil(), author,
                    text: "Created the routine for **04:00 Europe/Paris**.\n\nPreserve `main`.\n> An earlier quote.".into(),
                })],
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
        let display = MentionDisplay::new(source, &mentions, &[], &breaks);
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
                kinds(&MentionDisplay::new(typed, &[], &[], &breaks).text),
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
            quotes: vec![Quote::reply(MessageReference {
                item: Uuid::nil(),
                author: Author::Bot,
                text: "**Résumé**\n\nPreserve `main`.".into(),
            })],
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
    fn bot_files_render_as_links_to_their_attachments() {
        fn links(node: &markdown::mdast::Node, found: &mut Vec<(String, String)>) {
            if let markdown::mdast::Node::Link(link) = node {
                found.push((node.to_string(), link.url.clone()));
            }
            for child in node.children().into_iter().flatten() {
                links(child, found);
            }
        }
        let file = Attachment {
            id: Uuid::new_v4(),
            name: "daily_counts [utc]*.csv".into(),
            media_type: "text/csv".into(),
            size: 3,
        };
        let text = format!("**Revised:** {} done", file.fallback());
        let parts = vec![
            MessagePart::Text {
                text: "**Revised:** ".into(),
            },
            MessagePart::File { file: file.clone() },
            MessagePart::Text {
                text: " done".into(),
            },
        ];
        let content = plain_content(&text, &parts, &mut Vec::new());
        let display =
            MentionDisplay::new(&text, &[], &file_links(&content, &file_map(&parts)), &[]);
        let mut found = Vec::new();
        links(
            &markdown::to_mdast(&display.text, &markdown::ParseOptions::gfm()).unwrap(),
            &mut found,
        );
        assert_eq!(found.len(), 1, "{}", display.text);
        assert_eq!(found[0].0, file.name);
        assert_eq!(linked_file(&found[0].1), Some(file.id));
        assert_eq!(display.source_bytes(display.text.len()), text.len());
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
