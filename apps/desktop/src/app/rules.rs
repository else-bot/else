//! A bot's rules file: the one-line note shown when the bot edits a rule,
//! the Rules section in Bot details, and the dialog with the file and its
//! history.

use std::collections::HashMap;

use super::ElseApp;
use crate::avatar::bot_avatar;
use crate::model::{DiffLine, DiffOp, RuleChange, RulesFile, RulesUpdate, RulesVersion};
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, Size, WindowExt as _,
    h_flex, v_flex,
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
        RuleChange::Added => "added the rule",
        RuleChange::Changed => "changed the rule",
        RuleChange::Removed => "removed the rule",
        RuleChange::Restored => "restored the rule",
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
    v_flex()
        .w_full()
        .flex_none()
        .py_1()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border.opacity(0.6))
        .bg(theme.background)
        .overflow_hidden()
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .children(diff.iter().map(|line| {
            let (sign, color, fill) = match line.op {
                DiffOp::Same => (" ", theme.muted_foreground, None),
                DiffOp::Add => ("+", theme.success, Some(theme.success.opacity(0.12))),
                DiffOp::Remove => ("-", theme.danger, Some(theme.danger.opacity(0.10))),
            };
            h_flex()
                .w_full()
                .px_2p5()
                .gap_2()
                .items_start()
                .when_some(fill, |row, fill| row.bg(fill))
                .text_color(color)
                .child(div().flex_none().child(sign))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_normal()
                        .child(line.text.clone()),
                )
        }))
}

fn quote(words: &str, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .w_full()
        .pl_2p5()
        .border_l_2()
        .border_color(theme.border)
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(format!("From your message: \u{201c}{words}\u{201d}"))
}

impl ElseApp {
    fn rules_version_of(&self, bot: Uuid) -> Option<i64> {
        self.view
            .as_ref()
            .filter(|view| view.bot_id == bot)
            .and_then(|view| view.conversation.as_ref())
            .map(|conversation| conversation.rules_version())
    }

    fn rules_file(&self, bot: Uuid) -> Option<&RulesFile> {
        self.rules.get(&bot).and_then(|cache| cache.file.as_ref())
    }

    /// Load the open bot's rules when the stream reports a version the cache
    /// does not have yet.
    pub(super) fn ensure_rules(&mut self, bot: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(version) = self.rules_version_of(bot) else {
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
            let dialog = dialog.w(px(680.)).centered().bg(cx.theme().popover);
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
            dialog
                .title(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!("{name} rules")),
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

    pub(super) fn render_rules_update(
        &self,
        item: Uuid,
        bot: Uuid,
        update: &RulesUpdate,
        stamp: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let author = self.bots.iter().find(|b| b.id == bot);
        let name = author.map_or_else(|| "Bot".to_owned(), |b| b.name.clone());
        let current = self.rules_version_of(bot) == Some(update.version);
        let restoring = self.rules.get(&bot).is_some_and(|cache| cache.restoring);
        let section = update.section.clone();
        let shown = update.version;
        let view = Button::new(SharedString::from(format!("rules-view-{item}")))
            .focus_ring(false)
            .cursor_pointer()
            .label("View")
            .accessibility_label(format!("View the change to {}", update.section))
            .on_click(cx.listener(move |app, _, window, cx| {
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
            Button::new(SharedString::from(format!("rules-undo-{item}")))
                .focus_ring(false)
                .cursor_pointer()
                .label("Undo")
                .loading(restoring)
                .disabled(restoring)
                .accessibility_label(format!("Undo the change to {}", update.section))
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.restore_rules(bot, previous, shown, window, cx)
                }))
        });
        let line = h_flex()
            .id(SharedString::from(format!("message-{item}")))
            .w_full()
            .min_w_0()
            .items_center()
            .gap_1p5()
            .pl_2p5()
            .pr_1()
            .py_1()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border.opacity(0.6))
            .bg(theme.sidebar.opacity(0.6))
            .text_sm()
            .children(author.map(|b| {
                bot_avatar(
                    SharedString::from(format!("rules-{item}")),
                    b.avatar,
                    px(16.),
                )
                .still()
            }))
            .child(
                div()
                    .min_w_0()
                    .max_w(px(160.))
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .child(name),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(theme.muted_foreground)
                    .child(change_label(update.change)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .child(update.section.clone()),
            )
            .child(view)
            .children(undo);
        v_flex()
            .w_full()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .pr(super::message_row::BOT_GUTTER)
                    .child(line),
            )
            .children(stamp.map(|at| div().text_xs().text_color(theme.muted_foreground).child(at)))
            .into_any_element()
    }

    pub(super) fn render_rules_section(&self, bot: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let cache = self.rules.get(&bot);
        let file = cache.and_then(|cache| cache.file.as_ref());
        let only_history =
            file.is_some_and(|file| file.sections.is_empty() && !file.history.is_empty());
        let open = Button::new("rules-open")
            .focus_ring(false)
            .cursor_pointer()
            .label("Open")
            .accessibility_label("Open rules")
            .disabled(file.is_none())
            .on_click(cx.listener(move |app, _, window, cx| {
                app.open_rules(bot, None, RulesTab::Rules, None, window, cx)
            }));
        let heading = h_flex()
            .w_full()
            .items_center()
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("RULES"),
            )
            .when(only_history, |row| row.child(open));
        let body = v_flex().gap_3().child(heading);
        let muted = |text: String| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let Some(file) = file else {
            let text = cache
                .and_then(|cache| cache.problem.clone())
                .unwrap_or_else(|| "Loading…".to_owned());
            return body.child(muted(text)).into_any_element();
        };
        if file.sections.is_empty() && !file.history.is_empty() {
            return body
                .child(muted(
                    "No rules right now. Open shows the history of earlier rules, and you can \
                     restore any version from there."
                        .to_owned(),
                ))
                .into_any_element();
        }
        if file.sections.is_empty() {
            return body
                .child(muted(
                    "No rules yet. Tell the bot what to do whenever something happens, for \
                     example \"when a new issue arrives, start a T3 Code thread with `fix issue \
                     ABC-1`\". It saves the rule here and follows it word for word."
                        .to_owned(),
                ))
                .into_any_element();
        }
        let seen = self
            .preferences
            .rules_seen
            .get(&bot)
            .copied()
            .unwrap_or(file.version);
        let changed = file.changed_since(seen);
        let fresh = file
            .sections
            .iter()
            .filter(|section| changed.contains(&section.title.to_lowercase()))
            .count();
        let count = file.sections.len();
        let title = if count == 1 {
            "1 rule".to_owned()
        } else {
            format!("{count} rules")
        };
        let detail = match (fresh, file.history.first()) {
            (0, Some(latest)) => format!("Updated {}", when(latest.created_at)),
            (0, None) => "Followed word for word".to_owned(),
            (1, _) => "1 changed since you looked".to_owned(),
            (n, _) => format!("{n} changed since you looked"),
        };
        let card = Button::new("rules-summary")
            .ghost()
            .focus_ring(false)
            .cursor_pointer()
            .with_size(Size::Size(px(16.)))
            .w_full()
            .px_2p5()
            .py_2()
            .accessibility_label("Open rules")
            .on_click(cx.listener(move |app, _, window, cx| {
                app.open_rules(bot, None, RulesTab::Rules, None, window, cx)
            }))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2p5()
                    .items_center()
                    .child(
                        Icon::new(IconName::FileText)
                            .size_4()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .items_start()
                            .text_left()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(title),
                            )
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .items_center()
                                    .text_xs()
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
        body.child(
            div()
                .w_full()
                .rounded(theme.radius)
                .bg(theme.background)
                .border_1()
                .border_color(theme.border)
                .child(card),
        )
        .into_any_element()
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
        let tabs = h_flex().gap_1().children(
            [(RulesTab::Rules, "Rules"), (RulesTab::History, "History")]
                .into_iter()
                .map(|(value, label)| {
                    Button::new(SharedString::from(format!("rules-tab-{label}")))
                        .ghost()
                        .focus_ring(false)
                        .cursor_pointer()
                        .label(label)
                        .selected(value == tab)
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
                    .text_xs()
                    .text_color(if problem.is_some() {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(
                        problem
                            .unwrap_or_else(|| format!("To change a rule, tell {name} in chat.")),
                    ),
            )
            .child(
                Button::new("rules-copy")
                    .focus_ring(false)
                    .cursor_pointer()
                    .icon(IconName::Copy)
                    .label("Copy markdown")
                    .on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(markdown.clone()))
                    }),
            )
            .child(
                Button::new("rules-ask")
                    .primary()
                    .focus_ring(false)
                    .cursor_pointer()
                    .label("Ask to edit")
                    .accessibility_label(format!("Ask {name} to edit a rule"))
                    .on_click(on(owner, move |app, window, cx| {
                        app.ask_to_edit(focus.clone(), window, cx)
                    })),
            );
        v_flex()
            .w_full()
            .gap_3()
            .child(tabs)
            .child(div().h(px(300.)).w_full().child(body))
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
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&dialog.scroll)
            .gap_2()
            .children(sections)
            .into_any_element()
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
        let describe = |v: &RulesVersion| -> (String, String) {
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
            let by = if v.author == "owner" {
                "You"
            } else {
                bot_name.as_str()
            };
            (title, format!("{by} · {}", when(v.created_at)))
        };
        let list = v_flex()
            .id("rules-versions")
            .w(px(200.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .gap_0p5()
            .pr_2()
            .border_r_1()
            .border_color(theme.border)
            .children(file.history.iter().map(|v| {
                let (title, meta) = describe(v);
                let selected = v.version == picked.version;
                let version = v.version;
                Button::new(("rules-version", version as usize))
                    .ghost()
                    .focus_ring(false)
                    .cursor_pointer()
                    .with_size(Size::Size(px(16.)))
                    .w_full()
                    .px_2()
                    .py_1p5()
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
                            .child(
                                h_flex()
                                    .gap_1p5()
                                    .text_xs()
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(format!("v{version}")),
                                    )
                                    .child(div().min_w_0().truncate().child(title)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(meta),
                            ),
                    )
            }));
        let restoring = self.rules.get(&bot).is_some_and(|cache| cache.restoring)
            || self.rules_version_of(bot) != Some(file.version);
        let target = picked.version - 1;
        let expected = file.version;
        let restore = (picked.version == file.version && target >= 0).then(|| {
            Button::new("rules-restore")
                .focus_ring(false)
                .cursor_pointer()
                .icon(IconName::RotateCw)
                .label(if target == 0 {
                    "Clear rules".to_owned()
                } else {
                    format!("Restore v{target}")
                })
                .loading(restoring)
                .disabled(restoring)
                .on_click(on(owner, move |app, window, cx| {
                    app.restore_rules(bot, target, expected, window, cx)
                }))
        });
        let restore_this = (picked.version != file.version).then(|| {
            let version = picked.version;
            Button::new("rules-restore-this")
                .focus_ring(false)
                .cursor_pointer()
                .icon(IconName::RotateCw)
                .label(format!("Restore v{version}"))
                .loading(restoring)
                .disabled(restoring)
                .on_click(on(owner, move |app, window, cx| {
                    app.restore_rules(bot, version, expected, window, cx)
                }))
        });
        let detail = v_flex()
            .id("rules-version-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .gap_2p5()
            .when(!picked.owner_words.is_empty(), |col| {
                col.child(quote(&picked.owner_words, cx))
            })
            .child(diff_block(&picked.diff, cx))
            .child(
                h_flex()
                    .w_full()
                    .child(div().flex_1())
                    .children(restore)
                    .children(restore_this),
            );
        h_flex()
            .size_full()
            .items_start()
            .gap_3()
            .child(list)
            .child(detail)
            .into_any_element()
    }
}
