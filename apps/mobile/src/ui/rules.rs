//! A bot's rules and their history, as the desktop's rules dialog has
//! them: the rules now, each saved version with who changed what, and a
//! version's diff with Undo for the newest change or Restore for an older
//! one. Restoring saves a new version; the server refuses it when the rules
//! moved on meanwhile, so the owner restores what they looked at.

use chrono::{DateTime, Local, Utc};
use else_wire::rules::{DiffOp, RuleAuthor, RulesFile, RulesVersion};
use nsh_base::{alert_dialog, nav_page};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::list;
use super::parts::{self, colors};
use super::session;
use else_client::request::Request;

pub fn open(bot: Uuid, rules: Resource<RulesFile, String>) {
    super::push(format!("rules-{bot}"), move || screen(bot, rules));
}

fn when(at: DateTime<Utc>) -> String {
    let local = at.with_timezone(&Local);
    if local.date_naive() == Local::now().date_naive() {
        local.format("today %H:%M").to_string()
    } else {
        local.format("%b %-d").to_string()
    }
}

fn bot_name(bot: Uuid) -> String {
    session::bots().with_untracked(|b| {
        b.list
            .iter()
            .find(|it| it.id == bot)
            .map_or_else(|| "The bot".to_owned(), |it| it.name.clone())
    })
}

/// "v4 · You · today 09:12", and "Undone" when the owner undid it.
fn byline(bot: Uuid, version: &RulesVersion) -> String {
    let by = if version.author == RuleAuthor::Owner {
        "You".to_owned()
    } else {
        bot_name(bot)
    };
    let mut line = format!("v{} · {by} · {}", version.version, when(version.created_at));
    if let Some(at) = version.undone_at {
        line.push_str(&format!(" · Undone {}", when(at)));
    }
    line
}

fn screen(bot: Uuid, rules: Resource<RulesFile, String>) -> Screen {
    // Shown from the Details screen's read, so a rule the bot changes
    // updates here too.
    let body = dynamic(move || {
        let file = rules.get()?;
        let sections: Vec<AnyElement> = file
            .sections
            .iter()
            .map(|section| {
                view()
                    .px_4()
                    .py(px(11.0))
                    .gap(px(4.0))
                    .child(
                        text(section.title.clone())
                            .text_size(px(17.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors::label()),
                    )
                    .child(
                        text(section.body.clone())
                            .selectable(true)
                            .text_size(px(15.0))
                            .line_height(px(20.0))
                            .text_color(colors::label()),
                    )
                    .into_element()
            })
            .collect();
        let history: Vec<AnyElement> = file
            .history
            .iter()
            .map(|version| {
                let number = version.version;
                list::link_row(
                    version.title(),
                    byline(bot, version),
                    Some(move || open_version(bot, number, rules)),
                )
            })
            .collect();
        Some(
            view()
                .gap_6()
                .child(if sections.is_empty() {
                    parts::footnote("No rules right now.").into_element()
                } else {
                    list::section(
                        Some("Rules"),
                        sections,
                        Some("Ask the bot in the chat to add, change or remove a rule.".to_owned()),
                    )
                    .into_element()
                })
                .child((!history.is_empty()).then(|| {
                    list::section(
                        Some("History"),
                        history,
                        Some(
                            "Open a version to see what changed, and to undo or restore it."
                                .to_owned(),
                        ),
                    )
                })),
        )
    });
    nav_page()
        .key(format!("rules-{bot}"))
        .title("Rules")
        .header_large_title(false)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .child(view().py_6().child(body)),
        )
}

fn open_version(bot: Uuid, version: i64, rules: Resource<RulesFile, String>) {
    super::push(format!("rules-{bot}-v{version}"), move || {
        version_screen(bot, version, rules)
    });
}

/// The version to restore from this one, and its button: Undo on the
/// newest restores the one before it (or clears the rules), Restore on an
/// older one brings it back.
fn target(file: &RulesFile, version: i64) -> Option<(i64, String)> {
    if version == file.version {
        let previous = version - 1;
        if previous < 0 {
            return None;
        }
        Some((
            previous,
            if previous == 0 {
                "Clear All Rules".to_owned()
            } else {
                "Undo This Change".to_owned()
            },
        ))
    } else {
        Some((version, format!("Restore v{version}")))
    }
}

fn version_screen(bot: Uuid, number: i64, rules: Resource<RulesFile, String>) -> Screen {
    let request = signal(Request::<(i64, i64)>::Idle);
    let restore = move |target: i64, expected: i64, label: String| {
        session::spawn_held(async move {
            let choice = alert_dialog()
                .title(format!("{label}?"))
                .description(
                    "This saves a new version of the rules. The bot follows it from its next turn.",
                )
                .cancel("cancel", "Cancel")
                .action("restore", label)
                .show()
                .await;
            if !matches!(choice, Ok(r) if r.action.as_deref() == Some("restore")) {
                return;
            }
            let client = session::server().client.clone();
            session::submit(
                request,
                (target, expected),
                move |(target, expected)| client.restore_rules(bot, target, expected),
                move |()| {
                    request.try_set(Request::Idle);
                    rules.refetch();
                },
            );
        });
    };
    let body = dynamic(move || {
        let file = rules.get()?;
        let version = file.history.iter().find(|v| v.version == number)?.clone();
        let action = target(&file, number);
        let expected = file.version;
        let diff: Vec<AnyElement> = version
            .diff
            .iter()
            .map(|line| {
                let (mark, tone) = match line.op {
                    DiffOp::Add => ("+ ", colors::tint()),
                    DiffOp::Remove => ("− ", colors::destructive()),
                    _ => ("  ", colors::secondary()),
                };
                text(format!("{mark}{}", line.text))
                    .selectable(true)
                    .px_4()
                    .py(px(2.0))
                    .text_size(px(13.0))
                    .line_height(px(18.0))
                    .font_family("Menlo")
                    .text_color(tone)
                    .into_element()
            })
            .collect();
        Some(
            view()
                .gap_6()
                .child(list::section(
                    None,
                    [view()
                        .px_4()
                        .py(px(11.0))
                        .gap(px(4.0))
                        .child(
                            text(version.title())
                                .text_size(px(17.0))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(colors::label()),
                        )
                        .child(
                            text(byline(bot, &version))
                                .text_size(px(15.0))
                                .text_color(colors::secondary()),
                        )
                        .child((!version.owner_words.trim().is_empty()).then(|| {
                            text(format!("“{}”", version.owner_words.trim()))
                                .text_size(px(15.0))
                                .line_height(px(20.0))
                                .text_color(colors::label())
                        }))
                        .into_element()],
                    None,
                ))
                .child((!diff.is_empty()).then(|| {
                    list::section(
                        Some("Changes"),
                        [view().py_2().children(diff).into_element()],
                        None,
                    )
                }))
                .child(action.map(|(target, label)| {
                    let destructive = target == 0;
                    list::card([list::action_row(
                        label.clone(),
                        destructive,
                        move || !request.with(Request::busy),
                        move || restore(target, expected, label.clone()),
                    )])
                })),
        )
    });
    let message = dynamic(move || {
        request
            .with(|r| r.error().map(str::to_owned))
            .map(parts::footnote)
    });
    nav_page()
        .key(format!("rules-{bot}-v{number}"))
        .title(format!("v{number}"))
        .header_large_title(false)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .child(view().py_6().gap_4().child(body).child(message)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn file(version: i64) -> RulesFile {
        serde_json::from_value(json!({
            "version": version, "markdown": "", "sections": [], "history": [],
        }))
        .unwrap()
    }

    #[test]
    fn the_newest_version_undoes_and_an_older_one_restores() {
        assert_eq!(target(&file(3), 3), Some((2, "Undo This Change".into())));
        assert_eq!(target(&file(1), 1), Some((0, "Clear All Rules".into())));
        assert_eq!(target(&file(3), 2), Some((2, "Restore v2".into())));
    }
}
