use std::ops::Range;

use gpui_kit::component::input::{Enter, IndentInline, MoveDown, MoveUp, TextareaState};
use gpui_kit::component::{ActiveTheme as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::ElseApp;
use crate::api::Bot;
use crate::avatar::bot_avatar;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MentionQuery {
    range: Range<usize>,
    text: String,
}

impl MentionQuery {
    fn at(text: &str, selection: Range<usize>) -> Option<Self> {
        if !selection.is_empty() {
            return None;
        }
        let caret = selection.start;
        let before = text.get(..caret)?;
        let start = before.rfind('@')?;
        if !mention_boundary(&before[..start]) {
            return None;
        }
        let query = &before[start + 1..];
        if query.is_empty() || !query.chars().all(mention_character) {
            return None;
        }
        let suffix = &text[caret..];
        let end = caret
            + suffix
                .find(|c| !mention_character(c))
                .unwrap_or(suffix.len());
        Some(Self {
            range: start..end,
            text: query.to_lowercase(),
        })
    }

    fn candidates(&self, bots: &[Bot]) -> Vec<Uuid> {
        bots.iter()
            .filter(|bot| bot.is_active() && bot.name.to_lowercase().contains(&self.text))
            .map(|bot| bot.id)
            .collect()
    }
}

pub(super) fn mention_boundary(before: &str) -> bool {
    before
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || matches!(c, '(' | '[' | '{' | '"' | '\''))
}

fn mention_character(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

#[derive(Default)]
pub(super) enum MentionMenu {
    #[default]
    Closed,
    Dismissed(Entity<TextareaState>, MentionQuery),
    Open(MentionPicker),
}

pub(super) struct MentionPicker {
    /// The composer, or the note of the quote being commented on.
    input: Entity<TextareaState>,
    query: MentionQuery,
    candidates: Vec<Uuid>,
    selected: usize,
    scroll: UniformListScrollHandle,
}

#[derive(Clone, Copy)]
pub(super) enum MentionAction {
    Previous,
    Next,
    Accept,
    Dismiss,
}

impl ElseApp {
    /// The focused input that mentions bots: the quote note or the composer.
    fn mention_input(&self, window: &Window, cx: &App) -> Option<Entity<TextareaState>> {
        let note = self.quote_editor.as_ref().map(|editor| editor.input());
        note.into_iter()
            .chain([&self.composer])
            .find(|input| input.read(cx).focus_handle(cx).is_focused(window))
            .cloned()
    }

    pub(super) fn sync_mentions(&mut self, window: &Window, cx: &mut Context<Self>) {
        let query = self.mention_input(window, cx).and_then(|input| {
            let state = input.read(cx);
            let query = MentionQuery::at(&state.value(), state.selected_range())?;
            Some((input, query))
        });
        let Some((input, query)) = query else {
            if !matches!(self.mentions, MentionMenu::Closed) {
                self.mentions = MentionMenu::Closed;
                cx.notify();
            }
            return;
        };
        let candidates = query.candidates(&self.bots);
        match &mut self.mentions {
            MentionMenu::Dismissed(dismissed, previous)
                if *dismissed == input && *previous == query =>
            {
                return;
            }
            MentionMenu::Open(picker)
                if picker.input == input
                    && picker.query == query
                    && picker.candidates == candidates =>
            {
                return;
            }
            _ => {}
        }
        self.mentions = if candidates.is_empty() {
            MentionMenu::Closed
        } else {
            MentionMenu::Open(MentionPicker {
                input,
                query,
                candidates,
                selected: 0,
                scroll: UniformListScrollHandle::new(),
            })
        };
        cx.notify();
    }

    /// Applies `action` to the open picker; false when no picker took it.
    pub(super) fn mention_action(
        &mut self,
        action: MentionAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(input) = self.mention_input(window, cx) else {
            return false;
        };
        if input.update(cx, |state, cx| {
            state.marked_text_range(window, cx).is_some()
        }) {
            return false;
        }
        self.sync_mentions(window, cx);
        let MentionMenu::Open(picker) = &mut self.mentions else {
            return false;
        };
        match action {
            MentionAction::Previous | MentionAction::Next => {
                let count = picker.candidates.len();
                picker.selected = if matches!(action, MentionAction::Next) {
                    (picker.selected + 1) % count
                } else {
                    (picker.selected + count - 1) % count
                };
                picker
                    .scroll
                    .scroll_to_item(picker.selected, ScrollStrategy::Nearest);
                cx.notify();
            }
            MentionAction::Accept => {
                let bot = picker.candidates[picker.selected];
                self.insert_mention(bot, window, cx);
            }
            MentionAction::Dismiss => {
                self.mentions = MentionMenu::Dismissed(picker.input.clone(), picker.query.clone());
                cx.notify();
            }
        }
        cx.stop_propagation();
        true
    }

    fn insert_mention(&mut self, id: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let MentionMenu::Open(picker) = &self.mentions else {
            return;
        };
        let input = picker.input.clone();
        let state = input.read(cx);
        if MentionQuery::at(&state.value(), state.selected_range()).as_ref() != Some(&picker.query)
        {
            self.sync_mentions(window, cx);
            return;
        }
        let Some(bot) = self.bots.iter().find(|bot| bot.id == id && bot.is_active()) else {
            return;
        };
        let replacement = format!("@{} ", bot.name);
        let range = picker.query.range.clone();
        self.mentions = MentionMenu::Closed;
        input.update(cx, |state, cx| {
            state.set_selected_range(range, cx);
            state.replace(replacement, window, cx);
            state.focus_handle(cx).focus(window, cx);
        });
        cx.notify();
    }

    /// The picker for `input`, above or below the element that holds it.
    pub(super) fn render_mentions(
        &self,
        input: &Entity<TextareaState>,
        above: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let MentionMenu::Open(picker) = &self.mentions else {
            return None;
        };
        if picker.input != *input {
            return None;
        }
        let count = picker.candidates.len();
        let theme = cx.theme();
        Some(
            deferred(
                div()
                    .absolute()
                    .when(above, |menu| menu.bottom(relative(1.)).pb_2())
                    .when(!above, |menu| menu.top(relative(1.)).pt_2())
                    .left_0()
                    .w_full()
                    .child(
                        div()
                            .id("mention-menu")
                            .occlude()
                            .role(Role::ListBox)
                            .aria_label("Mention a bot")
                            .on_mouse_down_out(cx.listener(|app, _, _, cx| {
                                if let MentionMenu::Open(picker) = &app.mentions {
                                    app.mentions = MentionMenu::Dismissed(
                                        picker.input.clone(),
                                        picker.query.clone(),
                                    );
                                    cx.notify();
                                }
                            }))
                            .p_1()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .text_color(theme.popover_foreground)
                            .shadow_lg()
                            .child(
                                uniform_list(
                                    "mention-options",
                                    count,
                                    cx.processor(|app, range: Range<usize>, _, cx| {
                                        let MentionMenu::Open(picker) = &app.mentions else {
                                            return Vec::new();
                                        };
                                        range
                                            .filter_map(|ix| {
                                                let id = picker.candidates[ix];
                                                let bot =
                                                    app.bots.iter().find(|bot| bot.id == id)?;
                                                Some(app.render_mention_option(
                                                    bot,
                                                    ix == picker.selected,
                                                    cx,
                                                ))
                                            })
                                            .collect()
                                    }),
                                )
                                .h(px(count.min(6) as f32 * 44.))
                                .w_full()
                                .track_scroll(&picker.scroll),
                            ),
                    ),
            )
            // Over the quote editor when it holds the picker.
            .with_priority(3)
            .into_any_element(),
        )
    }

    fn render_mention_option(
        &self,
        bot: &Bot,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let id = bot.id;
        h_flex()
            .id(SharedString::from(format!("mention-{id}")))
            .role(Role::ListBoxOption)
            .aria_label(bot.name.clone())
            .aria_selected(selected)
            .w_full()
            .h(px(44.))
            .px_3()
            .gap_3()
            .items_center()
            .rounded(theme.radius)
            .cursor_pointer()
            .bg(if selected {
                theme.accent
            } else {
                theme.popover
            })
            .hover(|row| row.bg(theme.accent))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |app, _, window, cx| app.insert_mention(id, window, cx)))
            .child(
                bot_avatar(
                    SharedString::from(format!("mention-avatar-{id}")),
                    bot.avatar,
                    px(26.),
                )
                .still(),
            )
            .child(div().flex_1().min_w_0().truncate().child(bot.name.clone()))
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Bot"),
            )
            .into_any_element()
    }
}

/// Lets the open picker take the arrow keys, Enter and Tab before the input
/// inside `element` does.
pub(super) fn mention_keys<E: InteractiveElement>(element: E, cx: &mut Context<ElseApp>) -> E {
    element
        .capture_action(cx.listener(|app, _: &MoveDown, window, cx| {
            app.mention_action(MentionAction::Next, window, cx);
        }))
        .capture_action(cx.listener(|app, _: &MoveUp, window, cx| {
            app.mention_action(MentionAction::Previous, window, cx);
        }))
        .capture_action(cx.listener(|app, action: &Enter, window, cx| {
            if !action.shift && !action.secondary {
                app.mention_action(MentionAction::Accept, window, cx);
            }
        }))
        .capture_action(cx.listener(|app, _: &IndentInline, window, cx| {
            app.mention_action(MentionAction::Accept, window, cx);
        }))
}

#[cfg(test)]
mod tests {
    use super::MentionQuery;
    use uuid::Uuid;

    #[test]
    fn opens_after_the_first_character_at_a_mention_boundary() {
        for text in ["@s", "Hey @s", "Hello\n@日", "(@s", "@Scout @s"] {
            assert!(
                MentionQuery::at(text, text.len()..text.len()).is_some(),
                "{text}"
            );
        }
        for text in ["@", "Hey @", "a@scout", "hi@example.com", "@s ", "@@s"] {
            assert!(
                MentionQuery::at(text, text.len()..text.len()).is_none(),
                "{text}"
            );
        }
        assert!(MentionQuery::at("@scout", 1..3).is_none());
    }

    #[test]
    fn replaces_only_the_mention_at_the_caret_with_unicode_and_a_suffix() {
        let mut text = "Héllo @scout, help @todo".to_owned();
        let caret = "Héllo @sc".len();
        let query = MentionQuery::at(&text, caret..caret).unwrap();
        assert_eq!(query.text, "sc");
        text.replace_range(query.range, "@Scout Bot ");
        assert_eq!(text, "Héllo @Scout Bot , help @todo");
    }

    #[test]
    fn filters_active_bots_case_insensitively_including_multiword_names() {
        let bot = |id, name, cleanup| {
            serde_json::from_value(serde_json::json!({
                "id": Uuid::from_u128(id), "name": name, "cleanup": cleanup,
            }))
            .unwrap()
        };
        let bots = vec![
            bot(1, "Proliquid Scout", serde_json::Value::Null),
            bot(2, "Todo", serde_json::Value::Null),
            bot(
                3,
                "Scout removed",
                serde_json::json!({"state": "done", "error": null}),
            ),
            bot(4, "SCOUT", serde_json::Value::Null),
        ];
        let query = MentionQuery::at("Hey @Sc", 7..7).unwrap();
        assert_eq!(
            query.candidates(&bots),
            vec![Uuid::from_u128(1), Uuid::from_u128(4)]
        );
        let query = MentionQuery::at("@missing", 8..8).unwrap();
        assert!(query.candidates(&bots).is_empty());
    }
}
