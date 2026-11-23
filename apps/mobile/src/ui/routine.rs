//! One routine or reminder, opened from the bot's details: when it runs,
//! its task, and the owner's controls, as the desktop's panel has them:
//! on or off, quiet runs, retrying a failure or clearing it, and deleting.
//! It shows the routine from the open chat, so it follows live updates.

use std::collections::HashMap;

use else_wire::routines::{PausedReason, Routine};
use nsh_base::{alert_dialog, nav_page};
use nsh_core::prelude::*;
use uuid::Uuid;

use super::list;
use super::parts::{self, colors};
use super::session::{self, Chat};
use else_client::request::Request;

/// Retries sent whose answer is not in yet, by routine: reopening the
/// screen resends the same request instead of queueing another run.
#[derive(Clone, Default, PartialEq)]
struct PendingRetries(HashMap<Uuid, Uuid>);

fn pending_retries() -> Signal<PendingRetries> {
    global::<PendingRetries>()
}

fn key(routine: Uuid) -> String {
    format!("routine-{routine}")
}

pub fn open(bot: Uuid, routine: Uuid, chat: Signal<Chat>) {
    super::push(key(routine), move || screen(bot, routine, chat));
}

/// What the screen asks of the server. Each is safe to repeat: switches
/// carry their value, Retry its request ID.
#[derive(Debug, Clone, PartialEq)]
enum Change {
    Enable(bool),
    Quiet(bool),
    Retry(Uuid),
    ClearFailures,
    Delete,
}

/// Whether the owner may run it again now: failures not cleared, or
/// else.bot paused it. The API accepts a retry under the same rule.
fn retryable(routine: &Routine) -> bool {
    routine.failed_runs > 0 || (!routine.enabled && routine.paused_reason.is_some())
}

fn why_paused(routine: &Routine) -> Option<String> {
    if routine.enabled {
        return None;
    }
    Some(match routine.paused_reason {
        Some(PausedReason::Repeated) => format!(
            "else.bot paused it after {} failed runs.",
            routine.failed_runs
        ),
        Some(PausedReason::NeedsAction) => {
            "else.bot paused it: retrying won't fix the last failure.".to_owned()
        }
        _ => "Off. It won't run until you turn it on.".to_owned(),
    })
}

fn screen(bot: Uuid, id: Uuid, chat: Signal<Chat>) -> Screen {
    let routine = computed(move || {
        chat.with(|c| {
            c.conversation().and_then(|conversation| {
                conversation
                    .extras()
                    .routines
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .find(|r| r.id == id)
                    .cloned()
            })
        })
    });
    let request = signal(Request::<Change>::Idle);
    let idle = move || !request.with(Request::busy);
    let send = move |change: Change| {
        let leave = change == Change::Delete;
        let client = session::server().client.clone();
        session::submit(
            request,
            change,
            move |change| async move {
                match change {
                    Change::Enable(on) => client.set_routine_enabled(bot, id, on).await.map(drop),
                    Change::Quiet(on) => client.set_routine_quiet(bot, id, on).await.map(drop),
                    Change::Retry(request_id) => {
                        client.retry_routine(bot, id, request_id).await.map(drop)
                    }
                    Change::ClearFailures => {
                        client.dismiss_routine_failures(bot, id).await.map(drop)
                    }
                    Change::Delete => client.delete_routine(bot, id).await,
                }
            },
            // The bot's update brings the routine as the server saved it.
            move |()| {
                request.try_set(Request::Idle);
                if leave {
                    super::routes().update(|r| r.0.retain(|route| *route.key != *key(id)));
                }
            },
        );
    };
    let retry = move || {
        // The same request ID until the server answered, even across a
        // reopened screen.
        let request_id = pending_retries().with_untracked(|p| p.0.get(&id).copied());
        let request_id = request_id.unwrap_or_else(|| {
            let fresh = Uuid::new_v4();
            pending_retries().update(|p| {
                p.0.insert(id, fresh);
            });
            fresh
        });
        let client = session::server().client.clone();
        session::submit(
            request,
            Change::Retry(request_id),
            move |_| async move { client.retry_routine(bot, id, request_id).await.map(drop) },
            move |()| {
                pending_retries().update(|p| {
                    p.0.remove(&id);
                });
                request.try_set(Request::Idle);
            },
        );
    };
    let delete = move || {
        let Some((name, reminder)) =
            routine.with_untracked(|r| r.as_ref().map(|r| (r.name.clone(), r.run_at.is_some())))
        else {
            return;
        };
        spawn(async move {
            let choice = alert_dialog()
                .title(format!("Delete {name}?"))
                .description(if reminder {
                    "The reminder leaves the list and its task won't run. A run already in progress finishes."
                } else {
                    "The routine leaves the list and won't run again. A run already in progress finishes."
                })
                .cancel("cancel", "Cancel")
                .destructive("delete", "Delete")
                .show()
                .await;
            if matches!(choice, Ok(r) if r.action.as_deref() == Some("delete")) {
                send(Change::Delete);
            }
        });
    };

    let body = dynamic(move || {
        let Some(current) = routine.get() else {
            return Some(parts::footnote("This routine is no longer on the list.").into_element());
        };
        let reminder = current.run_at.is_some();
        let mut when = vec![current.schedule()];
        when.extend(current.next_run());
        let last = current.last_run_line();
        let enabled = current.enabled;
        let quiet = current.quiet;
        // A retry the server is running already disables another.
        let can_retry = retryable(&current);
        let retrying = current.retrying;
        let failed = current.failed_runs > 0;
        Some(untracked(|| {
            view()
                .gap_6()
                .child(list::section(
                    Some(if reminder { "When" } else { "Schedule" }),
                    when.into_iter()
                        .map(|line| {
                            text(line)
                                .px_4()
                                .py(px(11.0))
                                .text_size(px(17.0))
                                .text_color(colors::label())
                                .into_element()
                        })
                        .collect::<Vec<_>>(),
                    last.map(|(line, _)| line),
                ))
                .child(list::section(
                    Some("Task"),
                    [text(current.prompt.clone())
                        .selectable(true)
                        .px_4()
                        .py(px(11.0))
                        .text_size(px(15.0))
                        .line_height(px(20.0))
                        .text_color(colors::label())
                        .into_element()],
                    None,
                ))
                .child(
                    view()
                        .child(list::card(
                            std::iter::once(list::switch_row(
                                "On",
                                move || enabled,
                                idle,
                                move |on| send(Change::Enable(on)),
                            ))
                            .chain((!reminder).then(|| {
                                list::switch_row(
                                    "Only Post News",
                                    move || quiet,
                                    idle,
                                    move |on| send(Change::Quiet(on)),
                                )
                            })),
                        ))
                        .child(parts::footnote(why_paused(&current).unwrap_or_else(|| {
                            if reminder {
                                "It runs once, then leaves the list.".to_owned()
                            } else {
                                "With Only Post News, a run with nothing new stays silent."
                                    .to_owned()
                            }
                        }))),
                )
                .child((can_retry || failed).then(|| {
                    list::card(
                        can_retry
                            .then(|| {
                                list::action_row(
                                    if retrying { "Retrying…" } else { "Retry Now" },
                                    false,
                                    move || idle() && !retrying,
                                    retry,
                                )
                            })
                            .into_iter()
                            .chain(failed.then(|| {
                                list::action_row("Clear Failures", false, idle, move || {
                                    send(Change::ClearFailures)
                                })
                            })),
                    )
                }))
                .child(list::card([list::action_row(
                    if reminder {
                        "Delete Reminder"
                    } else {
                        "Delete Routine"
                    },
                    true,
                    idle,
                    delete,
                )]))
                .into_element()
        }))
    });
    let message = dynamic(move || {
        request
            .with(|r| r.error().map(str::to_owned))
            .map(parts::footnote)
    });
    nav_page()
        .key(key(id))
        .title(move || {
            routine.with(|r| {
                r.as_ref()
                    .map_or_else(|| "Routine".to_owned(), |r| r.name.clone())
            })
        })
        .header_large_title(false)
        .child(
            scroll()
                .flex_1()
                .bg(colors::background())
                .child(view().py_6().gap_4().child(message).child(body)),
        )
}
