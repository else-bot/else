//! A bot's request to install a skill: what it is and where it comes from,
//! its files to review, who may use it, the keys it needs, then Install or
//! Decline.

use std::collections::BTreeMap;
use std::fmt;

use else_wire::skills::{SkillFile, SkillInstall, SkillScope};
use nsh_base::{input, nav_page};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::super::files::size_label;
use super::super::list;
use super::super::parts::colors;
use super::super::session;
use super::{Fill, button, eyebrow, failure, frame};
use else_client::request::Request;

/// Keys typed for the skill. They never print, even in a debug log.
#[derive(Clone, PartialEq)]
struct Keys(BTreeMap<String, String>);

impl fmt::Debug for Keys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.0.keys()).finish()
    }
}

/// What the card asks of the server: install for `scope`, with keys or
/// without them, or decline.
#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Install {
        scope: SkillScope,
        keys: Option<Keys>,
    },
    Decline,
}

/// The keys as typed: none at all to add them later, or every one.
fn keys(names: &[String], typed: &BTreeMap<String, String>) -> Result<Option<Keys>, String> {
    let filled: BTreeMap<String, String> = names
        .iter()
        .map(|name| (name.clone(), typed.get(name).cloned().unwrap_or_default()))
        .collect();
    if filled.values().all(String::is_empty) {
        return Ok(None);
    }
    if filled.values().any(String::is_empty) {
        return Err("Fill in every key, or leave them all empty to add them later.".into());
    }
    Ok(Some(Keys(filled)))
}

fn bot_name(bot: Uuid) -> String {
    session::bots().with_untracked(|b| {
        b.list
            .iter()
            .find(|it| it.id == bot)
            .map_or_else(|| "this bot".to_owned(), |it| it.name.clone())
    })
}

/// "3 files · 4.2 kB".
fn files_line(files: &[SkillFile]) -> String {
    format!(
        "{} {} · {}",
        files.len(),
        if files.len() == 1 { "file" } else { "files" },
        size_label(files.iter().map(|file| file.size).sum())
    )
}

pub(super) fn card(bot: Uuid, install: SkillInstall) -> impl IntoElement {
    let request = signal(Request::<Answer>::Idle);
    let idle = move || !request.with(Request::busy);
    let scope = signal(SkillScope::All);
    let typed = signal(BTreeMap::<String, String>::new());
    let id = install.id;
    let name = bot_name(bot);
    let names = install.env.clone();
    let send = move |answer: Answer| {
        // The keys leave the fields with the request; a lost response
        // keeps them only in the request, for its resend.
        typed.set(BTreeMap::new());
        let client = session::server().client.clone();
        session::submit(
            request,
            answer,
            move |answer| async move {
                let approval = match answer {
                    Answer::Install { scope, keys } => Some((scope, keys.map(|k| k.0))),
                    Answer::Decline => None,
                };
                client
                    .answer_skill_install(bot, id, approval)
                    .await
                    .map(drop)
            },
            // Accepted, the card keeps no copy of the keys while it waits
            // for the server's update to replace it.
            move |()| {
                session::try_update(request, |r| {
                    if let Request::Accepted(Answer::Install { keys, .. }) = r {
                        *keys = None;
                    }
                });
            },
        );
    };
    let install_now = {
        let names = names.clone();
        move || match keys(&names, &typed.get_untracked()) {
            Ok(keys) => send(Answer::Install {
                scope: scope.get_untracked(),
                keys,
            }),
            Err(message) => request.update(|r| r.reject(message)),
        }
    };
    let scripts = install.files.iter().filter(|file| file.script).count();
    let review = {
        let install = install.clone();
        move || open_files(bot, install.clone())
    };
    let fields: Vec<AnyElement> = names
        .iter()
        .map(|var| key_field(var.clone(), typed))
        .collect();
    let install_label = {
        let name = name.clone();
        move || {
            SharedString::from(match scope.get() {
                SkillScope::Bot => format!("Install for {name}"),
                SkillScope::All | SkillScope::Unknown => "Install for all bots".to_owned(),
            })
        }
    };
    frame()
        .child(eyebrow("WANTS TO INSTALL A SKILL"))
        .child(
            text(install.name.clone())
                .text_size(px(17.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::label()),
        )
        .child(
            text(install.description.clone())
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::label()),
        )
        .child((!install.reason.trim().is_empty()).then(|| {
            text(format!("Why: {}", install.reason))
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::secondary())
        }))
        .child(
            text(install.source.label())
                .selectable(true)
                .text_size(px(13.0))
                .font_family("Menlo")
                .text_color(colors::secondary()),
        )
        .child(
            text(files_line(&install.files))
                .text_size(px(13.0))
                .text_color(colors::secondary()),
        )
        .child((scripts > 0).then(|| {
            text(format!(
                "{scripts} {} runs code on the bot's computer",
                if scripts == 1 { "script" } else { "scripts" }
            ))
            .text_size(px(13.0))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(colors::warning())
        }))
        .child(button("Review files", Fill::Plain, || true, review))
        .child(
            text("Who can use it")
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(colors::secondary()),
        )
        .child(
            view()
                .rounded(px(10.0))
                .overflow_hidden()
                .bg(colors::fill())
                .child(list::option_row(
                    "All bots".into(),
                    None,
                    move || scope.get() == SkillScope::All,
                    true,
                    move || {
                        if idle() {
                            scope.set(SkillScope::All)
                        }
                    },
                ))
                .child(view().ml_4().h(px(0.5)).bg(colors::separator()))
                .child(list::option_row(
                    format!("Only {name}"),
                    None,
                    move || scope.get() == SkillScope::Bot,
                    true,
                    move || {
                        if idle() {
                            scope.set(SkillScope::Bot)
                        }
                    },
                )),
        )
        .child((!fields.is_empty()).then(|| {
            view().gap_3().children(fields).child(
                text(format!(
                    "Saved as the task secret {}. You can leave the keys empty and add them later.",
                    install.secret
                ))
                .text_size(px(13.0))
                .line_height(px(18.0))
                .text_color(colors::secondary()),
            )
        }))
        .child(
            view()
                .flex_row()
                .gap_2()
                .child(button("Decline", Fill::PlainDestructive, idle, move || {
                    send(Answer::Decline)
                }))
                .child(button(install_label, Fill::Prominent, idle, install_now)),
        )
        .child(failure(request))
}

/// One masked field for a key the skill needs.
fn key_field(var: String, typed: Signal<BTreeMap<String, String>>) -> AnyElement {
    let key = var.clone();
    let shown = var.clone();
    view()
        .gap_1()
        .child(
            text(var.clone())
                .text_size(px(13.0))
                .font_family("Menlo")
                .text_color(colors::secondary()),
        )
        .child(
            input()
                .min_h(px(40.0))
                .px_3()
                .rounded(px(10.0))
                .bg(colors::fill())
                .value(move || typed.with(|t| t.get(&shown).cloned().unwrap_or_default()))
                .on_change(move |t: &str| {
                    typed.update(|v| {
                        v.insert(key.clone(), t.to_owned());
                    })
                })
                .masked(true)
                .auto_capitalize(AutoCapitalize::None)
                .auto_correct(false)
                .text_size(px(15.0))
                .text_color(colors::label())
                .caret_color(|| color(colors::tint()))
                .label(var),
        )
        .into_element()
}

/// The skill's files, as the server fetched them from the source: a list
/// to open each one from.
fn open_files(bot: Uuid, install: SkillInstall) {
    let key = format!("skill-{}", install.id);
    super::super::push(key.clone(), move || {
        files_screen(bot, install.clone(), key.clone())
    });
}

fn files_screen(bot: Uuid, install: SkillInstall, key: String) -> Screen {
    let id = install.id;
    let load = resource(
        || Some(()),
        move |(), _| {
            let client = session::server().client.clone();
            async move {
                client
                    .skill_install(bot, id)
                    .await
                    .map(|review| review.contents)
                    .map_err(|err| err.to_string())
            }
        },
    );
    let files = install.files.clone();
    let body = dynamic(move || match (load.get(), load.error()) {
        (None, None) => list::section(
            None,
            [text("Loading files…")
                .p_4()
                .text_color(colors::secondary())
                .into_element()],
            None,
        )
        .into_element(),
        (None, Some(message)) => list::section(
            None,
            [text(message)
                .p_4()
                .text_color(colors::destructive())
                .into_element()],
            None,
        )
        .into_element(),
        (Some(contents), _) => {
            let rows: Vec<AnyElement> = files
                .iter()
                .map(|file| {
                    let body = contents.get(&file.path).cloned();
                    let path = file.path.clone();
                    let mut value = size_label(file.size);
                    if file.script {
                        value = format!("Runs code · {value}");
                    }
                    list::link_row(
                        file.path.clone(),
                        value,
                        body.map(|body| move || open_file(id, path.clone(), body.clone())),
                    )
                })
                .collect();
            list::section(
                Some("Files"),
                rows,
                Some(format!("From {}", install.source.label())),
            )
            .into_element()
        }
    });
    nav_page()
        .key(key)
        .title(install.name.clone())
        .header_large_title(false)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .child(view().py_6().child(body)),
        )
}

fn open_file(install: Uuid, path: String, body: String) {
    let key = format!("skill-{install}-{path}");
    super::super::push(key.clone(), move || {
        nav_page()
            .key(key.clone())
            .title(path.clone())
            .header_large_title(false)
            .child(
                scroll().flex_1().bg(colors::background()).child(
                    text(body.clone())
                        .selectable(true)
                        .p_4()
                        .text_size(px(13.0))
                        .line_height(px(18.0))
                        .font_family("Menlo")
                        .text_color(colors::label()),
                ),
            )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_all_or_none() {
        let names = vec!["API_KEY".to_owned(), "API_SECRET".to_owned()];
        let typed = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(keys(&names, &typed(&[])), Ok(None));
        assert!(keys(&names, &typed(&[("API_KEY", "a")])).is_err());
        assert_eq!(
            keys(&names, &typed(&[("API_KEY", "a"), ("API_SECRET", "b")])),
            Ok(Some(Keys(typed(&[("API_KEY", "a"), ("API_SECRET", "b")]))))
        );
    }
}
