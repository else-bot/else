//! Watcher tools: a bot starts a periodic check that outlives its turn
//! (see [`crate::app::watchers`]), lists its watchers, and stops one.

use crate::http::tool_result::{Done, Refusal, ToolResult};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{AppState, Grant, fit, slice};
use crate::app::providers::Origin;
use crate::app::watchers::{
    DEFAULT_EVERY_MINUTES, MAX_EVERY_MINUTES, MAX_HOURS, MAX_PER_BOT, MIN_EVERY_MINUTES,
};
use crate::db::task_secrets;
use crate::db::watchers::{self, NewWatcher, Started, StopBy, Stopped};

pub(super) const START: &str = "computer_watch_start";
pub(super) const LIST: &str = "computer_watches";
pub(super) const STOP: &str = "computer_watch_stop";
const MAX_TEXT: usize = 8000;

pub(super) fn tools(guest: &Value) -> Vec<Value> {
    vec![
        json!({
            "name": START,
            "description": "Watch for something that may take hours or days (up to 30 days): \
                else.bot runs check in your computer every every_minutes until it prints \
                anything (found), fails 3 times in a row, or hours pass, then wakes you once \
                with prompt. Your turn ends now and the owner can keep chatting. The check \
                must only look, never act; act when woken.",
            "inputSchema": { "type": "object", "properties": {
                "check": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT,
                    "description": "Shell command, safe to repeat. Exit 0 printing nothing: not \
                        yet. Exit 0 printing something: found. A line starting STATUS: is shown \
                        to the owner and is not output." },
                "every_minutes": { "type": "integer", "minimum": MIN_EVERY_MINUTES,
                    "maximum": MAX_EVERY_MINUTES, "description": "Default 15." },
                "hours": { "type": "integer", "minimum": 1, "maximum": MAX_HOURS,
                    "description": "Deadline from now. Promise the owner nothing past it." },
                "purpose": { "type": "string", "minLength": 1, "maxLength": 300,
                    "description": "What it waits for, in the owner's words, e.g. example.com \
                        becomes available." },
                "prompt": { "type": "string", "minLength": 1, "maxLength": MAX_TEXT,
                    "description": "Your instructions to yourself for when it finds it, e.g. \
                        Register example.com, then tell the owner." },
                "secrets": { "type": "array", "maxItems": 4, "uniqueItems": true,
                    "items": { "type": "string", "pattern": "^[a-z][a-z0-9_-]{0,39}$" },
                    "description": "Vault entry names (see computer_secrets) whose variables every \
                        check gets. Read them from the environment; never print them." } },
                "required": ["check", "hours", "purpose", "prompt"], "additionalProperties": false },
            "annotations": guest,
        }),
        json!({
            "name": LIST,
            "description": "Your active watchers with their deadline, last status and next check.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "destructiveHint": false,
                             "idempotentHint": true, "openWorldHint": false },
        }),
        json!({
            "name": STOP,
            "description": "Stop one of your watchers. It does not wake you.",
            "inputSchema": { "type": "object", "properties": {
                "watch_id": { "type": "string", "format": "uuid" } },
                "required": ["watch_id"], "additionalProperties": false },
            "annotations": { "readOnlyHint": false, "destructiveHint": true,
                             "idempotentHint": true, "openWorldHint": false },
        }),
    ]
}

fn watcher_result(watcher: &watchers::Watcher) -> Value {
    let mut value = json!(else_wire::routines::Watcher::from(watcher));
    value["watch_id"] = json!(watcher.id);
    value
}

fn text(args: &Value, key: &str) -> Option<String> {
    args[key]
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

#[derive(Clone, Copy)]
pub(super) enum Op {
    Start,
    List,
    Stop,
}

pub(super) async fn call(state: &AppState, grant: &Grant, op: Op, args: &Value) -> ToolResult {
    let failed = |err: sqlx::Error| {
        tracing::warn!(error = %err, "a watcher tool failed");
        Err(Refusal::text(
            "WATCHER_ERROR: the watcher store failed. Try again.",
        ))
    };
    match op {
        Op::Start => {
            let (Some(check), Some(purpose), Some(prompt)) = (
                text(args, "check"),
                text(args, "purpose"),
                text(args, "prompt"),
            ) else {
                return Err(Refusal::text("check, purpose and prompt are required."));
            };
            if check.len() > MAX_TEXT || prompt.len() > MAX_TEXT || purpose.chars().count() > 300 {
                return Err(Refusal::text(
                    "check and prompt are at most 8000 bytes, purpose 300 characters.",
                ));
            }
            let every = args["every_minutes"]
                .as_i64()
                .unwrap_or(DEFAULT_EVERY_MINUTES);
            let Some(hours) = args["hours"].as_i64() else {
                return Err(Refusal::text("hours is required."));
            };
            if !(MIN_EVERY_MINUTES..=MAX_EVERY_MINUTES).contains(&every)
                || !(1..=MAX_HOURS).contains(&hours)
            {
                return Err(Refusal::text(format!(
                    "every_minutes must be {MIN_EVERY_MINUTES} to {MAX_EVERY_MINUTES} and \
                         hours 1 to {MAX_HOURS}."
                )));
            }
            let origin = match state.db.acquire().await {
                Ok(mut conn) => crate::db::delegations::origin(&mut conn, grant.turn_id).await,
                Err(err) => Err(err),
            };
            match origin {
                Ok(Origin::OwnerRequest) => {}
                Ok(_) => {
                    return Err(Refusal::text(
                        "NOT_ALLOWED: its check runs unattended for days, so only an owner \
                             request starts a watcher. Report what you would watch instead.",
                    ));
                }
                Err(err) => return failed(err),
            }
            let secrets = super::secret_names(args)?;
            // A retry answers with its watcher even if an entry was deleted
            // since; only a new watcher's entries must exist now.
            match watchers::retry_of(&state.db, grant.turn_id, &check, &purpose).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    super::resolve_secrets(state, &secrets).await?;
                }
                Err(err) => return failed(err),
            }
            let until = chrono::Utc::now() + chrono::Duration::hours(hours);
            let started = watchers::start(
                &state.db,
                NewWatcher {
                    bot_id: grant.bot_id,
                    turn_id: grant.turn_id,
                    purpose: &purpose,
                    check: &check,
                    prompt: &prompt,
                    secrets: &secrets,
                    every_minutes: every,
                    until,
                },
            )
            .await;
            if let Ok(Started::Created(_)) = &started {
                task_secrets::record_use(
                    &state.db,
                    &secrets,
                    grant.bot_id,
                    Some(grant.turn_id),
                    task_secrets::Use::Job,
                )
                .await;
            }
            match started {
                Ok(Started::Created(watcher) | Started::Existing(watcher)) => {
                    state.scheduler.wake();
                    let mut value = watcher_result(&watcher);
                    value["next"] = json!(if watcher.status.is_active() {
                        "Watching. End your turn: else.bot wakes you once when it ends. Tell \
                         the owner the deadline; nothing runs past it."
                    } else {
                        "This watcher already ended; see its outcome."
                    });
                    Ok(Done::json(fit(value)).logged(format!(
                        "watcher {} started: {}",
                        watcher.id,
                        slice(&purpose, 0, 200).1
                    )))
                }
                Ok(Started::Conflict(watcher)) => Err(Refusal::flagged(fit(json!({
                    "error": "CONFLICT: this request already started a watcher with this \
                        check and purpose but another prompt, interval or secrets. To watch \
                        differently, give it another purpose. No watcher was started.",
                    "watcher": watcher_result(&watcher),
                })))),
                Ok(Started::Stopped) => Err(Refusal::text(
                    "STOPPED: this turn was stopped; no watcher was started.",
                )),
                Ok(Started::TooMany(active)) => Err(Refusal::flagged(fit(json!({
                    "error": format!(
                        "WATCHER_LIMIT: {MAX_PER_BOT} watchers already run. Stop one with \
                         computer_watch_stop or wait for one to end. No watcher was started."
                    ),
                    "watchers": active.iter().map(watcher_result).collect::<Vec<_>>(),
                })))),
                Err(err) => failed(err),
            }
        }
        Op::List => {
            let listed = match state.db.acquire().await {
                Ok(mut conn) => watchers::list_active(&mut conn, grant.bot_id).await,
                Err(err) => Err(err),
            };
            match listed {
                Ok(active) => Ok(Done::json(fit(json!({
                    "watchers": active.iter().map(watcher_result).collect::<Vec<_>>(),
                })))),
                Err(err) => failed(err),
            }
        }
        Op::Stop => {
            let Some(id) = text(args, "watch_id").and_then(|s| s.parse::<Uuid>().ok()) else {
                return Err(Refusal::text("watch_id must be a watcher ID."));
            };
            match watchers::stop(&state.db, grant.bot_id, id, StopBy::Bot).await {
                Ok(Stopped::Stopped(watcher) | Stopped::NotActive(watcher)) => {
                    Ok(Done::json(fit(watcher_result(&watcher)))
                        .logged(format!("watcher {id} stopped")))
                }
                Ok(Stopped::NotFound) => Err(Refusal::text("NOT_FOUND: no such watcher.")),
                Err(err) => failed(err),
            }
        }
    }
}
