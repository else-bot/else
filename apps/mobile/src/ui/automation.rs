//! The owner's controls for what wakes or watches a bot, from its details:
//! a tap opens an action sheet, as iOS lists act on one item. Watchers can
//! be stopped; mail, social and webhook triggers turned off or on and
//! deleted, after a confirmation. Each request sets a value or removes the
//! item, so a repeat is harmless; the bot's live update shows the result.

use std::future::Future;

use else_client::api::ClientError;
use else_wire::email::MailTrigger;
use else_wire::routines::Webhook;
use else_wire::socials::Trigger;
use nsh_base::alert_dialog;
use uuid::Uuid;

use super::session;

/// Asks which action to take on `title`; `None` when the owner cancels.
async fn pick(title: String, actions: &[(&'static str, &'static str, bool)]) -> Option<String> {
    let mut sheet = alert_dialog().action_sheet().title(title);
    for (id, label, destructive) in actions {
        sheet = if *destructive {
            sheet.destructive(*id, *label)
        } else {
            sheet.action(*id, *label)
        };
    }
    let choice = sheet.cancel("cancel", "Cancel").show().await.ok()?;
    choice
        .action
        .filter(|id| id != "cancel")
        .map(|id| id.to_string())
}

/// Confirms a deletion: what stops, and that running work finishes.
async fn confirm(title: String, description: &'static str) -> bool {
    let choice = alert_dialog()
        .title(title)
        .description(description)
        .cancel("cancel", "Cancel")
        .destructive("delete", "Delete")
        .show()
        .await;
    matches!(choice, Ok(r) if r.action.as_deref() == Some("delete"))
}

/// How a change that removes an item treats an item already gone.
#[derive(Clone, Copy, PartialEq)]
enum Gone {
    /// Stopping or deleting: an item already gone is done.
    Done,
    /// Turning it off or on: an item gone is a failure to report.
    Failure,
}

/// Runs a change, says so when it failed, then reads the bot's lists
/// again so the row shows what the server kept even without live updates.
async fn run(bot: Uuid, gone: Gone, change: impl Future<Output = Result<(), ClientError>>) {
    let failure = match change.await {
        Ok(()) => None,
        Err(err)
            if gone == Gone::Done && matches!(err.code(), Some("not_found" | "not_active")) =>
        {
            None
        }
        Err(err) => Some(err.to_string()),
    };
    session::reload_chat(bot);
    if let Some(message) = failure {
        let _ = alert_dialog()
            .title("Couldn't save")
            .description(message)
            .cancel("ok", "OK")
            .show()
            .await;
    }
}

pub fn watcher(bot: Uuid, id: Uuid, purpose: String) {
    session::spawn_held(async move {
        if pick(purpose, &[("stop", "Stop Watching", true)])
            .await
            .is_some()
        {
            let client = session::server().client.clone();
            run(bot, Gone::Done, client.stop_watcher(bot, id)).await;
        }
    });
}

const DELETE_TRIGGER: &str =
    "It leaves the list and won't wake the bot again. Work already in progress finishes.";

pub fn mail(bot: Uuid, trigger: MailTrigger) {
    session::spawn_held(async move {
        let toggle = if trigger.wake {
            ("off", "Turn Off", false)
        } else {
            ("on", "Turn On", false)
        };
        let title = format!("New mail at {}", trigger.address);
        let Some(action) = pick(title.clone(), &[toggle, ("delete", "Delete Trigger", true)]).await
        else {
            return;
        };
        let client = session::server().client.clone();
        let account = trigger.account_id;
        match action.as_str() {
            "delete" if confirm(format!("Delete {title}?"), DELETE_TRIGGER).await => {
                run(bot, Gone::Done, client.delete_email_trigger(account, bot)).await;
            }
            // Turning it back on never grants read the owner withdrew, nor
            // brings back a trigger deleted meanwhile.
            "on" => {
                run(
                    bot,
                    Gone::Failure,
                    client.resume_email_trigger(account, bot),
                )
                .await
            }
            "off" => {
                run(
                    bot,
                    Gone::Failure,
                    client.set_email_wake(account, bot, false),
                )
                .await
            }
            _ => {}
        }
    });
}

pub fn social(bot: Uuid, trigger: Trigger) {
    session::spawn_held(async move {
        let toggle = if trigger.enabled {
            ("off", "Turn Off", false)
        } else {
            ("on", "Turn On", false)
        };
        let title = trigger.title().to_owned();
        let Some(action) = pick(title.clone(), &[toggle, ("delete", "Delete Trigger", true)]).await
        else {
            return;
        };
        let client = session::server().client.clone();
        let id = trigger.id;
        match action.as_str() {
            "delete" if confirm(format!("Delete {title}?"), DELETE_TRIGGER).await => {
                run(bot, Gone::Done, client.delete_social_trigger(bot, id)).await;
            }
            "on" | "off" => {
                run(bot, Gone::Failure, async {
                    client
                        .set_social_trigger_enabled(bot, id, action == "on")
                        .await
                        .map(drop)
                })
                .await
            }
            _ => {}
        }
    });
}

pub fn webhook(bot: Uuid, hook: Webhook) {
    session::spawn_held(async move {
        let toggle = if hook.enabled {
            ("off", "Turn Off", false)
        } else {
            ("on", "Turn On", false)
        };
        let title = hook.name.clone();
        let Some(action) = pick(title.clone(), &[toggle, ("delete", "Delete Webhook", true)]).await
        else {
            return;
        };
        let client = session::server().client.clone();
        match action.as_str() {
            "delete"
                if confirm(
                    format!("Delete {title}?"),
                    "Its URL stops working: calls are refused and won't wake the bot again. Work already in progress finishes.",
                )
                .await =>
            {
                run(bot, Gone::Done, client.delete_webhook(bot, hook.id)).await;
            }
            "on" | "off" => {
                run(bot, Gone::Failure, client.set_webhook_enabled(bot, hook.id, action == "on")).await
            }
            _ => {}
        }
    });
}
