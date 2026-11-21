//! The delegation tools: hand a task to another bot (`delegate`), ask
//! several at once (`council`), and change or stop a running task
//! (`delegation_update`, `delegation_cancel`).

use crate::http::tool_result::{self, Done, Refusal, ToolResult};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use crate::grants::Grant;

/// The owner reads a task by this line; the task text stays folded.
const TITLE_DESCRIPTION: &str =
    "A short name for the task, for the owner, e.g. Triage overnight WhatsApp DMs. No IDs.";

pub(super) fn delegate_schema() -> Value {
    json!({
        "description": "Hand one subtask to another registered bot (the main bot included) \
            from your registry or a create_bot result, by its exact bot_id. It is for work, not \
            status reports. Returns status pending with a task_id at once. Then tell the owner \
            briefly and end this attempt; the other bot's reply arrives later in a \
            continuation. At most one per attempt.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target_bot_id": { "type": "string", "format": "uuid" },
                "task": { "type": "string", "minLength": 1, "maxLength": 4000,
                          "description": "What the other bot should do, self-contained." },
                "title": { "type": "string", "minLength": 1,
                           "maxLength": crate::db::delegations::MAX_TITLE_CHARS,
                           "description": TITLE_DESCRIPTION },
                "files": { "type": "array", "maxItems": crate::app::messages::MAX_MESSAGE_FILES,
                           "items": { "type": "string", "minLength": 1, "maxLength": 1024 },
                           "description": "Project paths of files the other bot needs, up to 10 MiB \
                               each. It gets exact copies, never your project. Its replies can \
                               attach files back the same way you attach them for the owner." },
                "owner_answer": { "type": "string", "format": "uuid",
                                  "description": "ID of an answered choice card, yours or from a task you delegated, \
                                      that decides this task. The other bot sees the answer as the owner's words. \
                                      It does not use the request's delegate budget." },
            },
            "required": ["target_bot_id", "task", "title"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) fn council_schema() -> Value {
    json!({
        "description": "Ask 1 to 32 distinct bots for reports on the same self-contained task. Use list_bots with category_id to find every member of a category. Accepted atomically; runs asynchronously and returns all outcomes in a continuation. After pending, end this attempt immediately. Delegated tasks cannot create councils. No memory or permissions are shared.",
        "inputSchema": {"type": "object", "properties": {
            "target_bot_ids": {"type": "array", "items": {"type": "string", "format": "uuid"}, "minItems": 1, "maxItems": crate::db::delegations::MAX_COUNCIL_BOTS, "uniqueItems": true},
            "task": {"type": "string", "minLength": 1, "maxLength": 4000},
            "title": {"type": "string", "minLength": 1, "maxLength": crate::db::delegations::MAX_TITLE_CHARS, "description": TITLE_DESCRIPTION}
        }, "required": ["target_bot_ids", "task", "title"], "additionalProperties": false},
        "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
    })
}

pub(super) fn update_schema() -> Value {
    json!({
        "description": "Change a task you delegated that is still in progress, by its task_id. \
            The other bot stops its current attempt and continues the same task with your \
            message and its earlier work; the result still arrives in a continuation. Returns \
            the task's state. An ended task is not changed; its outcome arrives in a continuation.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "format": "uuid" },
                "message": { "type": "string", "minLength": 1, "maxLength": 4000,
                             "description": "The change, written to the other bot as a follow-up to its task." },
            },
            "required": ["task_id", "message"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) fn cancel_schema() -> Value {
    json!({
        "description": "Stop a task you delegated that is still in progress, by its task_id. \
            Returns its state: stopping until the other bot's attempt ends; the outcome, \
            normally cancelled with any partial reply, arrives in a continuation. An ended task is not changed.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "task_id": { "type": "string", "format": "uuid" },
            },
            "required": ["task_id"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": true,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) async fn delegate(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    use crate::db::delegations::{self, DelegateError};
    let Some(target) = args["target_bot_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
    else {
        return Err(Refusal::text(
            "target_bot_id must be a bot ID from your registry.",
        ));
    };
    let Some(task) = args["task"].as_str() else {
        return Err(Refusal::text("task is required."));
    };
    let owner_answer = match &args["owner_answer"] {
        Value::Null => None,
        value => match value.as_str().and_then(|s| s.parse::<Uuid>().ok()) {
            Some(id) => Some(id),
            None => {
                return Err(Refusal::text(
                    "owner_answer must be the ID of an answered choice card.",
                ));
            }
        },
    };
    let paths: Vec<String> = match &args["files"] {
        Value::Null => Vec::new(),
        Value::Array(paths) if paths.len() <= crate::app::messages::MAX_MESSAGE_FILES => {
            match paths
                .iter()
                .map(|p| p.as_str().map(str::to_owned))
                .collect()
            {
                Some(paths) => paths,
                None => return Err(Refusal::text("files must be project paths.")),
            }
        }
        _ => {
            let max = crate::app::messages::MAX_MESSAGE_FILES;
            return Err(Refusal::text(format!("files takes at most {max} paths.")));
        }
    };
    if !paths.is_empty() && from_email(state, grant).await {
        return Err(Refusal::text(
            "files are unavailable here: this turn cannot read the project. Put what the \
                 other bot needs in task.",
        )
        .logged("rejected NO_PROJECT"));
    }
    let files = if paths.is_empty() {
        Vec::new()
    } else {
        let Some(project) = state
            .registry
            .all()
            .iter()
            .find_map(|provider| provider.project_dir(grant.bot_id))
        else {
            return Err(Refusal::text("Your project directory is unavailable."));
        };
        let read = tokio::task::spawn_blocking(move || {
            paths
                .iter()
                .map(|path| {
                    crate::providers::output_files::read_linked(&project, path)
                        .map_err(|message| format!("FILE_UNAVAILABLE: {path}: {message}"))
                })
                .collect::<Result<Vec<_>, _>>()
        })
        .await;
        match read {
            Ok(Ok(files)) => files,
            Ok(Err(message)) => {
                return Err(Refusal::text(message).logged("rejected FILE_UNAVAILABLE"));
            }
            Err(_) => return Err(Refusal::text("Reading the files failed.")),
        }
    };
    let sent: Vec<Value> = files
        .iter()
        .map(|output| {
            use sha2::Digest;
            json!({
                "name": output.file.name,
                "media_type": output.file.media_type,
                "size": output.file.size,
                "sha256": hex::encode(sha2::Sha256::digest(&output.data)),
            })
        })
        .collect();
    let title = args["title"].as_str();
    match delegations::create(
        &state.db,
        grant.turn_id,
        target,
        task,
        title,
        &files,
        owner_answer,
    )
    .await
    {
        Ok(accepted) if accepted.status.is_ended() => {
            let log = format!(
                "already {} {}",
                accepted.status.as_str(),
                accepted.delegation_id
            );
            let result = json!({
                "status": accepted.status,
                "task_id": accepted.delegation_id,
                "target": { "bot_id": accepted.target_bot_id, "name": accepted.target_name },
                "outcome": accepted.outcome,
                "next": "This task already ended; use its recorded outcome and do not resubmit it.",
            });
            Ok(Done::json(result).logged(log))
        }
        Ok(accepted) => {
            state.grants.park(grant.turn_id).await;
            state.scheduler.wake();
            let log = format!(
                "pending {} to {} ({})",
                accepted.delegation_id, accepted.target_name, accepted.target_bot_id
            );
            let result = json!({
                "status": "pending",
                "parked": true,
                "task_id": accepted.delegation_id,
                "target": { "bot_id": accepted.target_bot_id, "name": accepted.target_name },
                "files": sent,
                "next": "Tell the owner it is in progress and end this attempt now. Do not wait, \
                         poll, or call more tools; the result arrives in a continuation. Each \
                         delegate call queues separate work: to change or stop this task later, \
                         use delegation_update or delegation_cancel with its task_id.",
            });
            Ok(Done::json(result).logged(log))
        }
        Err(err) => {
            let code = match err {
                DelegateError::NotAllowed => "NOT_ALLOWED",
                DelegateError::SelfTarget => "SELF_DELEGATION",
                DelegateError::UpstreamTarget => "UPSTREAM_TARGET",
                DelegateError::BotUnavailable => "BOT_UNAVAILABLE",
                DelegateError::BudgetExhausted
                | DelegateError::AnswerBudgetExhausted
                | DelegateError::CouncilBudgetExhausted => "BUDGET_EXHAUSTED",
                DelegateError::AttemptParked => "ATTEMPT_PARKED",
                DelegateError::InvalidTask => "INVALID_TASK",
                DelegateError::InvalidCouncil => "INVALID_COUNCIL",
                DelegateError::AnswerUnavailable => "ANSWER_UNAVAILABLE",
                DelegateError::NotRunning => "NOT_RUNNING",
                DelegateError::Db => "UNAVAILABLE",
            };
            let next = if err == DelegateError::BudgetExhausted {
                " If the owner chose this hand-off on a card, yours or one a task's result \
                 lists, call delegate again with owner_answer set to that card's ID; otherwise \
                 give the work back in your reply."
            } else {
                ""
            };
            let log = format!("rejected {code}");
            Err(Refusal::text(format!("{code}: {err}.{next}")).logged(log))
        }
    }
}

/// Whether `grant`'s turn traces back to incoming email, which cannot read
/// the project. Fails closed.
async fn from_email(state: &AppState, grant: &Grant) -> bool {
    let Ok(mut conn) = state.db.acquire().await else {
        return true;
    };
    !crate::db::delegations::origin(&mut conn, grant.turn_id)
        .await
        .is_ok_and(|origin| origin != crate::app::providers::Origin::ConnectorEvent)
}

fn task_json(task: &crate::db::delegations::Task) -> Value {
    use crate::db::delegations::TaskState;
    let mut value = json!({
        "task_id": task.id,
        "status": task.state.as_str(),
        "target": { "bot_id": task.target_bot_id, "name": task.target_name },
    });
    if let TaskState::Ended(_) = task.state {
        value["next"] = json!(
            "This task ended. Its outcome is in your context or arrives in your next continuation."
        );
    }
    value
}

fn task_id(args: &Value) -> Option<Uuid> {
    args["task_id"].as_str().and_then(|s| s.parse().ok())
}

fn control_error(err: crate::db::delegations::ControlError) -> ToolResult {
    use crate::db::delegations::ControlError;
    let (code, task, next) = match &err {
        ControlError::NotFound => (
            "NOT_FOUND",
            None,
            "Use a task_id from your tasks in progress.",
        ),
        ControlError::InvalidMessage => ("INVALID_MESSAGE", None, ""),
        ControlError::NotRunning => ("NOT_RUNNING", None, ""),
        ControlError::BotUnavailable => ("BOT_UNAVAILABLE", None, ""),
        ControlError::Ended(task) => (
            "ENDED",
            Some(task),
            "Nothing was changed. For more work, delegate a new task.",
        ),
        ControlError::TakenOver(task) => (
            "TAKEN_OVER",
            Some(task),
            "The owner is steering this task in that bot's chat, and its result becomes this \
             task's. Leave it to them.",
        ),
        ControlError::Db => ("UNAVAILABLE", None, ""),
    };
    let mut result = json!({ "error": format!("{code}: {err}") });
    if let Some(task) = task {
        result["task"] = task_json(task);
    }
    if !next.is_empty() {
        result["next"] = json!(next);
    }
    Err(Refusal::text(result.to_string()).logged(format!("rejected {code}")))
}

pub(super) async fn update(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let Some(task) = task_id(args) else {
        return Err(Refusal::text("task_id must be a task ID."));
    };
    let Some(message) = args["message"].as_str() else {
        return Err(Refusal::text("message is required."));
    };
    match crate::db::delegations::update(&state.db, grant.turn_id, task, message).await {
        Ok(updated) => {
            if updated.created {
                state.scheduler.wake();
            }
            let mut result = task_json(&updated.task);
            result["next"] = json!(if updated.created {
                "The task continues with your change; its result arrives in a continuation."
            } else {
                "This update was already applied."
            });
            let log = format!("updated {task} ({})", updated.task.state.as_str());
            Ok(Done::json(result).logged(log))
        }
        Err(err) => control_error(err),
    }
}

pub(super) async fn cancel(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    use crate::db::delegations::TaskState;
    let Some(task) = task_id(args) else {
        return Err(Refusal::text("task_id must be a task ID."));
    };
    match crate::db::delegations::cancel(&state.db, grant.turn_id, task).await {
        Ok(stopped) => {
            state.scheduler.wake();
            let mut result = task_json(&stopped);
            if stopped.state == TaskState::Stopping {
                result["next"] = json!(
                    "The stop is committed. The task ends as cancelled; any partial reply \
                     arrives in a continuation."
                );
            }
            let log = format!("cancel {task} ({})", stopped.state.as_str());
            Ok(Done::json(result).logged(log))
        }
        Err(err) => control_error(err),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilArgs {
    target_bot_ids: Vec<Uuid>,
    task: String,
    #[serde(default)]
    title: Option<String>,
}

pub(super) async fn council(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let parsed: CouncilArgs = tool_result::args(args, || {
        Refusal::text("Pass target_bot_ids from list_bots and a shared task.")
    })?;
    match crate::db::delegations::council(
        &state.db,
        grant.turn_id,
        &parsed.target_bot_ids,
        &parsed.task,
        parsed.title.as_deref(),
    )
    .await
    {
        Ok(accepted) => {
            let pending = accepted.iter().any(|a| !a.status.is_ended());
            if pending {
                state.grants.park(grant.turn_id).await;
                state.scheduler.wake();
            }
            let tasks: Vec<_> = accepted
                .iter()
                .map(|a| {
                    json!({"task_id": a.delegation_id,
                "target": {"bot_id": a.target_bot_id, "name": a.target_name},
                "status": a.status, "outcome": a.outcome})
                })
                .collect();
            Ok(Done::json(json!({"status": if pending {"pending"} else {"settled"},
                "parked": pending, "tasks": tasks,
                "next": if pending {"End this attempt. All reports arrive in a continuation; combine them and preserve failures and disagreements."}
                    else {"Use the recorded reports. Do not resubmit completed work."}})).logged(format!("council reports from {} bots", tasks.len())))
        }
        Err(err) => Err(Refusal::text(format!("COUNCIL_REFUSED: {err}")).logged("council refused")),
    }
}
