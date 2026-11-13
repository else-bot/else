//! The delegation tools: hand a task to another bot (`delegate`), ask
//! several at once (`council`), and change or stop a running task
//! (`delegation_update`, `delegation_cancel`).

use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::mcp::{Outcome, tool_error, tool_result};
use crate::grants::Grant;

pub(super) async fn delegate(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    use crate::db::delegations::{self, DelegateError};
    let Some(target) = args["target_bot_id"]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
    else {
        return (
            tool_error("target_bot_id must be a bot ID from your registry."),
            None,
        );
    };
    let Some(task) = args["task"].as_str() else {
        return (tool_error("task is required."), None);
    };
    let owner_answer = match &args["owner_answer"] {
        Value::Null => None,
        value => match value.as_str().and_then(|s| s.parse::<Uuid>().ok()) {
            Some(id) => Some(id),
            None => {
                return (
                    tool_error("owner_answer must be the ID of an answered choice card."),
                    None,
                );
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
                None => return (tool_error("files must be project paths."), None),
            }
        }
        _ => {
            let max = crate::app::messages::MAX_MESSAGE_FILES;
            return (
                tool_error(&format!("files takes at most {max} paths.")),
                None,
            );
        }
    };
    let files = if paths.is_empty() {
        Vec::new()
    } else {
        let Some(project) = state
            .scheduler
            .registry()
            .all()
            .iter()
            .find_map(|provider| provider.project_dir(grant.bot_id))
        else {
            return (tool_error("Your project directory is unavailable."), None);
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
                return (
                    tool_error(&message),
                    Some("rejected FILE_UNAVAILABLE".into()),
                );
            }
            Err(_) => return (tool_error("Reading the files failed."), None),
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
    match delegations::create(&state.db, grant.turn_id, target, task, &files, owner_answer).await {
        Ok(accepted) if !matches!(accepted.status.as_str(), "queued" | "running") => {
            let log = format!("already {} {}", accepted.status, accepted.delegation_id);
            let result = json!({
                "status": accepted.status,
                "task_id": accepted.delegation_id,
                "target": { "bot_id": accepted.target_bot_id, "name": accepted.target_name },
                "outcome": accepted.outcome,
                "next": "This task already ended; use its recorded outcome and do not resubmit it.",
            });
            (tool_result(result), Some(log))
        }
        Ok(accepted) => {
            state.scheduler.grants().park(grant.turn_id).await;
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
            (tool_result(result), Some(log))
        }
        Err(err) => {
            let code = match err {
                DelegateError::NotAllowed => "NOT_ALLOWED",
                DelegateError::SelfTarget => "SELF_DELEGATION",
                DelegateError::BotUnavailable => "BOT_UNAVAILABLE",
                DelegateError::BudgetExhausted | DelegateError::CouncilBudgetExhausted => {
                    "BUDGET_EXHAUSTED"
                }
                DelegateError::AttemptParked => "ATTEMPT_PARKED",
                DelegateError::InvalidTask => "INVALID_TASK",
                DelegateError::InvalidCouncil => "INVALID_COUNCIL",
                DelegateError::AnswerUnavailable => "ANSWER_UNAVAILABLE",
                DelegateError::NotRunning => "NOT_RUNNING",
                DelegateError::Db => "UNAVAILABLE",
            };
            let log = format!("rejected {code}");
            (tool_error(&format!("{code}: {err}")), Some(log))
        }
    }
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

fn control_error(err: crate::db::delegations::ControlError) -> Outcome {
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
    (
        tool_error(&result.to_string()),
        Some(format!("rejected {code}")),
    )
}

pub(super) async fn update(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Some(task) = task_id(args) else {
        return (tool_error("task_id must be a task ID."), None);
    };
    let Some(message) = args["message"].as_str() else {
        return (tool_error("message is required."), None);
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
            (tool_result(result), Some(log))
        }
        Err(err) => control_error(err),
    }
}

pub(super) async fn cancel(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    use crate::db::delegations::TaskState;
    let Some(task) = task_id(args) else {
        return (tool_error("task_id must be a task ID."), None);
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
            (tool_result(result), Some(log))
        }
        Err(err) => control_error(err),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilArgs {
    target_bot_ids: Vec<Uuid>,
    task: String,
}

pub(super) async fn council(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Ok(parsed) = serde_json::from_value::<CouncilArgs>(args.clone()) else {
        return (
            tool_error("Pass target_bot_ids from list_bots and a shared task."),
            None,
        );
    };
    match crate::db::delegations::council(
        &state.db,
        grant.turn_id,
        &parsed.target_bot_ids,
        &parsed.task,
    )
    .await
    {
        Ok(accepted) => {
            let pending = accepted
                .iter()
                .any(|a| matches!(a.status.as_str(), "queued" | "running"));
            if pending {
                state.scheduler.grants().park(grant.turn_id).await;
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
            (
                tool_result(json!({"status": if pending {"pending"} else {"settled"},
                "parked": pending, "tasks": tasks,
                "next": if pending {"End this attempt. All reports arrive in a continuation; combine them and preserve failures and disagreements."}
                    else {"Use the recorded reports. Do not resubmit completed work."}})),
                Some(format!("council reports from {} bots", tasks.len())),
            )
        }
        Err(err) => (
            tool_error(&format!("COUNCIL_REFUSED: {err}")),
            Some("council refused".into()),
        ),
    }
}
