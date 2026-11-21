mod card;

use super::schedule_text::local_time;
use super::{BotPanel, PanelEvent, automation_section};
use crate::app::delete_dialog::{DeleteTarget, open_delete_confirmation};
use crate::app::request::{Request, Settle};
use else_wire::routines::PausedReason;
use else_wire::routines::RunStatus;
use else_wire::routines::SkipReason;
use else_wire::routines::{LastRun, Routine};
use gpui_kit::*;
use uuid::Uuid;

/// A change to one routine. `Enable` and `Quiet` carry the value the
/// switch and menu show until the server answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RoutineChange {
    Enable(bool),
    Quiet(bool),
    DismissFailures,
    Retry,
    Model,
    Delete,
}

impl BotPanel {
    fn set_routine(
        &mut self,
        routine: Uuid,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (client, bot) = (self.client.clone(), self.bot);
        let request = move |_| client.set_routine_enabled(bot, routine, enabled);
        let change = RoutineChange::Enable(enabled);
        self.save_routine(routine, change, request, window, cx);
    }

    fn set_routine_quiet(
        &mut self,
        routine: Uuid,
        quiet: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (client, bot) = (self.client.clone(), self.bot);
        let request = move |_| client.set_routine_quiet(bot, routine, quiet);
        let change = RoutineChange::Quiet(quiet);
        self.save_routine(routine, change, request, window, cx);
    }

    fn dismiss_routine_failures(
        &mut self,
        routine: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (client, bot) = (self.client.clone(), self.bot);
        let request = move |_| client.dismiss_routine_failures(bot, routine);
        let change = RoutineChange::DismissFailures;
        self.save_routine(routine, change, request, window, cx);
    }

    pub(in crate::app) fn retry_routine(
        &mut self,
        routine: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (client, bot) = (self.client.clone(), self.bot);
        let request = move |attempt| client.retry_routine(bot, routine, attempt);
        let change = RoutineChange::Retry;
        self.save_routine(routine, change, request, window, cx);
    }

    /// Sends `change` through `request`, which gets the attempt as its
    /// request ID.
    fn save_routine<F>(
        &mut self,
        routine: Uuid,
        change: RoutineChange,
        request: impl FnOnce(Uuid) -> F,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) where
        F: std::future::Future<Output = Result<Routine, crate::api::ClientError>> + 'static,
    {
        let retrying = change == RoutineChange::Retry;
        let Some(attempt) = self.begin_routine_change(routine, change) else {
            return;
        };
        let request = request(attempt);
        cx.notify();
        let app = self.app.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = request.await;
            if let Err(err) = &result {
                crate::app::report_unauthorized(&app, err, cx);
            }
            let _ = this.update(cx, |panel, cx| {
                let outcome = panel.settle_routine_change(routine, attempt, &result, |err| {
                    if retrying {
                        retry_failure(err)
                    } else {
                        format!("Couldn't save. {err}")
                    }
                });
                if outcome.settled()
                    && let Ok(saved) = result
                {
                    // The row shows the routine as the server returned it.
                    panel.routines.remove(&routine);
                    cx.emit(PanelEvent::RoutineSaved(Box::new(saved)));
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(in crate::app::settings) fn changeable_routine(&self, routine: Uuid) -> Option<&Routine> {
        if !self.connected() {
            return None;
        }
        if self.routines.get(&routine).is_some_and(Request::busy) {
            return None;
        }
        self.automations()?
            .routines
            .as_deref()?
            .iter()
            .find(|r| r.id == routine)
    }

    /// Starts `change` unless the routine can't change now. Returns the
    /// attempt its result must settle.
    pub(in crate::app::settings) fn begin_routine_change(
        &mut self,
        routine: Uuid,
        change: RoutineChange,
    ) -> Option<Uuid> {
        self.changeable_routine(routine)?;
        self.routines.entry(routine).or_default().begin(change)
    }

    fn settle_routine_change<T>(
        &mut self,
        routine: Uuid,
        attempt: Uuid,
        result: &Result<T, crate::api::ClientError>,
        message: impl FnOnce(&crate::api::ClientError) -> String,
    ) -> Settle {
        self.routines
            .get_mut(&routine)
            .map_or(Settle::Stale, |save| {
                save.settle_as(attempt, result, message)
            })
    }

    /// Settles a change the model dialog sent; a saved routine shows as
    /// the server returned it.
    pub(in crate::app::settings) fn finish_routine_change<T>(
        &mut self,
        routine: Uuid,
        attempt: Uuid,
        result: &Result<T, crate::api::ClientError>,
        cx: &mut Context<Self>,
    ) {
        if self
            .settle_routine_change(routine, attempt, result, |err| {
                format!("Couldn't save. {err}")
            })
            .settled()
            && result.is_ok()
        {
            self.routines.remove(&routine);
        }
        cx.notify();
    }

    fn open_routine_delete_dialog(
        &mut self,
        routine: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.changeable_routine(routine).map(|r| match r.run_at {
            Some(_) => DeleteTarget::Reminder(r.name.clone()),
            None => DeleteTarget::Routine(r.name.clone()),
        }) else {
            return;
        };
        open_delete_confirmation(target, window, cx, move |panel, window, cx| {
            panel.delete_routine(routine, window, cx);
        });
    }

    fn delete_routine(&mut self, routine: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        let Some(attempt) = self.begin_routine_change(routine, RoutineChange::Delete) else {
            return;
        };
        cx.notify();
        let (client, bot) = (self.client.clone(), self.bot);
        let app = self.app.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = client.delete_routine(bot, routine).await;
            if let Err(err) = &result {
                crate::app::report_unauthorized(&app, err, cx);
            }
            let _ = this.update(cx, |panel, cx| {
                let outcome = panel.settle_routine_change(routine, attempt, &result, |err| {
                    format!("Couldn't delete. {err}")
                });
                if outcome.settled() && result.is_ok() {
                    panel.expanded_routine_problems.remove(&routine);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_routines(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.automations().map(|automations| {
            let connected = self.connected();
            automations
                .routines
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|routine| routine.run_at.is_none())
                .map(|routine| {
                    self.render_routine(
                        routine,
                        self.routines.get(&routine.id),
                        connected,
                        window,
                        cx,
                    )
                })
                .collect()
        });
        automation_section(
            "Routines",
            "No routines yet. Ask the bot in chat to do something on a schedule, for \
             example \"every weekday at 9, summarize my unread mail\". Its routines \
             appear here.",
            rows,
            cx,
        )
    }

    pub(super) fn render_reminders(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let rows = self.automations().map(|automations| {
            let mut reminders: Vec<&Routine> = automations
                .routines
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|routine| routine.run_at.is_some())
                .collect();
            reminders.sort_by_key(|reminder| (reminder.run_at, reminder.id));
            let connected = self.connected();
            reminders
                .into_iter()
                .map(|reminder| {
                    self.render_routine(
                        reminder,
                        self.routines.get(&reminder.id),
                        connected,
                        window,
                        cx,
                    )
                })
                .collect()
        });
        automation_section(
            "Reminders",
            "No reminders. Ask the bot to do something once, later, for example \
             \"in 6 hours, check how Scout is doing\". It appears here until it runs.",
            rows,
            cx,
        )
    }
}

/// Why a Retry queued nothing, in the owner's terms.
fn retry_failure(err: &crate::api::ClientError) -> String {
    match err.code() {
        Some("provider_unavailable") => "Couldn't retry. No provider can run this routine now. \
            Check Settings or choose another model for the routine."
            .to_owned(),
        Some("already_running") => "A run of this routine is already in progress.".to_owned(),
        Some("nothing_to_retry") => "This routine's last run no longer needs a retry.".to_owned(),
        Some("queue_full") => {
            "Couldn't retry. The bot's queue is full; try again once its requests finish."
                .to_owned()
        }
        _ => format!("Couldn't retry. {err}"),
    }
}

/// Whether the owner may run the routine again now: it has failures the
/// owner has not dismissed, or else.bot paused it. The API accepts a Retry
/// under the same rule.
pub(in crate::app) fn retryable(routine: &Routine) -> bool {
    routine.failed_runs > 0 || (!routine.enabled && routine.paused_reason.is_some())
}

/// What a routine shows under its schedule.
#[derive(Debug, PartialEq, Eq)]
enum Banner {
    /// How a retry is going.
    Progress(String),
    /// Why the routine stopped, or why the owner's last change didn't hold.
    Problem(String),
}

impl Banner {
    fn text(&self) -> &str {
        match self {
            Self::Progress(text) | Self::Problem(text) => text,
        }
    }
}

/// A change that failed outranks a retry's progress: it explains why the
/// switch or menu choice didn't hold.
fn banner(routine: &Routine, save: Option<&Request<RoutineChange>>) -> Option<Banner> {
    if let Some(err) = save.and_then(Request::error) {
        return Some(Banner::Problem(err.to_owned()));
    }
    let (text, attention) = last_run(routine)?;
    if routine.retrying {
        Some(Banner::Progress(text))
    } else {
        attention.then_some(Banner::Problem(text))
    }
}

fn last_run(routine: &Routine) -> Option<(String, bool)> {
    let run = routine.last_run.as_ref()?;
    let when = local_time(run.due_at, &routine.timezone);
    let error = run.error.as_ref().and_then(|e| e.message.clone());
    if routine.retrying {
        let progress = match run.status {
            RunStatus::Queued => "Retrying: waiting to start",
            RunStatus::Checking | RunStatus::Running => "Retrying: running now",
            _ => "Retrying: checking the result",
        };
        return Some((progress.to_owned(), false));
    }
    if !routine.enabled
        && let Some(reason) = &routine.paused_reason
    {
        let why = match reason {
            PausedReason::Repeated => format!("Paused after {} failed runs", routine.failed_runs),
            _ => "Paused: retrying won't fix the last failure".to_owned(),
        };
        let detail = error.map_or_else(String::new, |e| format!(" ({e})"));
        return Some((format!("{why}{detail}"), true));
    }
    let (text, attention) = last_run_status(run, &when, error);
    Some((
        text,
        attention && (routine.failed_runs > 0 || run.status == RunStatus::Skipped),
    ))
}

fn last_run_status(run: &LastRun, when: &str, error: Option<String>) -> (String, bool) {
    let reason = |skip: Option<SkipReason>| match skip {
        Some(SkipReason::Overlap) => "the previous run was still going",
        Some(SkipReason::QueueFull) => "too many messages were waiting",
        Some(SkipReason::ProviderUnavailable) => "no provider was available",
        Some(SkipReason::CheckCancelled) => "its check was stopped",
        Some(SkipReason::CheckUnavailable) => "its check could not start",
        Some(SkipReason::NothingNew | SkipReason::Unknown) | None => "it could not start",
    };
    let status = run.status.as_str();
    match run.status {
        RunStatus::Skipped if run.skip_reason == Some(SkipReason::NothingNew) => {
            (format!("Last run {when}: nothing new"), false)
        }
        RunStatus::NoNews => (format!("Last run {when}: no news"), false),
        RunStatus::Failed | RunStatus::Uncertain | RunStatus::Interrupted => {
            let detail = error.unwrap_or_else(|| status.to_owned());
            (format!("Last run {when} {status}: {detail}"), true)
        }
        RunStatus::Skipped => (format!("Skipped {when}: {}", reason(run.skip_reason)), true),
        RunStatus::Checking => (format!("Checking for new work (due {when})"), false),
        RunStatus::Queued | RunStatus::Running => (format!("Running now (due {when})"), false),
        RunStatus::Completed | RunStatus::Other(_) => (format!("Last run {when}: {status}"), false),
    }
}

#[cfg(test)]
mod tests {
    use super::{Banner, Request, banner, last_run, retryable};
    use else_wire::routines::Routine;

    #[test]
    fn a_failed_change_shows_over_a_retry_in_progress() {
        let retrying: Routine = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::nil(), "name": "Tickets", "prompt": "p",
            "cron": "0 * * * *", "timezone": "Europe/Paris", "enabled": false,
            "next_run_at": null, "updated_at": "2026-11-08T03:00:00Z",
            "failed_runs": 1, "paused_reason": "needs_action", "retrying": true,
            "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "running",
                          "skip_reason": null, "error": null },
        }))
        .unwrap();
        assert_eq!(
            banner(&retrying, None),
            Some(Banner::Progress("Retrying: running now".to_owned()))
        );
        let failed = Request::Failed("Couldn't save. offline".to_owned());
        assert_eq!(
            banner(&retrying, Some(&failed)),
            Some(Banner::Problem("Couldn't save. offline".to_owned())),
            "the owner learns why the switch snapped back"
        );
    }

    #[test]
    fn a_routine_whose_last_run_failed_offers_retry_while_enabled_or_paused() {
        let routine = |enabled: bool, failed_runs: i64, paused: Option<&str>| {
            serde_json::from_value::<Routine>(serde_json::json!({
                "id": uuid::Uuid::nil(), "name": "Tickets", "prompt": "p",
                "cron": "0 9 * * 1-5", "timezone": "Europe/Paris", "enabled": enabled,
                "next_run_at": null, "updated_at": "2026-11-20T09:00:00Z",
                "failed_runs": failed_runs, "paused_reason": paused,
            }))
            .unwrap()
        };
        assert!(
            retryable(&routine(true, 1, None)),
            "an enabled routine's failed run"
        );
        assert!(retryable(&routine(false, 3, Some("repeated"))));
        assert!(!retryable(&routine(true, 0, None)), "nothing failed");
        assert!(
            !retryable(&routine(false, 0, None)),
            "the owner switched it off"
        );
    }

    #[test]
    fn a_routine_card_flags_pauses_and_undismissed_failures_only() {
        let routine = |extra: serde_json::Value| -> Routine {
            let mut value = serde_json::json!({
                "id": uuid::Uuid::nil(), "name": "Tickets", "prompt": "p",
                "cron": "0 * * * *", "timezone": "Europe/Paris", "enabled": true,
                "next_run_at": null, "updated_at": "2026-11-08T03:00:00Z",
                "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "failed",
                              "skip_reason": null,
                              "error": { "code": "provider_unavailable",
                                         "message": "the provider process exited" } },
            });
            for (key, field) in extra.as_object().unwrap() {
                value[key] = field.clone();
            }
            serde_json::from_value(value).unwrap()
        };
        let (text, attention) =
            last_run(&routine(serde_json::json!({ "failed_runs": 1 }))).unwrap();
        assert!(attention);
        assert!(
            text.ends_with("failed: the provider process exited"),
            "{text}"
        );
        let (_, attention) = last_run(&routine(serde_json::json!({}))).unwrap();
        assert!(!attention, "dismissed failures stay visible but quiet");
        let (text, attention) = last_run(&routine(serde_json::json!({
            "enabled": false, "failed_runs": 3, "paused_reason": "repeated",
        })))
        .unwrap();
        assert!(attention);
        assert_eq!(
            text,
            "Paused after 3 failed runs (the provider process exited)"
        );
        let (text, attention) = last_run(&routine(serde_json::json!({
            "enabled": false, "failed_runs": 1, "paused_reason": "needs_action", "retrying": true,
            "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "running",
                          "skip_reason": null, "error": null },
        })))
        .unwrap();
        assert!(!attention, "a retry in progress hides the old failure");
        assert_eq!(text, "Retrying: running now");
        let skipped = |reason: &str| {
            last_run(&routine(serde_json::json!({
                "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "skipped",
                              "skip_reason": reason, "error": null },
            })))
            .unwrap()
        };
        let (text, attention) = skipped("nothing_new");
        assert!(!attention, "a check that found nothing is a normal run");
        assert!(text.ends_with(": nothing new"), "{text}");
        let (text, attention) = skipped("check_unavailable");
        assert!(attention);
        assert!(text.ends_with(": its check could not start"), "{text}");
        let (text, attention) = last_run(&routine(serde_json::json!({
            "quiet": true,
            "last_run": { "due_at": "2026-11-08T02:00:00Z", "status": "no_news",
                          "skip_reason": null, "error": null },
        })))
        .unwrap();
        assert!(!attention, "a run with no news is a normal run");
        assert_eq!(text, "Last run Sun 8 Nov, 03:00: no news");
    }
}
