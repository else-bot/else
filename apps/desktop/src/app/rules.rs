//! A bot's rules file: its cache and the Rules section in Bot details.
//! The one-line note shown when the bot edits a rule is in `row`, and the
//! dialog with the file and its history in `dialog`.

use crate::controls::{Control as _, Scale, Tier};
use std::collections::HashMap;

use super::ElseApp;
use super::settings::{BotPanel, PanelEvent};
use else_wire::rules::{RuleChange, RulesFile};
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

mod dialog;
mod row;

pub(super) use dialog::{RulesDialog, RulesTab};

#[derive(Default)]
pub(super) struct RulesCache {
    file: Option<RulesFile>,
    loading: Option<i64>,
    restoring: bool,
    problem: Option<String>,
    /// Kept in step with `file`, `problem` and the version last seen by
    /// [`ElseApp::summarize_rules`], so drawing does not count rules.
    summary: RulesSummary,
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
                app.summarize_rules(bot);
                cx.notify();
            });
        })
        .detach();
    }

    /// Only the Rules tab shows the current file; viewing an older change in
    /// History leaves newer ones unread.
    fn mark_rules_seen(&mut self, bot: Uuid, cx: &mut Context<Self>) {
        if let Some(version) = self.rules_file(bot).map(|file| file.version) {
            self.preferences.rules_seen.insert(bot, version);
            self.summarize_rules(bot);
            self.save_preferences(cx);
        }
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
        self.summarize_rules(bot);
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
                app.summarize_rules(bot);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn rules_summary(&self, bot: Uuid) -> RulesSummary {
        self.rules
            .get(&bot)
            .map(|cache| cache.summary.clone())
            .unwrap_or_default()
    }

    fn summarize_rules(&mut self, bot: Uuid) {
        let seen = self.preferences.rules_seen.get(&bot).copied();
        if let Some(cache) = self.rules.get_mut(&bot) {
            cache.summary = summarize(Some(cache), seen);
        }
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
