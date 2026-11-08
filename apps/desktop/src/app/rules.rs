//! A bot's rules file: the card shown when the bot edits a rule, the Rules
//! section in Bot details, and the dialog with the file, its history, and
//! its markdown.

use std::collections::HashMap;

use super::ElseApp;
use crate::avatar::bot_avatar;
use crate::model::{
    DiffLine, DiffOp, RuleChange, RuleSection, RulesFile, RulesUpdate, RulesVersion,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, Size, WindowExt as _,
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
    Markdown,
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
        RuleChange::Added => "added a rule",
        RuleChange::Changed => "updated its rules",
        RuleChange::Removed => "removed a rule",
        RuleChange::Restored => "restored its rules",
    }
}

fn counts(diff: &[DiffLine]) -> String {
    let removed = diff.iter().filter(|l| l.op == DiffOp::Remove).count();
    let added = diff.iter().filter(|l| l.op == DiffOp::Add).count();
    match (removed, added) {
        (0, a) => format!("{a} added"),
        (r, 0) => format!("{r} removed"),
        (r, a) => format!("{r} removed · {a} added"),
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

fn summary(section: &RuleSection) -> String {
    section
        .body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

fn chip(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .flex_none()
        .px_1p5()
        .rounded_full()
        .bg(theme.secondary)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(text.into())
}

fn accent_chip(text: impl Into<SharedString>, cx: &App) -> Div {
    let theme = cx.theme();
    chip(text, cx)
        .bg(theme.primary.opacity(0.14))
        .text_color(theme.primary)
}

fn dot(cx: &App) -> Div {
    div()
        .flex_none()
        .size(px(6.))
        .rounded_full()
        .bg(cx.theme().primary)
}

/// One line of a rule: a leading `Label:` reads as a small caps label and
/// backtick spans as code.
fn rule_line(line: &str, cx: &App) -> Div {
    let theme = cx.theme();
    let mut row = h_flex().w_full().flex_wrap().items_baseline().gap_x_1();
    let mut rest = line;
    if let Some((label, after)) = line.split_once(": ")
        && !label.is_empty()
        && label.len() <= 16
        && label.chars().all(|c| c.is_alphabetic() || c == ' ')
    {
        row = row.child(
            div()
                .flex_none()
                .mr_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(label.to_uppercase()),
        );
        rest = after;
    }
    for (ix, span) in rest.split('`').enumerate() {
        if span.is_empty() {
            continue;
        }
        if ix % 2 == 1 {
            row = row.child(
                div()
                    .px_1()
                    .rounded(px(4.))
                    .bg(theme.secondary)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .child(span.to_owned()),
            );
        } else {
            row = row.child(div().child(span.to_owned()));
        }
    }
    row
}

fn diff_block(diff: &[DiffLine], cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .w_full()
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let scroll = ScrollHandle::new();
        self.rules_dialog = Some(RulesDialog {
            bot,
            tab,
            focus,
            version: None,
            scroll,
        });
        if let Some(version) = self.rules_file(bot).map(|file| file.version) {
            self.preferences.rules_seen.insert(bot, version);
            self.save_preferences(cx);
        }
        let owner = cx.entity().downgrade();
        window.open_dialog(cx, move |dialog, _, cx| {
            let dialog = dialog.w(px(680.));
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
            dialog.title(format!("{name} rules")).child(content)
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
        if let Some(dialog) = &mut self.rules_dialog {
            dialog.tab = tab;
        }
        cx.notify();
    }

    fn focus_rule(&mut self, title: String, cx: &mut Context<Self>) {
        if let Some(dialog) = &mut self.rules_dialog {
            dialog.focus = Some(title);
        }
        self.scroll_to_focus();
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
        self.composer.update(cx, |state, cx| {
            state.set_value(text, window, cx);
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
        let header = h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap_1p5()
            .text_xs()
            .text_color(theme.muted_foreground)
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
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(name),
            )
            .child(div().flex_none().child(change_label(update.change)))
            .child(div().flex_1())
            .child(chip(
                if update.version > 1 {
                    format!("v{} → v{}", update.version - 1, update.version)
                } else {
                    format!("v{}", update.version)
                },
                cx,
            ));
        let title = h_flex()
            .gap_2()
            .items_center()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(update.section.clone()),
            )
            .child(accent_chip(
                match update.change {
                    RuleChange::Added => "new",
                    RuleChange::Changed => "changed",
                    RuleChange::Removed => "removed",
                    RuleChange::Restored => "restored",
                },
                cx,
            ));
        let view = Button::new(SharedString::from(format!("rules-view-{item}")))
            .ghost()
            .small()
            .focus_ring(false)
            .cursor_pointer()
            .icon(IconName::FileText)
            .label("View rules")
            .on_click(cx.listener(move |app, _, window, cx| {
                app.open_rules(bot, Some(section.clone()), RulesTab::Rules, window, cx)
            }));
        let previous = update.version - 1;
        let shown = update.version;
        let undo = current.then(|| {
            Button::new(SharedString::from(format!("rules-undo-{item}")))
                .ghost()
                .small()
                .focus_ring(false)
                .cursor_pointer()
                .label("Undo change")
                .loading(restoring)
                .disabled(restoring)
                .accessibility_label(format!("Undo the change to {}", update.section))
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.restore_rules(bot, previous, shown, window, cx)
                }))
        });
        let card = v_flex()
            .id(SharedString::from(format!("message-{item}")))
            .w_full()
            .gap_2p5()
            .p_3p5()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border.opacity(0.6))
            .bg(theme.sidebar.opacity(0.6))
            .child(header)
            .child(title)
            .child(quote(&update.owner_words, cx))
            .child(diff_block(&update.diff, cx))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_1()
                    .child(view)
                    .children(undo)
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(counts(&update.diff)),
                    ),
            );
        v_flex()
            .w_full()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .pr(super::message_row::BOT_GUTTER)
                    .child(card),
            )
            .children(stamp.map(|at| div().text_xs().text_color(theme.muted_foreground).child(at)))
            .into_any_element()
    }

    pub(super) fn render_rules_section(&self, bot: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let cache = self.rules.get(&bot);
        let file = cache.and_then(|cache| cache.file.as_ref());
        let has_rules = file.is_some_and(|file| !file.history.is_empty());
        let open = Button::new("rules-open")
            .ghost()
            .xsmall()
            .focus_ring(false)
            .cursor_pointer()
            .label("Open")
            .accessibility_label("Open rules")
            .disabled(file.is_none())
            .on_click(cx.listener(move |app, _, window, cx| {
                app.open_rules(bot, None, RulesTab::Rules, window, cx)
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
            .when(has_rules, |row| row.child(open));
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
        let rows = file.sections.iter().enumerate().map(|(ix, section)| {
            let title = section.title.clone();
            let fresh = changed.contains(&section.title.to_lowercase());
            div()
                .w_full()
                .rounded(theme.radius)
                .bg(theme.secondary)
                .child(
                    Button::new(("rule", ix))
                        .ghost()
                        .focus_ring(false)
                        .cursor_pointer()
                        .with_size(Size::Size(px(16.)))
                        .w_full()
                        .px_2p5()
                        .py_2()
                        .accessibility_label(format!("Open rule {}", section.title))
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.open_rules(bot, Some(title.clone()), RulesTab::Rules, window, cx)
                        }))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .items_start()
                                .text_left()
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .items_center()
                                        .when(fresh, |row| row.child(dot(cx)))
                                        .child(
                                            div()
                                                .min_w_0()
                                                .truncate()
                                                .text_sm()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .child(section.title.clone()),
                                        ),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(summary(section)),
                                ),
                        ),
                )
        });
        let latest = file.history.first().map(|v| {
            let by = if v.author == "owner" {
                "restored by you".to_owned()
            } else {
                let name = self
                    .bots
                    .iter()
                    .find(|b| b.id == bot)
                    .map_or("the bot", |b| b.name.as_str());
                format!("edited by {name}")
            };
            format!("v{} · {by} {}", v.version, when(v.created_at))
        });
        body.child(v_flex().gap_1p5().children(rows))
            .children(latest.map(muted))
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
            [
                (RulesTab::Rules, "Rules"),
                (RulesTab::History, "History"),
                (RulesTab::Markdown, "Markdown"),
            ]
            .into_iter()
            .map(|(value, label)| {
                Button::new(SharedString::from(format!("rules-tab-{label}")))
                    .ghost()
                    .small()
                    .focus_ring(false)
                    .cursor_pointer()
                    .label(label)
                    .selected(value == tab)
                    .on_click(on(owner, move |app, _, cx| app.set_rules_tab(value, cx)))
            }),
        );
        let body = match tab {
            RulesTab::Rules => self.render_rules_tab(bot, file, owner, cx),
            RulesTab::History => self.render_history_tab(bot, file, owner, cx),
            RulesTab::Markdown => self.render_markdown_tab(file, cx),
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
                    .ghost()
                    .small()
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
                    .small()
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
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(chip(format!("v{}", file.version), cx))
                    .child(div().flex_1())
                    .child(tabs),
            )
            .child(div().h(px(380.)).w_full().child(body))
            .child(footer)
            .into_any_element()
    }

    fn render_rules_tab(
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
        let outline = v_flex()
            .w(px(160.))
            .flex_none()
            .h_full()
            .gap_0p5()
            .pr_2()
            .border_r_1()
            .border_color(theme.border)
            .children(file.sections.iter().enumerate().map(|(ix, section)| {
                let title = section.title.clone();
                let selected = focus.as_deref() == Some(section.title.to_lowercase().as_str());
                let fresh = changed.contains(&section.title.to_lowercase());
                Button::new(("rules-outline", ix))
                    .ghost()
                    .small()
                    .focus_ring(false)
                    .cursor_pointer()
                    .w_full()
                    .selected(selected)
                    .accessibility_label(format!("Show rule {}", section.title))
                    .on_click(on(owner, move |app, _, cx| {
                        app.focus_rule(title.clone(), cx)
                    }))
                    .child(
                        h_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1p5()
                            .items_center()
                            .text_xs()
                            .when(fresh, |row| row.child(dot(cx)))
                            .child(div().min_w_0().truncate().child(section.title.clone())),
                    )
            }));
        let sections = file.sections.iter().map(|section| {
            let focused = focus.as_deref() == Some(section.title.to_lowercase().as_str());
            let fresh = changed.contains(&section.title.to_lowercase());
            let version = file
                .history
                .iter()
                .find(|v| v.section.eq_ignore_ascii_case(&section.title))
                .map(|v| v.version);
            v_flex()
                .w_full()
                .gap_1()
                .px_2p5()
                .py_2()
                .rounded(theme.radius)
                .when(focused, |block| {
                    block
                        .bg(theme.primary.opacity(0.08))
                        .border_1()
                        .border_color(theme.primary.opacity(0.25))
                })
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(section.title.clone()),
                        )
                        .when_some(version.filter(|_| fresh), |row, version| {
                            row.child(accent_chip(format!("changed in v{version}"), cx))
                        }),
                )
                .children(
                    section
                        .body
                        .lines()
                        .map(|line| rule_line(line, cx).text_sm()),
                )
        });
        h_flex()
            .size_full()
            .items_start()
            .gap_3()
            .child(outline)
            .child(
                v_flex()
                    .id("rules-sections")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .track_scroll(&dialog.scroll)
                    .gap_2()
                    .children(sections),
            )
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
            .w(px(220.))
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
                .ghost()
                .small()
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
                .ghost()
                .small()
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

    fn render_markdown_tab(&self, file: &RulesFile, cx: &App) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .id("rules-markdown")
            .size_full()
            .overflow_y_scroll()
            .p_3()
            .rounded(theme.radius)
            .bg(theme.background)
            .border_1()
            .border_color(theme.border.opacity(0.6))
            .font_family(theme.mono_font_family.clone())
            .text_xs()
            .children(file.markdown.lines().map(|line| {
                let heading = line.starts_with("## ");
                div()
                    .min_h(px(16.))
                    .whitespace_normal()
                    .when(heading, |line| line.text_color(theme.primary))
                    .child(line.to_owned())
            }))
            .into_any_element()
    }
}
