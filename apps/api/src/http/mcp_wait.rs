//! `mcp_wait`: park a request on a long-running job in an installed MCP,
//! and the watcher that repeats its status call until the job stops.

use std::sync::Arc;
use std::time::Duration;

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use super::AppState;
use super::guest_tools::approval::approve_card;
use super::mcp::{Outcome, tool_error, tool_result};
use crate::app::questions::Kind;
use crate::db::mcp_waits::{self, Check, CreateError, Due, Spec};
use crate::grants::Grant;
use crate::mcp::{Error, Library, PackageId};

pub const NAME: &str = "mcp_wait";

const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const CONCURRENT_CHECKS: usize = 4;

pub fn schema() -> Value {
    json!({
        "name": NAME,
        "description": "Wait for a long-running job you started in an installed MCP, such as a \
            delegated task or thread run that returned an id to check later, instead of ending \
            the request or polling yourself. Name the MCP's read-only status tool and its \
            arguments, a JSON pointer to the status field in its result, and the values that \
            mean the job still runs. The status is read from the tool's structuredContent, or \
            else from the JSON in its first text content block. The status call runs once now: \
            if the job already stopped, the result returns at once and nothing waits. Otherwise \
            this returns parked: end this attempt. else.bot repeats the status call without \
            you and continues this request once the status leaves running_values, the checks \
            fail three times in a row, or the timeout passes. The continuation gets the last \
            status result. The status tool must be one its server declares read-only, or \
            non-destructive and idempotent; any other tool is refused. One wait per attempt; \
            delegated tasks cannot wait.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": { "type": "string", "pattern": "^[a-z][a-z0-9-]{0,63}$" },
                "tool": { "type": "string", "minLength": 1, "maxLength": 200,
                          "description": "The MCP's read-only status tool, such as task_status." },
                "arguments": { "type": "object",
                               "description": "The status tool's arguments, such as the job id." },
                "status_pointer": { "type": "string", "maxLength": 200,
                                    "description": "JSON pointer to the status in the result, such as /workState." },
                "running_values": { "type": "array", "minItems": 1, "maxItems": 16,
                                    "description": "Status values that mean the job still runs, such as [\"working\"]." },
                "effect": { "type": "string", "enum": ["local", "destructive", "external"],
                            "description": "What the first status call does, as for mcp_request: local for \
                                a read or routine reversible work; destructive or external asks the \
                                owner to approve this exact call first and returns parked." },
                "purpose": { "type": "string", "minLength": 1, "maxLength": 300,
                             "description": "What the job does and what to do with its result." },
                "interval_seconds": { "type": "integer",
                                      "minimum": mcp_waits::MIN_INTERVAL_SECONDS,
                                      "maximum": mcp_waits::MAX_INTERVAL_SECONDS,
                                      "description": "Seconds between status calls; default 60." },
                "timeout_minutes": { "type": "integer", "minimum": 1,
                                     "maximum": mcp_waits::MAX_TIMEOUT_MINUTES,
                                     "description": "When to stop waiting; default 240." },
            },
            "required": ["id", "tool", "status_pointer", "running_values", "effect", "purpose"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false },
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    id: PackageId,
    tool: String,
    arguments: Option<Value>,
    status_pointer: String,
    running_values: Vec<Value>,
    effect: Effect,
    purpose: String,
    interval_seconds: Option<u32>,
    timeout_minutes: Option<u32>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Effect {
    Local,
    Destructive,
    External,
}

/// The spec, and the approval its first call needs, if any.
fn parse(args: &Value) -> Result<(Spec, Option<Kind>), &'static str> {
    let args: Args = serde_json::from_value(args.clone())
        .map_err(|_| "Use id, tool, status_pointer, running_values, effect and purpose, with optional arguments, interval_seconds and timeout_minutes.")?;
    let approval = match args.effect {
        Effect::Local => None,
        Effect::Destructive => Some(Kind::Destructive),
        Effect::External => Some(Kind::External),
    };
    let text = |s: &str, max: usize| !s.trim().is_empty() && s.len() <= max && !s.contains('\0');
    if !text(&args.tool, 200) {
        return Err("tool is the status tool's name.");
    }
    if !text(&args.purpose, 300) {
        return Err("purpose is 1 to 300 bytes of text.");
    }
    if !(args.status_pointer.is_empty() || args.status_pointer.starts_with('/'))
        || args.status_pointer.len() > 200
    {
        return Err("status_pointer is a JSON pointer such as /status.");
    }
    if !(1..=16).contains(&args.running_values.len()) {
        return Err("running_values lists 1 to 16 status values.");
    }
    let arguments = args.arguments.unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err("arguments is an object.");
    }
    let interval_seconds = args.interval_seconds.unwrap_or(60);
    if !(mcp_waits::MIN_INTERVAL_SECONDS..=mcp_waits::MAX_INTERVAL_SECONDS)
        .contains(&interval_seconds)
    {
        return Err("interval_seconds is 15 to 3600.");
    }
    let timeout_minutes = args.timeout_minutes.unwrap_or(240);
    if !(1..=mcp_waits::MAX_TIMEOUT_MINUTES).contains(&timeout_minutes) {
        return Err("timeout_minutes is 1 to 1440.");
    }
    let spec = Spec {
        mcp_id: args.id.into(),
        tool: args.tool,
        arguments,
        status_pointer: args.status_pointer,
        running_values: args.running_values,
        purpose: args.purpose.trim().to_owned(),
        interval_seconds,
        timeout_minutes,
    };
    Ok((spec, approval))
}

/// The status a tool result reports at `pointer`: from structuredContent,
/// or else from the JSON in its first text block that parses.
fn status_at(result: &Value, pointer: &str) -> Option<Value> {
    if let Some(found) = result
        .get("structuredContent")
        .and_then(|d| d.pointer(pointer))
    {
        return Some(found.clone());
    }
    result["content"].as_array()?.iter().find_map(|block| {
        let text = block["text"].as_str().filter(|_| block["type"] == "text")?;
        serde_json::from_str::<Value>(text)
            .ok()?
            .pointer(pointer)
            .cloned()
    })
}

fn classify(result: &Value, pointer: &str, running: &[Value]) -> Check {
    let text = result.to_string();
    if result["isError"] == true {
        return Check::Failed {
            message: text,
            permanent: false,
        };
    }
    match status_at(result, pointer) {
        Some(status) if running.contains(&status) => Check::Running(text),
        _ => Check::Finished(text),
    }
}

/// One status call. `None` while the MCP library is paused for
/// maintenance, which does not count against the wait.
async fn check(
    library: &Library,
    mcp_id: &str,
    tool: &str,
    arguments: &Value,
    pointer: &str,
    running: &[Value],
) -> Option<Check> {
    let id = match PackageId::try_from(mcp_id.to_owned()) {
        Ok(id) => id,
        Err(message) => {
            return Some(Check::Failed {
                message: message.into(),
                permanent: true,
            });
        }
    };
    let params = json!({ "name": tool, "arguments": arguments });
    match library.request(&id, "tools/call".into(), params).await {
        Ok(result) => Some(classify(&result, pointer, running)),
        Err(Error::Paused) => None,
        Err(error) => Some(Check::Failed {
            message: error.to_string(),
            permanent: !matches!(error, Error::Connection | Error::Timeout | Error::Io(_)),
        }),
    }
}

const MAX_TOOL_PAGES: usize = 20;

/// Whether repeating `tool` adds no effect by the server's own explicit
/// annotations: read-only, or non-destructive and idempotent. Missing
/// annotations default to unsafe in MCP, and so do they here.
fn repeatable(tool: &Value) -> bool {
    let hints = &tool["annotations"];
    hints["readOnlyHint"] == true
        || (hints["destructiveHint"] == false && hints["idempotentHint"] == true)
}

/// Find `tool` in the server's paginated tool list.
async fn listed_tool(
    library: &Library,
    id: &PackageId,
    tool: &str,
) -> Result<Option<Value>, Error> {
    let mut params = json!({});
    for _ in 0..MAX_TOOL_PAGES {
        let page = library.request(id, "tools/list".into(), params).await?;
        if let Some(found) = page["tools"]
            .as_array()
            .and_then(|tools| tools.iter().find(|t| t["name"] == tool))
        {
            return Ok(Some(found.clone()));
        }
        match page["nextCursor"].as_str() {
            Some(cursor) if !cursor.is_empty() => params = json!({ "cursor": cursor }),
            _ => break,
        }
    }
    Ok(None)
}

/// Why a status tool may not be called now. `Paused` passes with the
/// maintenance window; `permanent` when asking again cannot help.
enum Refusal {
    Paused,
    Refused { message: String, permanent: bool },
}

/// else.bot repeats the status call without the bot, so only a tool whose
/// server currently declares repeating it safe may be called. Checked when
/// the wait starts and before every repeat, since a package can be replaced
/// or its server upgraded while a wait is pending.
async fn qualify(library: &Library, mcp_id: &str, tool: &str) -> Result<(), Refusal> {
    let refuse = |message: String, permanent: bool| Refusal::Refused { message, permanent };
    let id = PackageId::try_from(mcp_id.to_owned()).map_err(|m| refuse(m.into(), true))?;
    match listed_tool(library, &id, tool).await {
        Ok(Some(found)) if repeatable(&found) => Ok(()),
        Ok(Some(_)) => Err(refuse(
            format!(
                "NOT_REPEATABLE: {mcp_id} does not declare {tool} read-only, or non-destructive \
                 and idempotent, so else.bot will not repeat it. Use that MCP's documented way \
                 to learn when the job ends."
            ),
            true,
        )),
        Ok(None) => Err(refuse(format!("{mcp_id} has no tool named {tool}."), true)),
        Err(Error::Paused) => Err(Refusal::Paused),
        Err(error) => Err(refuse(
            format!("Listing the MCP's tools failed: {error}"),
            !matches!(error, Error::Connection | Error::Timeout | Error::Io(_)),
        )),
    }
}

pub async fn call(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let (spec, approval) = match parse(args) {
        Ok(parsed) => parsed,
        Err(message) => return (tool_error(message), None),
    };
    let Some(library) = state.mcps.get() else {
        return (tool_error("The shared MCP library is unavailable."), None);
    };
    let log = format!("{} {} on {}", NAME, spec.tool, spec.mcp_id);
    let attempt = match mcp_waits::caller_attempt(&state.db, grant.turn_id).await {
        Ok(attempt) => attempt,
        Err(error) => return refused(error, &log),
    };
    let note = match approval {
        None => String::new(),
        Some(kind) => {
            let title = format!("Check {} on {} until it finishes", spec.tool, spec.mcp_id);
            let context = format!(
                "{}\n\nMCP: {} / {} ({}). else.bot calls it now and then every {} seconds \
                 for up to {} minutes with these arguments:\n{}",
                spec.purpose,
                spec.mcp_id,
                spec.tool,
                kind.as_str(),
                spec.interval_seconds,
                spec.timeout_minutes,
                spec.arguments,
            );
            match approve_card(state, grant, NAME, args, kind, title, context).await {
                Ok(note) => note.map(|n| format!("; {n}")).unwrap_or_default(),
                Err(outcome) => return outcome,
            }
        }
    };
    match qualify(library, &spec.mcp_id, &spec.tool).await {
        Ok(()) => {}
        Err(Refusal::Paused) => return (tool_error(&Error::Paused.to_string()), Some(log)),
        Err(Refusal::Refused { message, .. }) => {
            return (
                tool_error(&format!("{message} Nothing waits.")),
                Some(format!("{log} refused{note}")),
            );
        }
    }
    let first = check(
        library,
        &spec.mcp_id,
        &spec.tool,
        &spec.arguments,
        &spec.status_pointer,
        &spec.running_values,
    )
    .await;
    let first = match first {
        None => return (tool_error(&Error::Paused.to_string()), Some(log)),
        Some(Check::Failed { message, .. }) => {
            return (
                tool_error(&format!(
                    "The status call failed, so nothing waits: {}",
                    mcp_waits::bounded(&message)
                )),
                Some(format!("{log} failed{note}")),
            );
        }
        Some(Check::Finished(result)) => {
            let result = serde_json::from_str(&result).unwrap_or(Value::String(result));
            return (
                tool_result(json!({
                    "status": "finished",
                    "result": result,
                    "next": "The job is no longer running; nothing waits. Continue with this result.",
                })),
                Some(format!("{log} already finished{note}")),
            );
        }
        Some(Check::Running(result)) => result,
    };
    match mcp_waits::create(&state.db, grant.turn_id, attempt, &spec, &first).await {
        Ok(wait_id) => {
            state.scheduler.grants().park(grant.turn_id).await;
            (
                tool_result(json!({
                    "status": "waiting",
                    "parked": true,
                    "wait_id": wait_id,
                    "next": "Tell the owner briefly what you are waiting for and end this attempt \
                             now. Do not poll or call more tools; else.bot checks the job and \
                             continues this request with its result.",
                })),
                Some(format!("{log} waiting {wait_id}{note}")),
            )
        }
        Err(error) => refused(error, &log),
    }
}

fn refused(error: CreateError, log: &str) -> Outcome {
    let code = match error {
        CreateError::NotAllowed => "NOT_ALLOWED",
        CreateError::AttemptParked => "ATTEMPT_PARKED",
        CreateError::NotRunning => "NOT_RUNNING",
        CreateError::Db => "UNAVAILABLE",
    };
    (
        tool_error(&format!("{code}: {error}")),
        Some(format!("{log} refused")),
    )
}

async fn check_and_record(state: Arc<AppState>, wait: Due) {
    let Some(library) = state.mcps.get() else {
        return;
    };
    let result = match qualify(library, &wait.mcp_id, &wait.tool).await {
        Ok(()) => {
            let checked = check(
                library,
                &wait.mcp_id,
                &wait.tool,
                &wait.arguments,
                &wait.status_pointer,
                &wait.running_values,
            )
            .await;
            match checked {
                Some(result) => result,
                None => return,
            }
        }
        Err(Refusal::Paused) => return,
        Err(Refusal::Refused { message, permanent }) => Check::Failed { message, permanent },
    };
    match mcp_waits::record(&state.db, wait.id, &result).await {
        Ok(true) => state.scheduler.wake(),
        Ok(false) => {}
        Err(error) => {
            tracing::warn!(wait = %wait.id, %error, "recording an MCP wait check failed");
        }
    }
}

/// Check due waits until `stop`. Each package has at most one check in
/// flight, since its actor serves requests one at a time, so a slow server
/// holds one slot. Waits are durable: a restart picks up where the last
/// check left off, and stopping abandons checks in flight.
pub async fn run(state: Arc<AppState>, stop: CancellationToken) {
    let mut checks = JoinSet::new();
    let mut busy: HashMap<tokio::task::Id, String> = HashMap::new();
    let mut scan = tokio::time::interval(SCAN_INTERVAL);
    scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            Some(done) = checks.join_next_with_id() => {
                let id = match done {
                    Ok((id, ())) => id,
                    Err(error) => error.id(),
                };
                busy.remove(&id);
                continue;
            }
            _ = scan.tick() => {}
        }
        match mcp_waits::expire(&state.db).await {
            Ok(0) => {}
            Ok(_) => state.scheduler.wake(),
            Err(error) => tracing::warn!(%error, "expiring MCP waits failed"),
        }
        let free = CONCURRENT_CHECKS.saturating_sub(checks.len());
        if free == 0 || state.mcps.get().is_none() {
            continue;
        }
        let busy_packages: Vec<String> = busy.values().cloned().collect();
        let due = match mcp_waits::due(&state.db, &busy_packages, free).await {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!(%error, "listing due MCP waits failed");
                continue;
            }
        };
        for wait in due {
            let mcp_id = wait.mcp_id.clone();
            let task = checks.spawn(check_and_record(state.clone(), wait));
            busy.insert(task.id(), mcp_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_comes_from_structured_content_or_the_first_json_text_block() {
        let running = [json!("working"), json!("waiting_for_children")];
        let structured = json!({ "structuredContent": { "workState": "working" },
                                 "content": [{ "type": "text", "text": "{\"workState\":\"result_available\"}" }] });
        assert!(matches!(
            classify(&structured, "/workState", &running),
            Check::Running(_)
        ));
        let text = json!({ "content": [{ "type": "text", "text": "Task state" },
                                       { "type": "text", "text": "{\"workState\":\"result_available\"}" }] });
        assert!(matches!(
            classify(&text, "/workState", &running),
            Check::Finished(_)
        ));
        let missing = json!({ "content": [{ "type": "text", "text": "no json" }] });
        assert!(matches!(
            classify(&missing, "/workState", &running),
            Check::Finished(_)
        ));
        let error = json!({ "isError": true, "content": [] });
        assert!(matches!(
            classify(&error, "/workState", &running),
            Check::Failed {
                permanent: false,
                ..
            }
        ));
    }

    #[test]
    fn only_tools_declared_safe_to_repeat_are_waited_on() {
        let t3_task_status = json!({ "annotations": { "readOnlyHint": false,
            "destructiveHint": false, "idempotentHint": true } });
        assert!(repeatable(&t3_task_status));
        assert!(repeatable(
            &json!({ "annotations": { "readOnlyHint": true } })
        ));
        for unsafe_tool in [
            json!({}),
            json!({ "annotations": { "idempotentHint": true } }),
            json!({ "annotations": { "destructiveHint": false } }),
            json!({ "annotations": { "destructiveHint": true, "idempotentHint": true } }),
        ] {
            assert!(!repeatable(&unsafe_tool), "{unsafe_tool}");
        }
    }

    #[test]
    fn arguments_are_bounded() {
        let base = json!({ "id": "t3-code", "tool": "task_status", "arguments": { "taskId": "a" },
                           "status_pointer": "/workState", "running_values": ["working"],
                           "effect": "local", "purpose": "Review the PR when the task ends." });
        let (spec, approval) = parse(&base).unwrap();
        assert_eq!((spec.interval_seconds, spec.timeout_minutes), (60, 240));
        assert!(approval.is_none());
        for (field, value) in [
            ("interval_seconds", json!(5)),
            ("timeout_minutes", json!(0)),
            ("status_pointer", json!("workState")),
            ("running_values", json!([])),
            ("arguments", json!("a")),
            ("purpose", json!(" ")),
            ("extra", json!(true)),
            ("effect", json!("none")),
        ] {
            let mut args = base.clone();
            args[field] = value;
            assert!(parse(&args).is_err(), "{field}");
        }
    }
}
