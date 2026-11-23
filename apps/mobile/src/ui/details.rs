//! A bot's details, as the desktop's right panel shows them: its avatar
//! and name, its model, rules, routines, reminders, what it is watching,
//! and what wakes it. The lists come from the open chat, so they follow
//! the bot's live updates.

use chrono::{DateTime, Local, Utc};
use else_client::request::Request;
use else_wire::bots::Bot;
use else_wire::conversation::BotExtras;
use else_wire::email::AccountState;
use else_wire::routines::{PausedReason, Routine, WatcherStatus};
use else_wire::rules::RulesFile;
use else_wire::schedule::{
    last_woke, watcher_deadline, watcher_last_check, watcher_next, webhook_pace,
};
use nsh_base::{alert_dialog, nav_page};
use nsh_core::element::header::HeaderItem;
use nsh_core::prelude::*;
use uuid::Uuid;

use super::list;
use super::parts::{self, colors};
use super::session::{self, Chat};

fn key(bot: Uuid) -> String {
    format!("details-{bot}")
}

pub fn open(bot: Uuid, chat: Signal<Chat>) {
    super::push(key(bot), move || screen(bot, chat));
}

/// The open chat's lists, as far as the chat has loaded.
#[derive(Clone, PartialEq)]
enum Lists {
    Loading,
    Failed(String),
    /// `stale` while the chat fetches a newer page after a gap.
    Ready {
        extras: BotExtras,
        stale: bool,
    },
}

fn screen(bot: Uuid, chat: Signal<Chat>) -> Screen {
    let lists = computed(move || {
        chat.with(|c| match c {
            Chat::Loading => Lists::Loading,
            Chat::Failed(message) => Lists::Failed(message.clone()),
            Chat::Ready(conversation) => Lists::Ready {
                extras: conversation.extras().as_ref().clone(),
                stale: false,
            },
            Chat::Recovering(conversation) => Lists::Ready {
                extras: conversation.extras().as_ref().clone(),
                stale: true,
            },
        })
    });
    // Reconnecting reads again. The first read does not wait for the
    // socket: a proxy may serve HTTP while it blocks WebSockets.
    let reachable = computed(move || {
        !matches!(
            session::live_state().get(),
            session::LiveState::Offline(_) | session::LiveState::Unauthorized
        )
    });
    // Back on top of the stack (from the Model or Rules screen, or after
    // another device changed something), the model and rules are read
    // again: an owner's change on another device sends no live event.
    let on_top = computed(move || {
        super::routes().with(|r| r.0.last().is_some_and(|route| *route.key == *key(bot)))
    });
    // The model is read again after each bot.updated: the bot may have
    // changed its own model.
    let updates = computed(move || chat.with(|c| c.conversation().map_or(0, |c| c.bot_updates())));
    let model = resource(
        move || on_top.get().then(|| (updates.get(), reachable.get())),
        move |_, _| super::model::load_profile(session::server().client.clone(), bot),
    );
    // The rules are read again whenever the bot saves a new version, also
    // while the Rules page shows them.
    let version = computed(move || match lists.get() {
        Lists::Ready { extras, .. } => extras.rules_version,
        Lists::Loading | Lists::Failed(_) => None,
    });
    let rules = resource(
        move || Some((version.get(), on_top.get(), reachable.get())),
        move |_, _| {
            let client = session::server().client.clone();
            async move { client.rules(bot).await.map_err(|err| err.to_string()) }
        },
    );

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

    // The Model screen loads and retries on its own, so the row always
    // opens it, even while this screen's read is failing.
    let model_row = dynamic(move || {
        let detail = match (model.get(), model.error()) {
            (Some(info), _) => match info.saved {
                Some(p) if p.own => super::model::summary(&info.providers, &p.choice),
                Some(p) => format!(
                    "{} (default)",
                    super::model::summary(&info.providers, &p.choice)
                ),
                None => "Choose".to_owned(),
            },
            (None, Some(_)) => "Couldn't load".to_owned(),
            (None, None) => "Loading…".to_owned(),
        };
        Some(list::link_row(
            "Model",
            detail,
            Some(move || super::model::open(bot)),
        ))
    });

    let rules_row = dynamic(move || {
        let (detail, any) = match (rules.get(), rules.error()) {
            (Some(file), _) => match file.sections.len() {
                0 => ("None yet".to_owned(), false),
                1 => ("1 rule".to_owned(), true),
                n => (format!("{n} rules"), true),
            },
            (None, Some(_)) => ("Couldn't load".to_owned(), false),
            (None, None) => ("Loading…".to_owned(), false),
        };
        Some(list::link_row(
            "Rules",
            detail,
            any.then_some(move || open_rules(bot, rules)),
        ))
    });

    let lists = dynamic(move || {
        Some(match lists.get() {
            Lists::Loading => parts::footnote("Loading…").into_element(),
            Lists::Failed(message) => parts::footnote(message).into_element(),
            Lists::Ready { extras, stale } => {
                let now = Utc::now();
                view()
                    .gap_6()
                    .child(stale.then(|| parts::footnote("Updating…")))
                    .child(routines(&extras, bot, chat))
                    .child(reminders(&extras, bot, chat))
                    .child(watching(&extras, now, bot))
                    .child(triggers(&extras, bot))
                    .into_element()
            }
        })
    });

    let edit_item = move || {
        bot_record
            .get()
            .map(|record| {
                vec![
                    HeaderItem::new("Edit")
                        .key("edit")
                        .on_press(move || super::edit_bot::open(record.clone())),
                ]
            })
            .unwrap_or_default()
    };
    nav_page()
        .key(key(bot))
        .title("Details")
        .header_large_title(false)
        .header_right_items(edit_item)
        .child(
            scroll().flex_1().bg(colors::background()).child(
                view()
                    .py_6()
                    .gap_6()
                    .child(header)
                    .child(
                        view()
                            .child(list::card([
                                model_row.into_element(),
                                rules_row.into_element(),
                            ]))
                            .child(parts::footnote(
                                "Rules are what the bot does whenever something happens. Ask it in the chat to add or change one.",
                            )),
                    )
                    .child(mute_section(bot, bot_record))
                    .child(lists)
                    .child(delete_section(bot, bot_record)),
            ),
        )
}

/// The bot's category, opening the picker.
fn category_row(bot: Uuid) -> impl IntoElement {
    dynamic(move || {
        let name = session::bots().with(|b| {
            let id =
                b.list.iter().find(|it| it.id == bot).and_then(|it| {
                    else_client::layout::known_category(&b.categories, it.category_id)
                });
            id.and_then(|id| b.categories.iter().find(|c| c.id == id))
                .map_or_else(|| "Unassigned".to_owned(), |c| c.name.clone())
        });
        Some(list::link_row(
            "Category",
            name,
            Some(move || super::categories::open_picker(bot)),
        ))
    })
}

/// Mute: a muted bot is unread and announced only while it waits on the
/// owner. The switch shows what the server saved.
fn mute_section(bot: Uuid, record: Memo<Option<Bot>>) -> View {
    let request = signal(Request::<bool>::Idle);
    let muted = move || record.with(|b| b.as_ref().is_some_and(|b| b.muted));
    let set = move |on: bool| {
        let client = session::server().client.clone();
        session::submit(
            request,
            on,
            move |on| client.set_muted(bot, on),
            move |saved| {
                session::update_bot(saved);
                request.try_set(Request::Idle);
            },
        );
    };
    view()
        .child(list::card([
            category_row(bot).into_element(),
            list::switch_row("Mute", muted, move || !request.with(Request::busy), set),
        ]))
        .child(parts::footnote(move || {
            request
                .with(|r| r.error().map(str::to_owned))
                .unwrap_or_else(|| {
                    "A muted bot shows as unread only while it waits on you.".to_owned()
                })
        }))
}

/// Deleting the bot, after a confirmation: its work stops and it leaves
/// the list; its history, memory and files are kept. The server's main
/// bot cannot be deleted.
fn delete_section(bot: Uuid, record: Memo<Option<Bot>>) -> impl IntoElement {
    let request = signal(Request::<()>::Idle);
    let main = computed(move || session::bots().with(|b| b.default_bot_id == Some(bot)));
    let delete = move || {
        let name = record.with_untracked(|b| b.as_ref().map(|b| b.name.clone()));
        let Some(name) = name else { return };
        session::spawn_held(async move {
            let choice = alert_dialog()
                .title(format!("Delete {name}?"))
                .description(
                    "Any work in progress stops and the bot leaves your list. Its history, memory and files are kept.",
                )
                .cancel("cancel", "Cancel")
                .destructive("delete", "Delete")
                .show()
                .await;
            if !matches!(choice, Ok(r) if r.action.as_deref() == Some("delete")) {
                return;
            }
            let client = session::server().client.clone();
            session::submit(
                request,
                (),
                move |()| client.delete_bot(bot),
                move |_| {
                    // Its chat and the screens over it close with it.
                    let id = bot.to_string();
                    super::routes().update(|r| r.0.retain(|route| !route.key.contains(&id)));
                    session::refresh_bots();
                },
            );
        });
    };
    when(
        move || !main.get(),
        move || {
            view()
                .child(list::card([list::action_row(
                    "Delete Bot",
                    true,
                    move || !request.with(Request::busy),
                    delete,
                )]))
                .child(dynamic(move || {
                    request
                        .with(|r| r.error().map(str::to_owned))
                        .map(parts::footnote)
                }))
        },
    )
}

/// One list of the screen: its title, then its rows or what to ask the
/// bot when it is empty.
fn list(title: &'static str, empty: &'static str, rows: Vec<View>) -> View {
    let rows: Vec<AnyElement> = if rows.is_empty() {
        vec![
            text(empty)
                .p_4()
                .text_size(px(15.0))
                .line_height(px(20.0))
                .text_color(colors::secondary())
                .into_element(),
        ]
    } else {
        rows.into_iter().map(IntoElement::into_element).collect()
    };
    list::section(Some(title), rows, None)
}

/// An item of a list: a title, then detail lines, a warning one in orange.
fn item(title: String, lines: Vec<String>, warning: Option<String>, body: Option<String>) -> View {
    view().px_4().py(px(11.0)).gap(px(2.0)).child(
        view()
            .gap(px(2.0))
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

/// A routine's or reminder's lines: when it runs, in its own time zone,
/// then its last run, in orange when it needs the owner.
fn routine_item(routine: &Routine) -> View {
    let mut lines = vec![routine.schedule()];
    lines.extend(routine.next_run());
    let (last, warning) = match routine.last_run_line() {
        Some((line, true)) => (None, Some(line)),
        Some((line, false)) => (Some(line), None),
        None if !routine.enabled => (None, Some(paused(routine))),
        None => (None, None),
    };
    lines.extend(last);
    item(
        routine.name.clone(),
        lines,
        warning,
        Some(routine.prompt.clone()),
    )
}

/// Why a routine without a run yet is off.
fn paused(routine: &Routine) -> String {
    match routine.paused_reason {
        Some(PausedReason::Repeated) => format!("Paused after {} failed runs", routine.failed_runs),
        Some(PausedReason::NeedsAction) => "Paused: retrying won't fix the last failure".to_owned(),
        _ => "Paused".to_owned(),
    }
}

/// A list item that opens the owner's actions on it.
fn pressable(item: View, on_press: impl Fn() + 'static) -> View {
    item.active(|s| s.bg(colors::fill()))
        .role(AccessibilityRole::Button)
        .on_press(on_press)
}

/// A routine's or reminder's row, opening its controls.
fn routine_row(routine: &Routine, bot: Uuid, chat: Signal<Chat>) -> View {
    let id = routine.id;
    pressable(routine_item(routine), move || {
        super::routine::open(bot, id, chat)
    })
}

fn routines(extras: &BotExtras, bot: Uuid, chat: Signal<Chat>) -> View {
    let rows = extras
        .routines
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|r| r.run_at.is_none())
        .map(|r| routine_row(r, bot, chat))
        .collect();
    list(
        "Routines",
        "No routines yet. Ask the bot in the chat to do something on a schedule, for example \"every weekday at 9, summarize my unread mail\".",
        rows,
    )
}

fn reminders(extras: &BotExtras, bot: Uuid, chat: Signal<Chat>) -> View {
    let mut due: Vec<&Routine> = extras
        .routines
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|r| r.run_at.is_some())
        .collect();
    due.sort_by_key(|r| (r.run_at, r.id));
    list(
        "Reminders",
        "No reminders. Ask the bot to do something once, later, for example \"in 6 hours, check how Scout is doing\".",
        due.into_iter().map(|r| routine_row(r, bot, chat)).collect(),
    )
}

fn watching(extras: &BotExtras, now: DateTime<Utc>, bot: Uuid) -> View {
    let today = Local::now().date_naive();
    let rows = extras
        .watchers
        .iter()
        .filter(|w| w.status != WatcherStatus::Ended)
        .map(|watcher| {
            let until = format!(
                "Until {}",
                watcher_deadline(watcher.until.with_timezone(&Local), today)
            );
            let mut lines = vec![capitalized(watcher_last_check(watcher, now))];
            lines.extend(watcher_next(watcher, now).map(|next| format!("{until} · {next}")));
            if lines.len() == 1 {
                lines.push(until);
            }
            let (id, purpose) = (watcher.id, watcher.purpose.clone());
            pressable(
                item(watcher.purpose.clone(), lines, None, None),
                move || super::automation::watcher(bot, id, purpose.clone()),
            )
        })
        .collect();
    list(
        "Watching",
        "Nothing being watched. Ask the bot to keep an eye on something, for example \"tell me when the build turns green\".",
        rows,
    )
}

fn capitalized(line: String) -> String {
    let mut chars = line.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => line,
    }
}

fn triggers(extras: &BotExtras, bot: Uuid) -> View {
    let woke = |at: Option<DateTime<Utc>>| at.map(last_woke);
    let parse = |at: &Option<String>| at.as_deref().and_then(|at| at.parse().ok());
    let mail = extras.mail_triggers.iter().map(|t| {
        let mut lines = vec!["Email · New mail".to_owned()];
        if !t.wake {
            lines.push("Off".to_owned());
        }
        lines.extend(woke(parse(&t.last_woke_at)));
        let warning = (t.state == AccountState::Paused).then(|| "Inbox paused".to_owned());
        let trigger = t.clone();
        pressable(
            item(t.address.clone(), lines, warning, t.instructions.clone()),
            move || super::automation::mail(bot, trigger.clone()),
        )
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
        lines.extend(woke(parse(&t.last_woke_at)));
        let trigger = t.clone();
        pressable(
            item(
                t.title().to_owned(),
                lines,
                None,
                Some(t.instructions.clone()),
            ),
            move || super::automation::social(bot, trigger.clone()),
        )
    });
    let webhooks = extras.webhooks.iter().map(|w| {
        let mut lines = vec![format!(
            "Webhook · {}",
            webhook_pace(w.min_interval_minutes)
        )];
        if !w.enabled {
            lines.push("Off".to_owned());
        }
        lines.extend(woke(w.last_wake.as_ref().map(|wake| wake.woke_at)));
        let hook = w.clone();
        pressable(
            item(w.name.clone(), lines, None, Some(w.prompt.clone())),
            move || super::automation::webhook(bot, hook.clone()),
        )
    });
    list(
        "Triggers",
        "No triggers yet. Ask the bot to react to something, for example \"wake up when new mail arrives from my accountant\".",
        mail.chain(social).chain(webhooks).collect(),
    )
}

fn open_rules(bot: Uuid, rules: Resource<RulesFile, String>) {
    super::push(format!("rules-{bot}"), move || rules_screen(bot, rules));
}

fn rules_screen(bot: Uuid, rules: Resource<RulesFile, String>) -> Screen {
    // Shown from the Details screen's read, so a rule the bot changes
    // updates here too.
    let body = dynamic(move || {
        let file = rules.get()?;
        let rows: Vec<View> = file
            .sections
            .into_iter()
            .map(|section| {
                view().px_4().py(px(11.0)).gap(px(4.0)).child(
                    view()
                        .gap(px(4.0))
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
        Some(list("Rules", "No rules right now.", rows))
    });
    nav_page()
        .key(format!("rules-{bot}"))
        .title("Rules")
        .header_large_title(false)
        .child(
            scroll().flex_1().bg(colors::background()).child(
                view()
                    .py_6()
                    .child(body)
                    .child(parts::footnote(
                        "Ask the bot in the chat to add, change or remove a rule. The desktop app shows each version and can restore one.",
                    )),
            ),
        )
}
