//! The dialog with a bot's rules file and its history.

use crate::controls::{Control as _, Scale, Tier, segment, segment_track};

use super::{dot, when};
use crate::app::ElseApp;
use else_wire::rules::{DiffLine, DiffOp, RuleAuthor, RulesFile, RulesVersion};
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::app) enum RulesTab {
    #[default]
    Rules,
    History,
}

pub(in crate::app) struct RulesDialog {
    bot: Uuid,
    tab: RulesTab,
    focus: Option<String>,
    version: Option<i64>,
    scroll: ScrollHandle,
}

/// A click handler for dialog content, which renders outside the app's
/// own context.
fn on<E: ?Sized>(
    owner: &WeakEntity<ElseApp>,
    f: impl Fn(&mut ElseApp, &mut Window, &mut Context<ElseApp>) + 'static,
) -> impl Fn(&E, &mut Window, &mut App) + 'static {
    let owner = owner.clone();
    move |_, window, cx| {
        let _ = owner.update(cx, |app, cx| f(app, window, cx));
    }
}

/// One line of a rule as wrapping text: a leading `Label:` reads bold and
/// backtick spans as code.
fn rule_line(line: &str, cx: &App) -> StyledText {
    let theme = cx.theme();
    let mut text = String::with_capacity(line.len());
    let mut highlights = Vec::new();
    let mut rest = line;
    if let Some((label, after)) = line.split_once(": ")
        && !label.is_empty()
        && label.len() <= 16
        && label.chars().all(|c| c.is_alphabetic() || c == ' ')
    {
        text.push_str(label);
        text.push_str(": ");
        highlights.push((
            0..label.len() + 1,
            HighlightStyle {
                font_weight: Some(FontWeight::SEMIBOLD),
                ..Default::default()
            },
        ));
        rest = after;
    }
    for (ix, span) in rest.split('`').enumerate() {
        let start = text.len();
        text.push_str(span);
        if ix % 2 == 1 && !span.is_empty() {
            highlights.push((
                start..text.len(),
                HighlightStyle {
                    background_color: Some(theme.secondary),
                    ..Default::default()
                },
            ));
        }
    }
    StyledText::new(text).with_highlights(highlights)
}

fn diff_block(diff: &[DiffLine], cx: &App) -> Div {
    let theme = cx.theme();
    // The same surface and radius as a code block in the transcript, with
    // 12 px of padding on every side and 12.5 px mono at 1.6 line height.
    v_flex()
        .w_full()
        .flex_none()
        .p_3()
        .rounded(theme.radius)
        .bg(theme.secondary)
        .overflow_hidden()
        .font_family(theme.mono_font_family.clone())
        .text_size(px(12.5))
        .line_height(px(20.))
        .children(diff.iter().map(|line| {
            // The band and the sign carry the colour; the text stays in the
            // body colour so it reads on the tint.
            let (sign, color, text, fill) = match line.op {
                DiffOp::Same | DiffOp::Unknown => {
                    (" ", theme.muted_foreground, theme.muted_foreground, None)
                }
                DiffOp::Add => (
                    "+",
                    theme.success,
                    theme.foreground,
                    Some(theme.success.opacity(0.11)),
                ),
                DiffOp::Remove => (
                    "-",
                    theme.danger,
                    theme.foreground,
                    Some(theme.danger.opacity(0.11)),
                ),
            };
            // A changed line: an 11% tint with a 2 px gutter bar in its
            // colour. Unchanged lines keep the bar's space so text aligns.
            h_flex()
                .w_full()
                .pl(px(6.))
                .pr_2()
                .gap_2()
                .items_start()
                .border_l_2()
                .border_color(if fill.is_some() {
                    color
                } else {
                    transparent_black()
                })
                .when_some(fill, |row, fill| row.bg(fill))
                .text_color(text)
                .child(div().flex_none().text_color(color).child(sign))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
                        .child(line.text.clone()),
                )
        }))
}

/// The tallest a tab's body grows before it scrolls; shorter content
/// leaves the dialog shorter.
const BODY_MAX: f32 = 380.;
/// The shortest a tab's body gets: enough for a version or two, so a short
/// diff leaves no dead space under it.
const BODY_MIN: f32 = 120.;

fn quote(words: &str, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .w_full()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(format!("From your message: \u{201c}{words}\u{201d}"))
}

impl ElseApp {
    pub(in crate::app) fn open_rules(
        &mut self,
        bot: Uuid,
        focus: Option<String>,
        tab: RulesTab,
        version: Option<i64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scroll = ScrollHandle::new();
        self.rules_dialog = Some(RulesDialog {
            bot,
            tab,
            focus,
            version,
            scroll,
        });
        if tab == RulesTab::Rules {
            self.mark_rules_seen(bot, cx);
        }
        let owner = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            // One 24 px inset for the title, body and footer.
            let dialog = crate::appearance::modal_surface(dialog, cx)
                // Wide enough that its edges clear the sidebar and the
                // Details panel divider instead of resting on them.
                .w(px(880.))
                .p_6()
                .centered();
            let Some(app) = owner.upgrade() else {
                return dialog;
            };
            let app = app.read(cx);
            let name = app
                .bots
                .iter()
                .find(|b| b.id == bot)
                .map_or_else(|| "Bot".to_owned(), |b| b.name.clone());
            let content = app.render_rules_dialog(bot, &name, &owner, cx);
            // The close button shares the title's line, centred on it, at
            // the kit's usual distance from the dialog's right edge.
            dialog
                .close_button(false)
                .title(
                    h_flex()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_lg()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(format!("{name} rules")),
                        )
                        .child(
                            Button::new("rules-close")
                                .focus_ring(false)
                                .cursor_pointer()
                                .small()
                                .ghost()
                                .mr(px(-10.))
                                .icon(IconName::Close)
                                .accessibility_label("Close")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        ),
                )
                .child(content)
        });
        self.scroll_to_focus();
        cx.notify();
    }

    fn scroll_to_focus(&self) {
        let Some(dialog) = &self.rules_dialog else {
            return;
        };
        let (Some(focus), Some(file)) = (&dialog.focus, self.rules_file(dialog.bot)) else {
            return;
        };
        if let Some(ix) = file
            .sections
            .iter()
            .position(|s| s.title.eq_ignore_ascii_case(focus))
        {
            dialog.scroll.scroll_to_item(ix);
        }
    }

    fn set_rules_tab(&mut self, tab: RulesTab, cx: &mut Context<Self>) {
        let Some(dialog) = &mut self.rules_dialog else {
            return;
        };
        dialog.tab = tab;
        let bot = dialog.bot;
        if tab == RulesTab::Rules {
            self.mark_rules_seen(bot, cx);
        }
        cx.notify();
    }

    fn pick_rules_version(&mut self, version: i64, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.rules_dialog {
            dialog.version = Some(version);
        }
        cx.notify();
    }

    fn ask_to_edit(
        &mut self,
        section: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = match section {
            Some(title) => format!("Change the rule \"{title}\": "),
            None => "Change your rules: ".to_owned(),
        };
        window.close_dialog(cx);
        let end = text.len();
        self.composer.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.set_selected_range(end..end, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn render_rules_dialog(
        &self,
        bot: Uuid,
        name: &str,
        owner: &WeakEntity<ElseApp>,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(dialog) = self.rules_dialog.as_ref().filter(|d| d.bot == bot) else {
            return div().into_any_element();
        };
        let tab = dialog.tab;
        let Some(file) = self.rules_file(bot) else {
            return div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Loading…")
                .into_any_element();
        };
        // The switch hugs its two labels at the leading edge, above
        // everything it governs.
        let tabs = segment_track(cx).children(
            [(RulesTab::Rules, "Rules"), (RulesTab::History, "History")]
                .into_iter()
                .map(|(value, label)| {
                    crate::panel_kit::hugging(segment(
                        SharedString::from(format!("rules-tab-{label}")),
                        label,
                        value == tab,
                        false,
                        cx,
                    ))
                    .on_click(on(owner, move |app, _, cx| app.set_rules_tab(value, cx)))
                }),
        );
        let body = match tab {
            RulesTab::Rules => self.render_rules_tab(bot, file, cx),
            RulesTab::History => self.render_history_tab(bot, file, owner, cx),
        };
        let problem = self.rules.get(&bot).and_then(|cache| cache.problem.clone());
        let markdown = file.markdown.clone();
        let focus = dialog.focus.clone();
        let action = (tab == RulesTab::History)
            .then(|| self.history_action(bot, file, owner, cx))
            .flatten();
        let footer = h_flex()
            .w_full()
            .items_center()
            .gap_2()
            .pt_3()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(if problem.is_some() {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .map(|hint| match (problem, action) {
                        (Some(problem), action) => hint
                            .child(problem)
                            .children(action.map(|action| h_flex().child(action))),
                        (None, Some(action)) => hint.child(h_flex().child(action)),
                        (None, None) => {
                            hint.child(format!("To change a rule, tell {name} in chat."))
                        }
                    }),
            )
            .child(
                Button::new("rules-copy")
                    .control(Tier::Secondary, Scale::Standard, false, cx)
                    .icon(IconName::Copy)
                    .label("Copy markdown")
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(markdown.clone()))
                    }),
            )
            .child(
                Button::new("rules-ask")
                    .control(Tier::Primary, Scale::Standard, false, cx)
                    .label("Ask to edit")
                    .accessibility_label(format!("Ask {name} to edit a rule"))
                    .on_click(on(owner, move |app, window, cx| {
                        app.ask_to_edit(focus.clone(), window, cx)
                    })),
            );
        v_flex()
            .w_full()
            .gap_4()
            .child(h_flex().child(tabs))
            .child(div().w_full().child(body))
            .child(footer)
            .into_any_element()
    }

    fn render_rules_tab(&self, bot: Uuid, file: &RulesFile, cx: &App) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(dialog) = &self.rules_dialog else {
            return div().into_any_element();
        };
        if file.sections.is_empty() {
            return div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No rules right now. Earlier versions are in History.")
                .into_any_element();
        }
        let seen = self.preferences.rules_seen.get(&bot).copied().unwrap_or(0);
        let latest = file.history.first().map_or(0, |v| v.version);
        let changed: std::collections::HashSet<String> = file
            .history
            .iter()
            .filter(|v| v.version == latest || v.version > seen)
            .map(|v| v.section.to_lowercase())
            .collect();
        let focus = dialog.focus.as_ref().map(|f| f.to_lowercase());
        let sections = file.sections.iter().map(|section| {
            let key = section.title.to_lowercase();
            let focused = focus.as_deref() == Some(key.as_str());
            v_flex()
                .w_full()
                .gap_1()
                .px_3()
                .py_2p5()
                .rounded(theme.radius)
                .bg(theme.secondary.opacity(0.5))
                .when(focused, |block| {
                    block
                        .bg(theme.primary.opacity(0.08))
                        .border_1()
                        .border_color(theme.primary.opacity(0.25))
                })
                .child(
                    h_flex()
                        .gap_1p5()
                        .items_center()
                        .when(changed.contains(&key), |row| row.child(dot(cx)))
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(section.title.clone()),
                        ),
                )
                .children(
                    section
                        .body
                        .lines()
                        .map(|line| div().w_full().text_sm().child(rule_line(line, cx))),
                )
        });
        v_flex()
            .id("rules-sections")
            .w_full()
            .min_h(px(BODY_MIN))
            .max_h(px(BODY_MAX))
            .overflow_y_scroll()
            .track_scroll(&dialog.scroll)
            .gap_2()
            .children(sections)
            .into_any_element()
    }

    /// History's one action: clear or step back from the current version,
    /// or restore the older version being viewed.
    fn history_action(
        &self,
        bot: Uuid,
        file: &RulesFile,
        owner: &WeakEntity<ElseApp>,
        cx: &App,
    ) -> Option<AnyElement> {
        let dialog = self.rules_dialog.as_ref()?;
        let picked = dialog
            .version
            .and_then(|v| file.history.iter().find(|h| h.version == v))
            .or_else(|| file.history.first())?;
        let restoring = self.rules.get(&bot).is_some_and(|cache| cache.restoring)
            || self.rules_version_of(bot, cx) != Some(file.version);
        let expected = file.version;
        let current = picked.version == file.version;
        let target = if current {
            picked.version - 1
        } else {
            picked.version
        };
        if target < 0 {
            return None;
        }
        // Quiet secondary text; clearing turns red only on hover.
        let clear = target == 0;
        let danger = cx.theme().danger;
        let (label, icon, color) = if clear {
            (
                "Clear rules".to_owned(),
                IconName::Trash,
                cx.theme().muted_foreground,
            )
        } else {
            (
                format!("Restore v{target}"),
                IconName::RotateCw,
                cx.theme().secondary_foreground,
            )
        };
        // The Button owns its hover style, so the red comes from a group
        // hover on the label it wraps.
        let group = "rules-history-action";
        let content = h_flex()
            .gap_1p5()
            .items_center()
            .text_color(color)
            .when(clear, |content| {
                content.group_hover(group, move |style| style.text_color(danger))
            })
            .child(Icon::new(icon).size_3p5())
            .child(label.clone());
        Some(
            div()
                .group(group)
                .child(
                    Button::new(if current {
                        "rules-restore"
                    } else {
                        "rules-restore-this"
                    })
                    .control(Tier::Ghost, Scale::Compact, restoring, cx)
                    .flush_start(Scale::Compact)
                    .accessibility_label(label)
                    .loading(restoring)
                    .child(content)
                    .on_click(on(owner, move |app, window, cx| {
                        app.restore_rules(bot, target, expected, window, cx)
                    })),
                )
                .into_any_element(),
        )
    }

    fn render_history_tab(
        &self,
        bot: Uuid,
        file: &RulesFile,
        owner: &WeakEntity<ElseApp>,
        cx: &App,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(dialog) = &self.rules_dialog else {
            return div().into_any_element();
        };
        let picked = dialog
            .version
            .and_then(|v| file.history.iter().find(|h| h.version == v))
            .or_else(|| file.history.first());
        let Some(picked) = picked else {
            return div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No versions yet.")
                .into_any_element();
        };
        let bot_name = self
            .bots
            .iter()
            .find(|b| b.id == bot)
            .map_or_else(|| "Bot".to_owned(), |b| b.name.clone());
        let describe = |v: &RulesVersion| -> (String, String, String) {
            let title = v.title();
            let by = if v.author == RuleAuthor::Owner {
                "You"
            } else {
                bot_name.as_str()
            };
            (title, by.to_owned(), when(v.created_at))
        };
        let list = v_flex()
            // A narrow column that fits a version's title and author; the
            // diff takes the rest.
            .id("rules-versions")
            .w(px(220.))
            .flex_none()
            .max_h(px(BODY_MAX))
            .overflow_y_scroll()
            .gap_1()
            .pr_3()
            .border_r_1()
            .border_color(theme.border)
            .children(file.history.iter().map(|v| {
                let (title, author, date) = describe(v);
                let undone = v.undone_at.is_some();
                let selected = v.version == picked.version;
                let version = v.version;
                Button::new(("rules-version", version as usize))
                    .ghost()
                    .focus_ring(false)
                    .cursor_pointer()
                    .with_size(Size::Size(px(16.)))
                    .w_full()
                    .px_2p5()
                    .py_2()
                    .selected(selected)
                    .accessibility_label(format!("Show version {version}"))
                    .on_click(on(owner, move |app, _, cx| {
                        app.pick_rules_version(version, cx)
                    }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .items_start()
                            .text_left()
                            .whitespace_normal()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_1p5()
                                    .text_size(px(13.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .when(undone, |title| title.line_through())
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(format!("v{version}")),
                                    ),
                            )
                            // Author, then date: the date never wraps; a long
                            // author name truncates first.
                            .child(
                                h_flex()
                                    .w_full()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(div().min_w_0().truncate().child(author))
                                    .child(div().flex_none().child(format!(" · {date}"))),
                            ),
                    )
            }));
        let detail = v_flex()
            .id("rules-version-detail")
            .flex_1()
            .min_w_0()
            .max_h(px(BODY_MAX))
            .overflow_y_scroll()
            .gap_4()
            .when_some(picked.undone_at, |col, at| {
                col.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("Undone by you · {}", when(at))),
                )
            })
            .when(!picked.owner_words.is_empty(), |col| {
                col.child(quote(&picked.owner_words, cx))
            })
            .child(diff_block(&picked.diff, cx));
        // The divider runs the body's full height, however short the diff.
        h_flex()
            .w_full()
            .min_h(px(BODY_MIN))
            .max_h(px(BODY_MAX))
            .items_stretch()
            .gap_6()
            .child(list)
            .child(detail)
            .into_any_element()
    }
}
