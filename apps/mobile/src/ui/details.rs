//! A bot's details, as the desktop's right panel shows them: its avatar
//! and name, its model, rules, routines, reminders, what it is watching,
//! and what wakes it. The lists come from the open chat, so they follow
//! the bot's live updates.

use chrono::{DateTime, Local, Utc};
use else_wire::conversation::BotExtras;
use else_wire::email::AccountState;
use else_wire::routines::{PausedReason, Routine, Watcher, WatcherStatus};
use else_wire::rules::RulesFile;
use else_wire::schedule::{describe_cron, describe_hours, local_time};
use else_wire::settings::{BotProfile, ProviderInfo};
use nsh_base::nav_page;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::parts::{self, colors};
use super::session::{self, Chat};

fn key(bot: Uuid) -> String {
    format!("details-{bot}")
}

pub fn open(bot: Uuid, chat: Signal<Chat>) {
    super::push(key(bot), move || screen(bot, chat));
}

/// What the server answered for a part of the screen loaded on open.
#[derive(Clone, PartialEq)]
enum Fetch<T> {
    Loading,
    Failed(String),
    Ready(T),
}

/// The model row's data: the providers, for labels, and the profile.
#[derive(Clone, PartialEq)]
struct Model {
    providers: Vec<ProviderInfo>,
    profile: Option<BotProfile>,
}

fn screen(bot: Uuid, chat: Signal<Chat>) -> Screen {
    let model = signal(Fetch::<Model>::Loading);
    // The Model screen writes its saved profile here, so the row updates.
    let saved = signal(None::<BotProfile>);
    let rules = signal(Fetch::<RulesFile>::Loading);
    let extras = computed(move || {
        chat.with(|c| c.conversation().map(|c| c.extras.clone()))
            .unwrap_or_default()
    });

    let client = session::server().client.clone();
    {
        let client = client.clone();
        spawn(async move {
            let (caps, profile) = futures_util::join!(
                client.clone().capabilities(),
                client.clone().bot_profile(bot)
            );
            model.set(match (caps, profile) {
                (Ok(caps), Ok(profile)) => Fetch::Ready(Model {
                    providers: super::model::offered(caps.providers),
                    profile,
                }),
                (Err(err), _) | (_, Err(err)) => Fetch::Failed(err.to_string()),
            });
        });
    }
    // Loaded on open and again whenever the bot saves a new version.
    let version = computed(move || extras.with(|e| e.rules_version));
    effect(move || {
        let _ = version.get();
        let client = client.clone();
        spawn(async move {
            rules.set(match client.rules(bot).await {
                Ok(file) => Fetch::Ready(file),
                Err(err) => Fetch::Failed(err.to_string()),
            });
        });
    });
    effect(move || {
        if let Some(profile) = saved.get() {
            model.update(|m| {
                if let Fetch::Ready(m) = m {
                    m.profile = Some(profile);
                }
            });
        }
    });

    let bot_record =
        computed(move || session::bots().with(|b| b.list.iter().find(|x| x.id == bot).cloned()));
    let header = view()
        .items_center()
        .gap_2()
        .pb_2()
        .child(parts::avatar(
            move || bot_record.with(|b| b.as_ref().map(|b| b.avatar).unwrap_or_default()),
            96.0,
        ))
        .child(
            text(move || {
                bot_record.with(|b| b.as_ref().map(|b| b.name.clone()).unwrap_or_default())
            })
            .text_size(px(22.0))
            .font_weight(FontWeight::BOLD)
            .text_color(colors::label()),
        );

    let model_row = dynamic(move || {
        let (detail, ready) = match model.get() {
            Fetch::Loading => ("Loading…".to_owned(), false),
            Fetch::Failed(message) => (message, false),
            Fetch::Ready(Model { providers, profile }) => match profile {
                Some(p) => {
                    let summary = super::model::summary(&providers, &p.choice);
                    if p.own {
                        (summary, true)
                    } else {
                        (format!("{summary} (default)"), true)
                    }
                }
                None => ("Choose".to_owned(), true),
            },
        };
        Some(link_row(
            "Model",
            detail,
            ready.then_some(move || super::model::open(bot, saved)),
        ))
    });

    let rules_row = dynamic(move || {
        let (detail, file) = match rules.get() {
            Fetch::Loading => ("Loading…".to_owned(), None),
            Fetch::Failed(message) => (message, None),
            Fetch::Ready(file) => match file.sections.len() {
                0 => ("None yet".to_owned(), None),
                1 => ("1 rule".to_owned(), Some(file)),
                n => (format!("{n} rules"), Some(file)),
            },
        };
        let open = file.map(|file| move || open_rules(bot, file.clone()));
        Some(link_row("Rules", detail, open))
    });

    let lists = dynamic(move || {
        let extras = extras.get();
        Some(
            view()
                .gap_6()
                .child(routines(&extras))
                .child(reminders(&extras))
                .child(watching(&extras))
                .child(triggers(&extras)),
        )
    });

    nav_page()
        .key(key(bot))
        .title("Details")
        .header_large_title(false)
        .child(
            scroll().flex_1().bg(colors::background()).child(
                view()
                    .py_6()
                    .gap_6()
                    .child(header)
                    .child(
                        view()
                            .child(
                                parts::group().child(
                                    view().mb(px(-0.5)).child(model_row).child(rules_row),
                                ),
                            )
                            .child(parts::footnote(
                                "Rules are what the bot does whenever something happens. Ask it in the chat to add or change one.",
                            )),
                    )
                    .child(lists),
            ),
        )
}

/// A row that opens another screen: its title, its value, and a chevron
/// when there is something to open.
fn link_row(title: &'static str, detail: String, open: Option<impl Fn() + 'static>) -> View {
    let enabled = open.is_some();
    view()
        .flex_row()
        .items_center()
        .min_h(px(44.0))
        .pl_4()
        .active(move |s| if enabled { s.bg(colors::fill()) } else { s })
        .role(AccessibilityRole::Button)
        .label(format!("{title}, {detail}"))
        .on_press(move || {
            if let Some(open) = &open {
                open();
            }
        })
        .child(
            view()
                .flex_1()
                .flex_row()
                .items_center()
                .gap_3()
                .pr_4()
                .py(px(11.0))
                .border_b(px(0.5))
                .border_color(colors::separator())
                .child(text(title).text_size(px(17.0)).text_color(colors::label()))
                .child(
                    text(detail)
                        .flex_1()
                        .number_of_lines(1)
                        .text_right()
                        .text_size(px(17.0))
                        .text_color(colors::secondary()),
                )
                .child(enabled.then(|| {
                    text("›")
                        .text_size(px(22.0))
                        .text_color(colors::secondary())
                })),
        )
}

/// One list of the screen: its title, then its rows or what to ask the
/// bot when it is empty.
fn list(title: &'static str, empty: &'static str, rows: Vec<View>) -> View {
    let heading = text(title.to_uppercase())
        .mx_4()
        .px_4()
        .pb(px(6.0))
        .text_size(px(13.0))
        .text_color(colors::secondary());
    if rows.is_empty() {
        return view().child(heading).child(
            parts::group().child(
                text(empty)
                    .p_4()
                    .text_size(px(15.0))
                    .line_height(px(20.0))
                    .text_color(colors::secondary()),
            ),
        );
    }
    view()
        .child(heading)
        // The group clips the last row's separator, as iOS lists end.
        .child(parts::group().child(view().mb(px(-0.5)).children(rows)))
}

/// An item of a list: a title, then detail lines, a warning one in orange.
fn item(title: String, lines: Vec<String>, warning: Option<String>, body: Option<String>) -> View {
    view().pl_4().child(
        view()
            .py(px(11.0))
            .pr_4()
            .gap(px(2.0))
            .border_b(px(0.5))
            .border_color(colors::separator())
            .child(text(title).text_size(px(17.0)).text_color(colors::label()))
            .children(lines.into_iter().map(|line| {
                text(line)
                    .text_size(px(15.0))
                    .line_height(px(20.0))
                    .text_color(colors::secondary())
            }))
            .child(warning.map(|line| {
                text(line)
                    .text_size(px(15.0))
                    .line_height(px(20.0))
                    .text_color(colors::warning())
            }))
            .child(body.map(|body| {
                text(body)
                    .number_of_lines(3)
                    .text_size(px(15.0))
                    .line_height(px(20.0))
                    .text_color(colors::secondary())
            })),
    )
}

fn routines(extras: &BotExtras) -> View {
    let rows = extras
        .routines
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|r| r.run_at.is_none())
        .map(|routine| {
            let mut lines = vec![describe_cron(&routine.cron)];
            if let Some(hours) = &routine.active_hours {
                lines.push(describe_hours(hours));
            }
            if let (true, Some(at)) = (routine.enabled, routine.next_run_at) {
                lines.push(format!("Next run {}", local_time(at, &routine.timezone)));
            }
            item(
                routine.name.clone(),
                lines,
                warning(routine),
                Some(routine.prompt.clone()),
            )
        })
        .collect();
    list(
        "Routines",
        "No routines yet. Ask the bot in the chat to do something on a schedule, for example \"every weekday at 9, summarize my unread mail\".",
        rows,
    )
}

fn reminders(extras: &BotExtras) -> View {
    let mut due: Vec<&Routine> = extras
        .routines
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|r| r.run_at.is_some())
        .collect();
    due.sort_by_key(|r| (r.run_at, r.id));
    let rows = due
        .into_iter()
        .map(|reminder| {
            let when = reminder
                .run_at
                .map(|at| format!("Runs {}", local_time(at, &reminder.timezone)))
                .into_iter()
                .collect();
            item(
                reminder.name.clone(),
                when,
                warning(reminder),
                Some(reminder.prompt.clone()),
            )
        })
        .collect();
    list(
        "Reminders",
        "No reminders. Ask the bot to do something once, later, for example \"in 6 hours, check how Scout is doing\".",
        rows,
    )
}

/// Why a routine is not running, when it is not.
fn warning(routine: &Routine) -> Option<String> {
    if routine.enabled {
        return match routine.failed_runs {
            0 => None,
            1 => Some("Its last run failed".to_owned()),
            n => Some(format!("Its last {n} runs failed")),
        };
    }
    Some(
        match routine.paused_reason {
            Some(PausedReason::NeedsAction) => "Paused: its last run needs you",
            Some(PausedReason::Repeated) => "Paused after several failed runs",
            _ => "Paused",
        }
        .to_owned(),
    )
}

fn watching(extras: &BotExtras) -> View {
    let rows = extras
        .watchers
        .iter()
        .filter(|w| w.status != WatcherStatus::Ended)
        .map(|watcher| {
            let mut lines = vec![format!("Until {}", local(watcher.until))];
            if let Some(next) = watcher.next_check_at {
                lines.push(format!("Next check {}", local(next)));
            }
            item(
                watcher.purpose.clone(),
                lines,
                watcher_problem(watcher),
                None,
            )
        })
        .collect();
    list(
        "Watching",
        "Nothing being watched. Ask the bot to keep an eye on something, for example \"tell me when the build turns green\".",
        rows,
    )
}

fn watcher_problem(watcher: &Watcher) -> Option<String> {
    watcher
        .last_status
        .clone()
        .filter(|_| watcher.status == WatcherStatus::Checking)
}

fn triggers(extras: &BotExtras) -> View {
    let mail = extras.mail_triggers.iter().map(|t| {
        let mut lines = vec!["Email · New mail".to_owned()];
        if !t.wake {
            lines.push("Off".to_owned());
        }
        let warning = (t.state == AccountState::Paused).then(|| "Inbox paused".to_owned());
        item(t.address.clone(), lines, warning, t.instructions.clone())
    });
    let social = extras.social_triggers.iter().map(|t| {
        let connection = t
            .username
            .as_ref()
            .map(|u| format!("@{u}"))
            .unwrap_or_else(|| t.connection_name.clone());
        let mut lines = vec![format!(
            "{} · {connection} · {}",
            t.provider.label(),
            t.wake.label()
        )];
        if !t.enabled {
            lines.push("Off".to_owned());
        }
        item(
            t.title().to_owned(),
            lines,
            None,
            Some(t.instructions.clone()),
        )
    });
    let webhooks = extras.webhooks.iter().map(|w| {
        let mut lines = vec!["Webhook".to_owned()];
        if !w.enabled {
            lines.push("Off".to_owned());
        }
        item(w.name.clone(), lines, None, Some(w.prompt.clone()))
    });
    list(
        "Triggers",
        "No triggers yet. Ask the bot to react to something, for example \"wake up when new mail arrives from my accountant\".",
        mail.chain(social).chain(webhooks).collect(),
    )
}

/// A moment in the phone's time zone, as the routines show theirs.
fn local(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local)
        .format("%a %-d %b, %H:%M")
        .to_string()
}

fn open_rules(bot: Uuid, file: RulesFile) {
    super::push(format!("rules-{bot}"), move || {
        rules_screen(bot, file.clone())
    });
}

fn rules_screen(bot: Uuid, file: RulesFile) -> Screen {
    let rows = file
        .sections
        .into_iter()
        .map(|section| {
            view().pl_4().child(
                view()
                    .py(px(11.0))
                    .pr_4()
                    .gap(px(4.0))
                    .border_b(px(0.5))
                    .border_color(colors::separator())
                    .child(
                        text(section.title)
                            .text_size(px(17.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(colors::label()),
                    )
                    .child(
                        text(section.body)
                            .selectable(true)
                            .text_size(px(15.0))
                            .line_height(px(20.0))
                            .text_color(colors::label()),
                    ),
            )
        })
        .collect();
    nav_page()
        .key(format!("rules-{bot}"))
        .title("Rules")
        .header_large_title(false)
        .child(
            scroll().flex_1().bg(colors::background()).child(
                view()
                    .py_6()
                    .child(list("Rules", "", rows))
                    .child(parts::footnote(
                        "Ask the bot in the chat to add, change or remove a rule. The desktop app shows each version and can restore one.",
                    )),
            ),
        )
}
