//! A bot's rules file: the one-line note shown when the bot edits a rule,
//! the Rules section in Bot details, and the dialog with the file and its
//! history.

use crate::controls::{Control as _, Scale, Tier, segment, segment_track};
use std::collections::HashMap;

use super::ElseApp;
use super::settings::{BotPanel, PanelEvent};
use else_wire::rules::{
    DiffLine, DiffOp, RuleAuthor, RuleChange, RulesFile, RulesUpdate, RulesVersion,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, Size, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct RulesCache {
    file: Option<RulesFile>,
    loading: Option<i64>,
    restoring: bool,
    problem: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum RulesTab {
    #[default]
    Rules,
    History,
}

pub(super) struct RulesDialog {
    bot: Uuid,
    tab: RulesTab,
    focus: Option<String>,
    version: Option<i64>,
    scroll: ScrollHandle,
}

pub(super) type RulesState = HashMap<Uuid, RulesCache>;

/// What the Rules section of Bot details shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) enum RulesSummary {
    /// Not loaded yet, with why the last load failed.
    #[default]
    Loading,
    Failed(String),
    /// Every rule was removed; the history stays.
    OnlyHistory,
    Empty,
    Rules {
        count: usize,
        /// Rules changed since the owner last looked.
        fresh: usize,
        updated: Option<chrono::DateTime<chrono::Utc>>,
    },
}

/// The Rules section's summary of `cache`, counting the rules changed
/// after version `seen`; a bot never looked at has none.
fn summarize(cache: Option<&RulesCache>, seen: Option<i64>) -> RulesSummary {
    let Some(file) = cache.and_then(|cache| cache.file.as_ref()) else {
        return match cache.and_then(|cache| cache.problem.clone()) {
            Some(problem) => RulesSummary::Failed(problem),
            None => RulesSummary::Loading,
        };
    };
    if file.sections.is_empty() {
        return if file.history.is_empty() {
            RulesSummary::Empty
        } else {
            RulesSummary::OnlyHistory
        };
    }
    let changed = file.changed_since(seen.unwrap_or(file.version));
    RulesSummary::Rules {
        count: file.sections.len(),
        fresh: file
            .sections
            .iter()
            .filter(|section| changed.contains(&section.title.to_lowercase()))
            .count(),
        updated: file.history.first().map(|latest| latest.created_at),
    }
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

fn change_label(change: RuleChange) -> &'static str {
    match change {
        RuleChange::Added => "Added rule",
        RuleChange::Changed => "Changed rule",
        RuleChange::Removed => "Removed rule",
        RuleChange::Restored => "Restored rule",
        RuleChange::Unknown => "Updated rule",
    }
}

fn when(at: chrono::DateTime<chrono::Utc>) -> String {
    let local = at.with_timezone(&chrono::Local);
    if local.date_naive() == chrono::Local::now().date_naive() {
        local.format("today %H:%M").to_string()
    } else {
        local.format("%b %-d").to_string()
    }
}

fn dot(cx: &App) -> Div {
    div()
        .flex_none()
        .size(px(6.))
        .rounded_full()
        .bg(cx.theme().primary)
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
    fn rules_version_of(&self, bot: Uuid, cx: &App) -> Option<i64> {
        self.shown(cx)
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())
            .map(|conversation| conversation.rules_version())
    }

    pub(super) fn restoring_rules(&self, bot: Uuid) -> bool {
        self.rules.get(&bot).is_some_and(|cache| cache.restoring)
    }

    fn rules_file(&self, bot: Uuid) -> Option<&RulesFile> {
        self.rules.get(&bot).and_then(|cache| cache.file.as_ref())
    }

    /// Load the open bot's rules when the stream reports a version the cache
    /// does not have yet.
    pub(super) fn ensure_rules(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(version) = self.rules_version_of(bot, cx) else {
            return;
        };
        let cache = self.rules.entry(bot).or_default();
        let have = cache.file.as_ref().map(|file| file.version);
        if have == Some(version) || cache.loading == Some(version) {
            return;
        }
        cache.loading = Some(version);
        let request = self.client.clone().rules(bot);
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |app, cx| {
                let cache = app.rules.entry(bot).or_default();
                if cache.loading != Some(version) {
                    return;
                }
                cache.loading = None;
                match result {
                    Ok(file) => {
                        cache.problem = None;
                        let current = file.version;
                        cache.file = Some(file);
                        if let std::collections::hash_map::Entry::Vacant(seen) =
                            app.preferences.rules_seen.entry(bot)
                        {
                            seen.insert(current);
                            app.save_preferences(cx);
                        }
                    }
                    Err(err) => cache.problem = Some(format!("Could not load rules: {err}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn open_rules(
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

    /// Only the Rules tab shows the current file; viewing an older change in
    /// History leaves newer ones unread.
    fn mark_rules_seen(&mut self, bot: Uuid, cx: &mut Context<Self>) {
        if let Some(version) = self.rules_file(bot).map(|file| file.version) {
            self.preferences.rules_seen.insert(bot, version);
            self.save_preferences(cx);
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

    fn restore_rules(
        &mut self,
        bot: Uuid,
        version: i64,
        expected: i64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cache = self.rules.entry(bot).or_default();
        if cache.restoring {
            return;
        }
        cache.restoring = true;
        cache.problem = None;
        let request = self.client.clone().restore_rules(bot, version, expected);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |app, cx| {
                let cache = app.rules.entry(bot).or_default();
                cache.restoring = false;
                if let Err(err) = result {
                    cache.problem = Some(format!("Could not restore: {err}"));
                }
                cx.notify();
            });
        })
        .detach();
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

    pub(super) fn rules_summary(&self, bot: Uuid) -> RulesSummary {
        summarize(
            self.rules.get(&bot),
            self.preferences.rules_seen.get(&bot).copied(),
        )
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
            let title = match (v.change, v.restored_from) {
                (RuleChange::Restored, Some(0)) => "Cleared all rules".to_owned(),
                (RuleChange::Restored, Some(from)) => format!("Restored v{from}"),
                (change, _) => format!(
                    "{} {}",
                    match change {
                        RuleChange::Added => "Added",
                        RuleChange::Removed => "Removed",
                        _ => "Changed",
                    },
                    v.section
                ),
            };
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

impl BotPanel {
    pub(super) fn render_rules_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let summary = self.rules();
        let open = Button::new("rules-open")
            .control(Tier::Secondary, Scale::Standard, false, cx)
            .label("Open")
            .accessibility_label("Open rules")
            .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::OpenRules)));
        let heading = h_flex()
            .w_full()
            .pb(super::panel::ROW_GAP)
            .items_center()
            .child(crate::appearance::section_label("Rules", cx).flex_1())
            .when(*summary == RulesSummary::OnlyHistory, |row| row.child(open));
        let body = super::panel::section_frame(cx).child(heading);
        let muted = |text: String| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let (count, fresh, updated) = match summary {
            RulesSummary::Loading => {
                return body.child(muted("Loading…".to_owned())).into_any_element();
            }
            RulesSummary::Failed(problem) => {
                return body.child(muted(problem.clone())).into_any_element();
            }
            RulesSummary::OnlyHistory => {
                return body
                    .child(muted(
                        "No rules right now. Open shows the history of earlier rules, and you \
                         can restore any version from there."
                            .to_owned(),
                    ))
                    .into_any_element();
            }
            RulesSummary::Empty => {
                return body
                    .child(muted(
                        "No rules yet. Tell the bot what to do whenever something happens, for \
                         example \"when a new issue arrives, start a T3 Code thread with `fix \
                         issue ABC-1`\". It saves the rule here and follows it word for word."
                            .to_owned(),
                    ))
                    .into_any_element();
            }
            RulesSummary::Rules {
                count,
                fresh,
                updated,
            } => (*count, *fresh, *updated),
        };
        let title = if count == 1 {
            "1 rule".to_owned()
        } else {
            format!("{count} rules")
        };
        let detail = match (fresh, updated) {
            (0, Some(at)) => format!("Updated {}", when(at)),
            (0, None) => "Followed word for word".to_owned(),
            (1, _) => "1 changed since you looked".to_owned(),
            (n, _) => format!("{n} changed since you looked"),
        };
        let card = Button::new("rules-summary")
            .ghost()
            .focus_ring(false)
            .cursor_pointer()
            .with_size(Size::Size(px(16.)))
            .mx(px(-8.))
            .my(px(-8.))
            .px_2()
            .py_2()
            .accessibility_label("Open rules")
            .on_click(cx.listener(|_, _, _, cx| cx.emit(PanelEvent::OpenRules)))
            .child(
                // The same anatomy as the Model row above it: a name over
                // one meta line, and a chevron.
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .items_center()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .items_start()
                            .text_left()
                            .child(super::panel::item_name(title))
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .text_size(px(12.5))
                                    .line_height(px(18.))
                                    .text_color(theme.muted_foreground)
                                    .when(fresh > 0, |row| row.child(dot(cx)))
                                    .child(div().truncate().child(detail)),
                            ),
                    )
                    .child(
                        Icon::new(IconName::ChevronRight)
                            .size_4()
                            .text_color(theme.muted_foreground),
                    ),
            );
        body.child(card).into_any_element()
    }
}

impl super::conversation::ConversationView {
    pub(super) fn render_rules_update(
        &self,
        item: Uuid,
        bot: Uuid,
        update: &RulesUpdate,
        stamp: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let author = self.inputs.bots.iter().find(|b| b.id == bot);
        let current = self.conversation.as_ref().map(|c| c.rules_version()) == Some(update.version)
            && update.undone_at.is_none();
        let restoring = self.inputs.restoring_rules;
        let section = update.section.clone();
        let shown = update.version;
        let view = super::action_card::event_action(
            SharedString::from(format!("rules-view-{item}")),
            "View",
            false,
            cx,
        )
        .accessibility_label(format!("View the change to {}", update.section))
        .on_click(self.on_app(move |app, _, window, cx| {
            app.open_rules(
                bot,
                Some(section.clone()),
                RulesTab::History,
                Some(shown),
                window,
                cx,
            )
        }));
        let previous = update.version - 1;
        let undo = current.then(|| {
            super::action_card::event_action(
                SharedString::from(format!("rules-undo-{item}")),
                "Undo",
                restoring,
                cx,
            )
            .loading(restoring)
            .accessibility_label(format!("Undo the change to {}", update.section))
            .on_click(self.on_app(move |app, _, window, cx| {
                app.restore_rules(bot, previous, shown, window, cx)
            }))
        });
        let undone = update.undone_at;
        let line = super::action_card::event_row(&theme)
            .id(SharedString::from(format!("message-{item}")))
            .child(self.event_faces(&format!("rules-{item}"), author.map(|b| b.id)))
            .child(
                super::action_card::event_verb(&theme, change_label(update.change))
                    .when(undone.is_some(), |verb| verb.line_through()),
            )
            .child(
                super::action_card::event_object(&theme, update.section.clone())
                    .when(undone.is_some(), |object| {
                        object.line_through().text_color(theme.muted_foreground)
                    }),
            )
            .when_some(undone, |row, at| {
                row.child(
                    div()
                        .flex_none()
                        .text_size(px(12.5))
                        .text_color(super::message_row::meta_color(&theme))
                        .child(format!("Undone · {}", when(at))),
                )
            })
            .child(super::action_card::event_end(
                &theme,
                super::action_card::EventEnd::Actions(std::iter::once(view).chain(undo).collect()),
                super::action_card::Disclosure::None,
            ));
        let _ = stamp;
        line.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{RulesCache, RulesSummary, summarize};
    use else_wire::rules::RulesFile;
    use serde_json::json;

    fn file(sections: &[&str], history: &[(i64, &str, &str)]) -> RulesFile {
        serde_json::from_value(json!({
            "version": history.first().map_or(1, |(version, _, _)| *version),
            "markdown": "",
            "sections": sections.iter().map(|title| json!({ "title": title, "body": "b" })).collect::<Vec<_>>(),
            "history": history.iter().map(|(version, section, change)| json!({
                "version": version, "section": section, "change": change,
                "owner_words": "", "author": "bot", "restored_from": null, "diff": [],
                "created_at": format!("2026-11-{}T09:00:00Z", 20 + version), "undone_at": null,
            })).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    fn cache(file: Option<RulesFile>, problem: Option<&str>) -> RulesCache {
        RulesCache {
            file,
            problem: problem.map(str::to_owned),
            ..RulesCache::default()
        }
    }

    #[test]
    fn the_rules_section_counts_rules_changed_since_the_owner_looked() {
        assert_eq!(summarize(None, None), RulesSummary::Loading);
        assert_eq!(
            summarize(
                Some(&cache(None, Some("Could not load rules: offline"))),
                None
            ),
            RulesSummary::Failed("Could not load rules: offline".into())
        );
        assert_eq!(
            summarize(Some(&cache(Some(file(&[], &[])), None)), None),
            RulesSummary::Empty
        );
        assert_eq!(
            summarize(
                Some(&cache(Some(file(&[], &[(2, "Mail", "removed")])), None)),
                None
            ),
            RulesSummary::OnlyHistory
        );
        let rules = file(
            &["Mail", "Deploys"],
            &[
                (3, "Deploys", "changed"),
                (2, "Mail", "added"),
                (1, "Deploys", "added"),
            ],
        );
        let updated = Some("2026-11-23T09:00:00Z".parse().unwrap());
        assert_eq!(
            summarize(Some(&cache(Some(rules.clone()), None)), Some(1)),
            RulesSummary::Rules {
                count: 2,
                fresh: 2,
                updated
            }
        );
        assert_eq!(
            summarize(Some(&cache(Some(rules.clone()), None)), Some(2)),
            RulesSummary::Rules {
                count: 2,
                fresh: 1,
                updated
            }
        );
        assert_eq!(
            summarize(Some(&cache(Some(rules), None)), None),
            RulesSummary::Rules {
                count: 2,
                fresh: 0,
                updated
            },
            "a bot never looked at has nothing new"
        );
    }
}
