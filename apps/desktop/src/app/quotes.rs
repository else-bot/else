use super::conversation::ConversationView;
use super::reply::{Author, MessageReference, Quote};
use super::{APP_CONTEXT, CommentSelection, ElseApp, QuoteSelection, RowKey};
use gpui_kit::assets::IconName;
use gpui_kit::base::TextSelection;
use gpui_kit::base::input::InputEvent;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Escape, Textarea, TextareaState};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::ops::Range;
use uuid::Uuid;

/// A selection in one message that the owner can quote.
pub(super) struct QuotePrompt {
    bot: Uuid,
    source: MessageReference,
    highlight: Option<Range<usize>>,
    /// Where the drag that made the selection began and ended.
    drag: (Point<Pixels>, Point<Pixels>),
}

impl QuotePrompt {
    /// Above the selection's first line, by the pointer's last position.
    fn above(&self) -> Point<Pixels> {
        let (press, release) = self.drag;
        point(release.x, press.y.min(release.y))
    }

    /// Below the selection's last line.
    fn below(&self) -> Point<Pixels> {
        let (press, release) = self.drag;
        point(release.x, press.y.max(release.y))
    }
}

/// The comment being written for a quote, new or already in the draft.
pub(super) struct QuoteEditor {
    bot: Uuid,
    quote: Quote,
    existing: bool,
    input: Entity<TextareaState>,
    at: Point<Pixels>,
    below: bool,
    full_quote: bool,
    _events: Subscription,
}

// Past these, a card's one-line quote or three-line comment may be cut off.
const LONG_EXCERPT: usize = 40;
const LONG_COMMENT: usize = 150;

impl ElseApp {
    /// After a left click ends, offers to quote a selection made inside one
    /// message of the open conversation.
    pub(super) fn offer_quote(
        &mut self,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Text selection settles in its own mouse-up handler.
        let owner = cx.entity().downgrade();
        window.defer(cx, move |_, cx| {
            let _ = owner.update(cx, |app, cx| {
                let press = app.quote_press.take().unwrap_or(at);
                let prompt = app.selected_quote((press, at), cx);
                if app.quote_prompt.is_some() || prompt.is_some() {
                    app.quote_prompt = prompt;
                    cx.notify();
                }
            });
        });
    }

    fn selected_quote(
        &self,
        drag: (Point<Pixels>, Point<Pixels>),
        cx: &App,
    ) -> Option<QuotePrompt> {
        let bot = self.selected?;
        let view = self.view.as_ref().filter(|view| view.bot_id == bot)?;
        let mut found = None;
        for (key, text) in &view.texts {
            let RowKey::Item(item) = key else { continue };
            if let Some(selection) = text.selection(cx) {
                if found.is_some() {
                    return None;
                }
                found = Some((*item, selection));
            }
        }
        let (item, (text, highlight)) = found?;
        let author = Author::of(&view.conversation.as_ref()?.item(item)?.kind)?;
        Some(QuotePrompt {
            bot,
            source: MessageReference {
                item,
                author,
                text: text.trim().to_owned(),
            },
            highlight,
            drag,
        })
    }

    pub(super) fn quote_selection(
        &mut self,
        comment: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prompt) = self
            .quote_prompt
            .take()
            .filter(|prompt| self.selected == Some(prompt.bot))
        else {
            return;
        };
        TextSelection::clear(window, cx);
        let below = prompt.below();
        let quote = Quote::new(prompt.source, prompt.highlight);
        if comment {
            self.open_quote_editor(prompt.bot, quote, below, window, cx);
        } else {
            self.add_quote(prompt.bot, quote, window, cx);
        }
        cx.notify();
    }

    pub(super) fn add_quote(
        &mut self,
        bot: Uuid,
        quote: Quote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drafts.entry(bot).or_default().quotes.push(quote);
        self.paint_quotes(cx);
        if self.selected == Some(bot) {
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    pub(super) fn remove_quote(
        &mut self,
        bot: Uuid,
        id: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = self.drafts.get_mut(&bot) {
            draft.quotes.retain(|quote| quote.id != id);
        }
        self.expanded_quotes.remove(&id);
        if self.quote_editor.as_ref().is_some_and(|e| e.quote.id == id) {
            self.quote_editor = None;
        }
        self.paint_quotes(cx);
        if self.selected == Some(bot) {
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn clear_quotes(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<_> = self
            .drafts
            .get(&bot)
            .map(|draft| draft.quotes.iter().map(|q| q.id).collect())
            .unwrap_or_default();
        for id in ids {
            self.remove_quote(bot, id, window, cx);
        }
    }

    /// Opens the comment editor below the selection for a new quote, or
    /// above the composer's card for one already in the draft.
    fn open_quote_editor(
        &mut self,
        bot: Uuid,
        quote: Quote,
        at: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let existing = self
            .drafts
            .get(&bot)
            .is_some_and(|d| d.quotes.iter().any(|q| q.id == quote.id));
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .placeholder("What should change here?")
                .auto_grow(3, 10)
                .submit_on_enter(true)
        });
        input.update(cx, |state, cx| {
            state.set_value(quote.comment.clone(), window, cx);
            state.focus_handle(cx).focus(window, cx);
        });
        let _events = cx.subscribe_in(&input, window, |app, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                app.save_quote_comment(window, cx);
            }
        });
        self.quote_editor = Some(QuoteEditor {
            bot,
            quote,
            existing,
            input,
            at,
            below: !existing,
            full_quote: false,
            _events,
        });
        cx.notify();
    }

    /// Saves the comment being written for `bot`'s draft, so sending the
    /// draft neither drops it nor leaves the editor pointing at sent quotes.
    pub(super) fn settle_quote_editor(
        &mut self,
        bot: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .quote_editor
            .as_ref()
            .is_some_and(|editor| editor.bot == bot)
        {
            self.save_quote_comment(window, cx);
        }
    }

    fn save_quote_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.quote_editor.take() else {
            return;
        };
        let comment = editor.input.read(cx).value().trim().to_owned();
        let QuoteEditor {
            bot,
            mut quote,
            existing,
            ..
        } = editor;
        quote.comment = comment;
        if existing {
            if let Some(slot) = self
                .drafts
                .get_mut(&bot)
                .and_then(|d| d.quotes.iter_mut().find(|q| q.id == quote.id))
            {
                slot.comment = quote.comment;
            }
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
            cx.notify();
        } else {
            self.add_quote(bot, quote, window, cx);
        }
    }

    fn cancel_quote_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.quote_editor.take().is_some() {
            self.composer.read(cx).focus_handle(cx).focus(window, cx);
            cx.notify();
        }
    }

    /// Paints every quote of the open conversation's draft over the message
    /// it came from.
    pub(super) fn paint_quotes(&mut self, cx: &mut App) {
        if let Some(view) = self.view.as_mut() {
            paint_view(view, &self.drafts, cx);
        }
    }

    /// Paints the quotes on one row, as when its Markdown finishes parsing.
    pub(super) fn paint_row_quotes(&mut self, key: RowKey, cx: &mut App) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let quotes = self
            .drafts
            .get(&view.bot_id)
            .map_or(&[][..], |d| &d.quotes[..]);
        if let Some(text) = view.texts.get_mut(&key) {
            text.highlight(ranges(quotes, key), highlight_color(cx), cx);
        }
    }

    pub(super) fn render_quote_prompt(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let prompt = self.quote_prompt.as_ref()?;
        if self.selected != Some(prompt.bot) {
            return None;
        }
        let theme = cx.theme();
        let action = |id: &'static str,
                      label: &'static str,
                      icon: IconName,
                      action: &dyn Action,
                      comment: bool| {
            Button::new(id)
                .focus_ring(false)
                .cursor_pointer()
                .ghost()
                .small()
                .icon(icon)
                .label(label)
                .children(
                    Kbd::binding_for_action(action, Some(APP_CONTEXT), window)
                        .map(|kbd| div().flex_none().text_xs().child(kbd.appearance(false))),
                )
                .on_click(
                    cx.listener(move |app, _, window, cx| app.quote_selection(comment, window, cx)),
                )
        };
        Some(
            deferred(
                anchored()
                    .position(prompt.above())
                    .anchor(Anchor::BottomLeft)
                    .offset(point(px(-16.), px(-18.)))
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        h_flex()
                            .id("quote-prompt")
                            .occlude()
                            .on_mouse_down_out(cx.listener(|app, _, _, cx| {
                                if app.quote_prompt.take().is_some() {
                                    cx.notify();
                                }
                            }))
                            .gap_0p5()
                            .p_0p5()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .text_color(theme.popover_foreground)
                            .shadow_lg()
                            .child(action(
                                "quote-selection",
                                "Quote",
                                IconName::Reply,
                                &QuoteSelection,
                                false,
                            ))
                            .child(action(
                                "comment-selection",
                                "Comment",
                                IconName::Pencil,
                                &CommentSelection,
                                true,
                            )),
                    ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    pub(super) fn render_quote_editor(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editor = self.quote_editor.as_ref()?;
        if self.selected != Some(editor.bot) {
            return None;
        }
        let theme = cx.theme();
        let excerpt = editor.quote.source.text.clone();
        let long = excerpt.chars().count() > LONG_COMMENT || excerpt.lines().count() > 3;
        let quote = div()
            .id("quote-editor-excerpt")
            .pl_2()
            .border_l_2()
            .border_color(theme.primary.opacity(0.6))
            .text_sm()
            .text_color(theme.muted_foreground);
        let quote = if editor.full_quote {
            quote
                .max_h(rems(9.))
                .overflow_y_scroll()
                .child(excerpt)
                .into_any_element()
        } else {
            quote
                .line_clamp(3)
                .text_ellipsis()
                .child(editor.quote.excerpt())
                .into_any_element()
        };
        Some(
            deferred(
                anchored()
                    .position(editor.at)
                    .anchor(if editor.below {
                        Anchor::TopLeft
                    } else {
                        Anchor::BottomLeft
                    })
                    .offset(point(
                        px(-16.),
                        if editor.below { px(10.) } else { px(-10.) },
                    ))
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        v_flex()
                            .id("quote-editor")
                            .occlude()
                            .key_context("QuoteEditor")
                            .capture_action(cx.listener(|app, _: &Escape, window, cx| {
                                app.cancel_quote_comment(window, cx);
                                cx.stop_propagation();
                            }))
                            .w(px(380.))
                            .gap_2()
                            .p_2p5()
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .text_color(theme.popover_foreground)
                            .shadow_lg()
                            .child(quote)
                            .when(long, |column| {
                                column.child(
                                    Button::new("quote-editor-more")
                                        .focus_ring(false)
                                        .cursor_pointer()
                                        .ghost()
                                        .xsmall()
                                        .self_start()
                                        .label(if editor.full_quote {
                                            "Collapse quote"
                                        } else {
                                            "Show full quote"
                                        })
                                        .on_click(cx.listener(|app, _, _, cx| {
                                            if let Some(editor) = app.quote_editor.as_mut() {
                                                editor.full_quote = !editor.full_quote;
                                                cx.notify();
                                            }
                                        })),
                                )
                            })
                            .child(
                                div()
                                    .px_2()
                                    .py_1()
                                    .rounded(theme.radius)
                                    .border_1()
                                    .border_color(theme.input)
                                    .bg(theme.background)
                                    .child(
                                        Textarea::new(&editor.input)
                                            .appearance(false)
                                            .aria_label("Comment on the quote"),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .gap_2()
                                    .justify_end()
                                    .child(
                                        Button::new("quote-editor-cancel")
                                            .focus_ring(false)
                                            .cursor_pointer()
                                            .ghost()
                                            .small()
                                            .label("Cancel")
                                            .child(shortcut("Esc"))
                                            .on_click(cx.listener(|app, _, window, cx| {
                                                app.cancel_quote_comment(window, cx)
                                            })),
                                    )
                                    .child(
                                        Button::new("quote-editor-save")
                                            .focus_ring(false)
                                            .cursor_pointer()
                                            .primary()
                                            .small()
                                            .label("Save")
                                            .child(shortcut("Enter"))
                                            .on_click(cx.listener(|app, _, window, cx| {
                                                app.save_quote_comment(window, cx)
                                            })),
                                    ),
                            ),
                    ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    pub(super) fn render_quote_stack(
        &self,
        bot: Uuid,
        bot_name: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let quotes = &self.drafts.get(&bot)?.quotes;
        if quotes.is_empty() {
            return None;
        }
        let theme = cx.theme().clone();
        let numbered = quotes.len() > 1;
        let cards = quotes.iter().enumerate().map(|(ix, quote)| {
            let id = quote.id;
            let expanded = self.expanded_quotes.contains(&id);
            let excerpt = quote.excerpt();
            let long = excerpt.chars().count() > LONG_EXCERPT
                || quote.comment.chars().count() > LONG_COMMENT
                || quote.comment.lines().count() > 1;
            let author = match quote.source.author {
                Author::Owner => "You".to_owned(),
                Author::Bot => bot_name.to_owned(),
            };
            let quoted = if expanded {
                div()
                    .id(SharedString::from(format!("quote-text-{id}")))
                    .max_h(rems(9.))
                    .overflow_y_scroll()
                    .text_color(theme.muted_foreground)
                    .child(quote.source.text.clone())
                    .into_any_element()
            } else {
                h_flex()
                    .min_w_0()
                    .gap_1p5()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(IconName::Reply).xsmall().flex_none())
                    .child(
                        div()
                            .flex_none()
                            .max_w(relative(0.4))
                            .truncate()
                            .text_color(theme.foreground)
                            .child(author),
                    )
                    .child(div().flex_none().child("·"))
                    .child(div().flex_1().min_w_0().truncate().child(excerpt))
                    .into_any_element()
            };
            let comment = (!quote.comment.is_empty()).then(|| {
                let text = div().text_color(theme.foreground);
                if expanded {
                    text.child(quote.comment.clone())
                } else {
                    text.line_clamp(3)
                        .text_ellipsis()
                        .child(super::reply::flatten(&quote.comment))
                }
            });
            h_flex()
                .id(SharedString::from(format!("quote-{id}")))
                .w_full()
                .items_start()
                .gap_2()
                .pl_2()
                .pr_1()
                .py_1p5()
                .rounded(theme.radius)
                .border_l_2()
                .border_color(theme.primary)
                .bg(theme.muted)
                .text_sm()
                .when(numbered, |row| {
                    row.child(
                        div()
                            .flex_none()
                            .size_5()
                            .rounded_full()
                            .bg(theme.primary.opacity(0.18))
                            .text_color(theme.primary)
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child((ix + 1).to_string()),
                    )
                })
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(quoted)
                        .children(comment)
                        .when(long, |column| {
                            column.child(
                                Button::new(SharedString::from(format!("quote-more-{id}")))
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .ghost()
                                    .xsmall()
                                    .self_start()
                                    .label(if expanded { "Show less" } else { "Show more" })
                                    .on_click(cx.listener(move |app, _, _, cx| {
                                        if !app.expanded_quotes.remove(&id) {
                                            app.expanded_quotes.insert(id);
                                        }
                                        cx.notify();
                                    })),
                            )
                        }),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .gap_0p5()
                        .child(
                            Button::new(SharedString::from(format!("quote-edit-{id}")))
                                .focus_ring(false)
                                .cursor_pointer()
                                .ghost()
                                .xsmall()
                                .icon(IconName::Pencil)
                                .accessibility_label(if quote.comment.is_empty() {
                                    "Add a comment"
                                } else {
                                    "Edit comment"
                                })
                                .on_click(cx.listener(
                                    move |app, event: &ClickEvent, window, cx| {
                                        let Some(quote) = app
                                            .drafts
                                            .get(&bot)
                                            .and_then(|d| d.quotes.iter().find(|q| q.id == id))
                                            .cloned()
                                        else {
                                            return;
                                        };
                                        let at = event.position();
                                        app.open_quote_editor(bot, quote, at, window, cx);
                                    },
                                )),
                        )
                        .child(
                            Button::new(SharedString::from(format!("quote-remove-{id}")))
                                .focus_ring(false)
                                .cursor_pointer()
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .accessibility_label("Remove quote")
                                .on_click(cx.listener(move |app, _, window, cx| {
                                    app.remove_quote(bot, id, window, cx)
                                })),
                        ),
                )
        });
        Some(
            v_flex()
                .pt_1()
                .gap_1()
                .when(numbered, |column| {
                    column.child(
                        h_flex()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(
                                div()
                                    .flex_1()
                                    .child(format!("{} quoted passages", quotes.len())),
                            )
                            .child(
                                Button::new("clear-quotes")
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .ghost()
                                    .xsmall()
                                    .label("Clear all")
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.clear_quotes(bot, window, cx)
                                    })),
                            ),
                    )
                })
                .child(
                    v_flex()
                        .id("quote-stack")
                        .max_h(px(300.))
                        .overflow_y_scroll()
                        .gap_1()
                        .children(cards),
                )
                .into_any_element(),
        )
    }
}

fn shortcut(keys: &'static str) -> impl IntoElement {
    div().flex_none().text_xs().opacity(0.7).child(keys)
}

fn highlight_color(cx: &App) -> Hsla {
    cx.theme().primary.opacity(0.22)
}

fn ranges(quotes: &[Quote], key: RowKey) -> Vec<Range<usize>> {
    quotes
        .iter()
        .filter(|quote| RowKey::Item(quote.source.item) == key)
        .filter_map(|quote| quote.highlight.clone())
        .collect()
}

pub(super) fn paint_view(
    view: &mut ConversationView,
    drafts: &std::collections::HashMap<Uuid, super::reply::Draft>,
    cx: &mut App,
) {
    let quotes = drafts.get(&view.bot_id).map_or(&[][..], |d| &d.quotes[..]);
    let color = highlight_color(cx);
    for (key, text) in view.texts.iter_mut() {
        text.highlight(ranges(quotes, *key), color, cx);
    }
}
