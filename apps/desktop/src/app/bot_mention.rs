use else_wire::bots::Avatar;
use std::ops::Range;

use gpui_kit::base::text::{
    InlineElement, InlineRenderContext, MarkdownNode, MarkdownParseContext, MarkdownPlugin,
};
use gpui_kit::component::h_flex;
use gpui_kit::component::input::{InlineToken, InputContent, Textarea, TextareaState};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use markdown::mdast::Node;
use uuid::Uuid;

use super::ElseApp;
use crate::avatar::bot_avatar;

pub(super) use else_client::mention::Registry as MentionRegistry;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct BotMention {
    pub range: Range<usize>,
    id: Uuid,
    name: String,
    text: String,
    avatar: Avatar,
}

impl From<else_client::mention::Mention> for BotMention {
    fn from(found: else_client::mention::Mention) -> Self {
        Self {
            range: found.range,
            id: found.id,
            name: found.name,
            text: found.text,
            avatar: found.avatar,
        }
    }
}

impl BotMention {
    pub(super) fn find(source: &str, bots: &MentionRegistry) -> Vec<Self> {
        else_client::mention::find(source, bots)
            .into_iter()
            .map(Self::from)
            .collect()
    }

    pub(super) fn in_markdown(source: &str, bots: &MentionRegistry) -> Vec<Self> {
        else_client::mention::find_in_markdown(source, bots)
            .into_iter()
            .map(Self::from)
            .collect()
    }

    pub(super) fn content(source: &str, mentions: &[Self]) -> InputContent {
        mentions
            .iter()
            .fold(InputContent::new(source), |content, mention| {
                content
                    .with_token(
                        mention.range.clone(),
                        InlineToken::new(
                            mention.id.to_string(),
                            source[mention.range.clone()].to_owned(),
                        )
                        .with_label(mention.name.clone()),
                    )
                    .expect("bot mentions have disjoint ranges at complete character boundaries")
            })
    }

    /// The mentions in text being edited, leaving out a name that touches
    /// the selection: it may still be typed into a longer name.
    fn settled(text: &str, selection: Range<usize>, bots: &MentionRegistry) -> Vec<Self> {
        let mut mentions = Self::find(text, bots);
        mentions.retain(|mention| {
            mention.range.end < selection.start || mention.range.start >= selection.end
        });
        mentions
    }

    /// Turns the settled bot names in `input`'s text into chips, keeping its
    /// text, caret and undo history.
    pub(super) fn chip_input(input: &Entity<TextareaState>, bots: &MentionRegistry, cx: &mut App) {
        input.update(cx, |state, cx| {
            let text = state.value();
            let mentions = Self::settled(&text, state.selected_range(), bots);
            // Rejected while composing or when another caret cuts a name;
            // the next change or caret move chips it.
            let _ = state.set_tokens(Self::content(&text, &mentions), cx);
        });
    }

    /// Draws the tokens of `textarea` as the chips of `mentions`.
    pub(super) fn chips(textarea: Textarea, mentions: Vec<Self>) -> Textarea {
        textarea.token(move |token, _, cx| {
            match mentions
                .iter()
                .find(|mention| mention.token_id() == token.token().id().as_ref())
            {
                Some(mention) => {
                    mention.token_element(token.line_height(), token.is_selected(), false, cx)
                }
                None => div().child(token.token().text().clone()).into_any_element(),
            }
        })
    }

    pub(super) fn token_element(
        &self,
        height: Pixels,
        selected: bool,
        clickable: bool,
        cx: &App,
    ) -> AnyElement {
        let element = self
            .element(height)
            .when(clickable, |chip| chip.cursor_pointer())
            .into_any_element();
        if selected {
            use gpui_kit::component::ActiveTheme as _;
            div()
                .bg(cx.theme().selection)
                .child(element)
                .into_any_element()
        } else {
            element
        }
    }

    pub(super) fn token_id(&self) -> String {
        self.id.to_string()
    }

    pub(super) fn bot(&self) -> Uuid {
        self.id
    }

    fn element(&self, height: Pixels) -> Stateful<Div> {
        h_flex()
            .id(SharedString::from(format!(
                "bot-mention-{}-{}",
                self.id, self.range.start
            )))
            .h(height)
            .gap_1()
            .items_center()
            .font_weight(FontWeight::BOLD)
            .child(bot_avatar("mention-avatar", self.avatar, px(18.)).still())
            .child(self.name.clone())
    }
}

#[derive(Default)]
pub(super) struct MentionDisplay {
    pub text: String,
    replacements: Vec<(Range<usize>, Range<usize>)>,
}

impl MentionDisplay {
    pub(super) fn new(
        source: &str,
        mentions: &[BotMention],
        links: &[(Range<usize>, String)],
        hard_breaks: &[usize],
    ) -> Self {
        let mut edits: Vec<_> = mentions
            .iter()
            .map(|mention| {
                (
                    mention.range.clone(),
                    format!("[bot](bot-mention:{}:{})", mention.id, mention.range.start),
                )
            })
            .chain(links.iter().cloned())
            .chain(hard_breaks.iter().map(|&at| (at..at, "  ".to_owned())))
            .collect();
        edits.sort_by_key(|(range, _)| range.start);
        let mut display = Self::default();
        let mut end = 0;
        for (range, replacement) in edits {
            display.text.push_str(&source[end..range.start]);
            let start = display.text.len();
            display.text.push_str(&replacement);
            display
                .replacements
                .push((range.clone(), start..display.text.len()));
            end = range.end;
        }
        display.text.push_str(&source[end..]);
        display
    }

    pub(super) fn source_bytes(&self, displayed: usize) -> usize {
        let (mut source_end, mut display_end) = (0, 0);
        for (source, display) in &self.replacements {
            if displayed < display.start {
                break;
            }
            if displayed < display.end {
                return source.start;
            }
            source_end = source.end;
            display_end = display.end;
        }
        source_end + displayed - display_end
    }
}

#[derive(Clone)]
pub(super) struct MentionPlugin {
    pub mentions: Vec<BotMention>,
    pub owner: WeakEntity<ElseApp>,
}

impl MarkdownPlugin for MentionPlugin {
    fn name(&self) -> &str {
        "bot-mention"
    }

    fn parse(&self, node: &Node, _: &MarkdownParseContext<'_>) -> Option<MarkdownNode> {
        let Node::Link(link) = node else { return None };
        let (id, offset) = link.url.strip_prefix("bot-mention:")?.split_once(':')?;
        let key = (id.parse::<Uuid>().ok()?, offset.parse::<usize>().ok()?);
        let mention = self
            .mentions
            .iter()
            .find(|mention| (mention.id, mention.range.start) == key)?;
        Some(MarkdownNode::new("bot-mention", key).text(mention.text.clone()))
    }

    fn render_inline(
        &self,
        node: &MarkdownNode,
        context: &InlineRenderContext,
        _: &mut Window,
        _: &mut App,
    ) -> Option<InlineElement> {
        let key = node.data::<(Uuid, usize)>()?;
        let mention = self
            .mentions
            .iter()
            .find(|mention| (mention.id, mention.range.start) == *key)?;
        let (owner, bot) = (self.owner.clone(), mention.id);
        Some(InlineElement::new(
            mention
                .element(context.line_height())
                .cursor_pointer()
                // Keeps the press from starting a text selection.
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(move |_, window, cx| {
                    let _ = owner.update(cx, |app, cx| app.open_bot(bot, window, cx));
                }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{BotMention, MentionDisplay, MentionRegistry};
    use else_wire::bots::Bot;
    use uuid::Uuid;

    fn bot(id: u128, name: &str) -> Bot {
        serde_json::from_value(serde_json::json!({"id": Uuid::from_u128(id), "name": name}))
            .unwrap()
    }

    #[test]
    fn full_names_match_at_boundaries_and_the_longest_name_wins() {
        let bots = MentionRegistry::new(&[bot(1, "Scout"), bot(2, "Scout Comp"), bot(3, "日報")]);
        let source = "Hi @scout comp, compare with @日報. Email me@Scout and @Scouting stay plain.";
        let matches = BotMention::find(source, &bots);
        assert_eq!(
            matches.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![Uuid::from_u128(2), Uuid::from_u128(3)]
        );
        assert_eq!(&source[matches[0].range.clone()], "@scout comp");
        assert_eq!(&source[matches[1].range.clone()], "@日報");
        let content = BotMention::content(source, &matches);
        assert_eq!(content.text().as_ref(), source);
        assert_eq!(content.tokens()[0].token().text().as_ref(), "@scout comp");
        assert_eq!(content.tokens()[0].token().label().as_ref(), "Scout Comp");
    }

    #[test]
    fn unknown_deleted_partial_and_grapheme_prefixes_stay_plain() {
        let mut deleted = bot(2, "Removed");
        deleted.cleanup =
            Some(serde_json::from_value(serde_json::json!({"state":"done","error":null})).unwrap());
        let bots = MentionRegistry::new(&[
            bot(1, "Scout"),
            bot(3, "e"),
            bot(4, "Line\nBreak"),
            bot(5, "Tab\tName"),
            bot(6, "Line\u{2028}Name"),
            deleted,
        ]);
        for source in [
            "@Missing",
            "@Removed",
            "@Sco",
            "name@Scout",
            "@Scoutish",
            "@e\u{301}",
            "@Line\nBreak",
            "@Tab\tName",
            "@Line\u{2028}Name",
        ] {
            assert!(BotMention::find(source, &bots).is_empty(), "{source}");
        }
        assert_eq!(BotMention::find("(@Scout)! @Scout", &bots).len(), 2);
    }

    #[test]
    fn markdown_only_decorates_prose_and_keeps_code_and_links_intact() {
        let bots = MentionRegistry::new(&[bot(1, "Scout Comp")]);
        let source = "Ask **@Scout Comp**. `@Scout Comp`\n\n```text\n@Scout Comp\n```\n\n[visit @Scout Comp](https://example.test/@Scout) and \\@Scout Comp.";
        let matches = BotMention::in_markdown(source, &bots);
        assert_eq!(matches.len(), 1);
        assert_eq!(&source[matches[0].range.clone()], "@Scout Comp");
        let display = MentionDisplay::new(source, &matches, &[], &[]);
        assert!(display.text.contains("**[bot](bot-mention:"));
        assert!(display.text.contains("`@Scout Comp`"));
        assert!(
            display
                .text
                .contains("[visit @Scout Comp](https://example.test/@Scout)")
        );
    }

    #[test]
    fn display_offsets_map_back_to_original_message_bytes() {
        let source = "Hé @Scout and @日報 finish.";
        let matches = BotMention::find(
            source,
            &MentionRegistry::new(&[bot(1, "Scout"), bot(2, "日報")]),
        );
        let display = MentionDisplay::new(source, &matches, &[], &[]);
        assert_eq!(display.source_bytes(0), 0);
        for (original, replacement) in &display.replacements {
            assert_eq!(display.source_bytes(replacement.start), original.start);
            assert_eq!(display.source_bytes(replacement.start + 3), original.start);
            assert_eq!(display.source_bytes(replacement.end), original.end);
        }
        assert_eq!(display.source_bytes(display.text.len()), source.len());
        let plain = MentionDisplay::new("no mentions ✓", &[], &[], &[]);
        assert_eq!(plain.source_bytes(plain.text.len()), "no mentions ✓".len());
    }

    #[test]
    fn a_name_touching_the_caret_stays_text_until_the_owner_moves_on() {
        let bots = MentionRegistry::new(&[bot(1, "Tech"), bot(2, "Tech Manager")]);
        let typing = "Report to @Tech";
        assert!(BotMention::settled(typing, 15..15, &bots).is_empty());
        let picked = "Report to @Tech Manager now";
        let settled = BotMention::settled(picked, 24..24, &bots);
        assert_eq!(settled.len(), 1);
        assert_eq!(&picked[settled[0].range.clone()], "@Tech Manager");
        assert_eq!(BotMention::settled(picked, 0..0, &bots).len(), 1);
        assert!(BotMention::settled(picked, 12..12, &bots).is_empty());
    }

    #[test]
    fn ambiguous_names_stay_plain_instead_of_showing_an_arbitrary_avatar() {
        let bots = MentionRegistry::new(&[bot(1, "Scout"), bot(2, "SCOUT"), bot(3, "Todo")]);
        let source = "Ask @Scout and @Todo";
        let matches = BotMention::find(source, &bots);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].id, Uuid::from_u128(3));
        assert_eq!(
            BotMention::content(source, &matches).text().as_ref(),
            source
        );
    }
}
