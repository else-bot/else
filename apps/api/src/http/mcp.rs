//! else.bot tools for provider sessions, over MCP streamable HTTP.
//!
//! A provider CLI connects to `POST /tools/v1/mcp` with its turn's task
//! grant as the bearer token; identity comes only from that grant. The
//! endpoint answers JSON-RPC with plain JSON responses (no server stream).
//! Memory results carry provenance outside the generated text, and recalled
//! text is data, never instructions. Other tools act on the bot's computer
//! or create work and bots for the owner. Each call is logged to
//! the calling bot's memory as tool activity (arguments and which entries
//! were returned, not copies of their text).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::ApiError;
use super::tools::check;
use crate::app::memory_tree::NodeId;
use crate::db::memory::{self, EntryRow, Kind, NewEntry, RESPONSE_BUDGET};
use crate::db::memory_tree;
use crate::grants::{Grant, Tool};

use crate::app::tools::SERVER_NAME;
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const HIT_BODY: usize = 600;
const MAX_QUERY: usize = 200;
const MAX_READ: i64 = 20;

pub async fn endpoint(
    State(state): State<Arc<AppState>>,
    Extension(grant): Extension<Grant>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Ok(Json(message)) = body else {
        return rpc_error(Value::Null, -32700, "Parse error");
    };
    let valid_version = message["jsonrpc"] == "2.0";
    let method = message["method"].as_str();
    let params_ok = matches!(message.get("params"), None | Some(Value::Object(_)));
    let id = match message.get("id") {
        Some(id @ (Value::String(_) | Value::Number(_))) => Some(id.clone()),
        None => None,
        Some(_) => return rpc_error(Value::Null, -32600, "Invalid request"),
    };
    let (Some(method), true, true) = (method, valid_version, params_ok) else {
        return rpc_error(id.unwrap_or(Value::Null), -32600, "Invalid request");
    };
    let Some(id) = id else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = &message["params"];
    let result = match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or_default();
            let version = PROTOCOL_VERSIONS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&PROTOCOL_VERSIONS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": SERVER_NAME, "version": env!("ELSE_VERSION") },
                "instructions": SERVER_INSTRUCTIONS,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": super::tool_search::listed(all_tools()) })),
        "tools/call" if params["name"] == super::tool_search::FIND => {
            match params["arguments"]["query"].as_str() {
                Some(query) if !query.trim().is_empty() && query.len() <= 200 => {
                    Ok(tool_result(super::tool_search::find(&all_tools(), query)))
                }
                _ => Ok(tool_error("Give a query of at most 200 bytes.")),
            }
        }
        "tools/call" => {
            // use_tool runs a deferred tool exactly as a direct call would:
            // the same grant check below, under the deferred tool's name.
            let (name, arguments) = if params["name"] == super::tool_search::USE {
                match super::tool_search::target(&all_tools(), &params["arguments"]) {
                    Ok((name, arguments)) => (Some(name), arguments),
                    Err(message) => {
                        return Json(
                            json!({ "jsonrpc": "2.0", "id": id, "result": tool_error(message) }),
                        )
                        .into_response();
                    }
                }
            } else {
                (params["name"].as_str(), &params["arguments"])
            };
            let needs = match name {
                Some(
                    "delegate" | "council" | "list_bots" | "delegation_update"
                    | "delegation_cancel",
                ) => Tool::Delegate,
                Some("mcp_install" | super::skills::INSTALL) => Tool::ManageMcp,
                Some(super::skills::READ) => Tool::Mcp,
                Some(
                    "mcp_list"
                    | "mcp_request"
                    | "mcp_wait"
                    | "mcp_connect"
                    | "mcp_connection_status",
                ) => Tool::Mcp,
                Some("create_bot") => Tool::CreateBot,
                Some("rename_bot" | "set_bot_model") => Tool::EditBot,
                Some("rules_update") => Tool::Rules,
                // Previews read the project, which only turns with the computer may do.
                Some(super::visual_tools::NAME) => Tool::Computer,
                Some("request_secret") => Tool::RequestSecret,
                Some("ask_permission") => Tool::AskPermission,
                Some("ask_owner" | "confirm_permission" | "social_connect" | "mail_connect") => {
                    Tool::AskOwner
                }
                Some("draft_email" | "send_draft") => Tool::DraftEmail,
                Some(name) if super::outgoing_tools::TOOL_NAMES.contains(&name) => Tool::AskOwner,
                Some(name)
                    if name.starts_with("routine_")
                        || name.starts_with("reminder_")
                        || name.starts_with("webhook_")
                        || name.starts_with("social_trigger_")
                        || name.starts_with("mail_trigger_") =>
                {
                    Tool::Routines
                }
                Some(name) if name.starts_with("computer_") => Tool::Computer,
                Some("mail_accounts" | "mail_action" | "mail_action_status") => Tool::Mail,
                Some("social_connections" | "social_action" | "social_action_status") => {
                    Tool::Social
                }
                Some("else_status") => Tool::Diagnose,
                _ => Tool::ReadMemory,
            };
            if matches!(name, Some("delegate" | "council"))
                && !grant.allows(Tool::Delegate)
                && let Some(refusal) = task_cannot_delegate(&state, &grant).await
            {
                return Json(json!({ "jsonrpc": "2.0", "id": id, "result": refusal }))
                    .into_response();
            }
            if let Err(err) = check(&state, &grant, needs).await {
                return err.into_response();
            }
            match (name, arguments) {
                (Some(name), args @ (Value::Object(_) | Value::Null)) => {
                    Ok(call(&state, &grant, name, args).await)
                }
                _ => Err((-32602, "Invalid params: name and object arguments")),
            }
        }
        _ => Err((-32601, "Method not found")),
    };
    match result {
        Ok(result) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response(),
        Err((code, message)) => rpc_error(id, code, message),
    }
}

/// A delegated task stays one hop, but a bare grant error leaves it guessing
/// how its follow-up work gets done. Name the supported route instead.
async fn task_cannot_delegate(state: &AppState, grant: &Grant) -> Option<Value> {
    let parent: Option<String> = sqlx::query_scalar(
        "SELECT b.name FROM delegations d JOIN bots b ON b.id = d.parent_bot_id
         WHERE d.child_turn_id = ?1",
    )
    .bind(grant.turn_id)
    .fetch_optional(&state.db)
    .await
    .ok()?;
    let parent = parent?;
    Some(tool_error(
        &json!({
            "error": "NOT_ALLOWED: a delegated task cannot delegate or start a council.",
            "next": format!(
                "Do not retry. Finish your part, then end your reply with each follow-up \
                 another bot should do: its name, bot_id, and a self-contained task. Your \
                 reply goes to {parent}, which can delegate it."
            ),
        })
        .to_string(),
    ))
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }))
        .into_response()
}

fn tool_list() -> Value {
    let read_only = json!({ "readOnlyHint": true, "destructiveHint": false,
                            "idempotentHint": true, "openWorldHint": false });
    let bot = json!({ "type": "string", "format": "uuid",
                      "description": "Another bot's ID from memory_bots. Omit for your own memory." });
    json!([
        {
            "name": "memory_bots",
            "description": "List the bots whose memory you may read: yourself, and bots the owner shared.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": read_only,
        },
        {
            "name": "memory_search",
            "description": "Search original memory entries (messages, corrections, tool activity) by words. \
                Returns up to 20 hits, best first, with sequence numbers to read in full. Entries an owner \
                correction replaced are hidden unless include_superseded is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1, "maxLength": MAX_QUERY },
                    "bot_id": bot,
                    "include_superseded": { "type": "boolean" },
                    "cursor": { "type": "string", "description": "next_cursor from a previous page" },
                },
                "required": ["query"],
                "additionalProperties": false,
            },
            "annotations": read_only,
        },
        {
            "name": "memory_read",
            "description": "Read original memory entries in order from start_seq (count up to 20). \
                With count 1, offset pages through a long entry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start_seq": { "type": "integer", "minimum": 0 },
                    "count": { "type": "integer", "minimum": 1, "maximum": MAX_READ },
                    "offset": { "type": "integer", "minimum": 0 },
                    "bot_id": bot,
                },
                "required": ["start_seq"],
                "additionalProperties": false,
            },
            "annotations": read_only,
        },
        {
            "name": "memory_view",
            "description": "The saved overview of a bot's memory: ordered nodes, each covering a range of \
                entries (start+span), oldest first. Summaries are exact copies or joins of originals; \
                zoom into a node, or read originals, before relying on it. Reports covered and pending \
                ranges explicitly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "bot_id": bot,
                    "from_index": { "type": "integer", "minimum": 0,
                                    "description": "next_index from a previous page" },
                },
                "additionalProperties": false,
            },
            "annotations": read_only,
        },
        {
            "name": "memory_zoom",
            "description": "Open one node from memory_view or a previous zoom: its two halves, or for \
                span 1 the original entry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start_seq": { "type": "integer", "minimum": 0 },
                    "span": { "type": "integer", "minimum": 1 },
                    "bot_id": bot,
                },
                "required": ["start_seq", "span"],
                "additionalProperties": false,
            },
            "annotations": read_only,
        },
        {
            "name": "list_bots",
            "description": "Discover all other active bots and categories, including the main bot ID. Filter by category_id to find council members. This does not grant access to their private memory.",
            "inputSchema": {"type": "object", "properties": {"category_id": {"type": "string", "format": "uuid"}}, "additionalProperties": false},
            "annotations": read_only,
        },
        {
            "name": "council",
            "description": "Ask 1 to 32 distinct bots for reports on the same self-contained task. Use list_bots with category_id to find every member of a category. Accepted atomically; runs asynchronously and returns all outcomes in a continuation. After pending, end this attempt immediately. Delegated tasks cannot create councils. No memory or permissions are shared.",
            "inputSchema": {"type": "object", "properties": {
                "target_bot_ids": {"type": "array", "items": {"type": "string", "format": "uuid"}, "minItems": 1, "maxItems": crate::db::delegations::MAX_COUNCIL_BOTS, "uniqueItems": true},
                "task": {"type": "string", "minLength": 1, "maxLength": 4000}
            }, "required": ["target_bot_ids", "task"], "additionalProperties": false},
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
        },
        {
            "name": "delegate",
            "description": "Hand one subtask to another registered bot (the main bot included) \
                from your registry or a create_bot result, by its exact bot_id. It is for work, not \
                status reports. Not available in a delegated task. Returns status pending with a \
                task_id at once. Then tell the owner briefly and end this attempt; the other bot's \
                reply arrives later in a continuation. At most one per attempt and two per \
                request.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target_bot_id": { "type": "string", "format": "uuid" },
                    "task": { "type": "string", "minLength": 1, "maxLength": 4000,
                              "description": "What the other bot should do, self-contained." },
                    "files": { "type": "array", "maxItems": crate::app::messages::MAX_MESSAGE_FILES,
                               "items": { "type": "string", "minLength": 1, "maxLength": 1024 },
                               "description": "Project paths of files the other bot needs, up to 10 MiB \
                                   each. It gets exact copies, never your project. Its replies can \
                                   attach files back the same way you attach them for the owner." },
                    "owner_answer": { "type": "string", "format": "uuid",
                                      "description": "ID of a choice card the owner answered in this request that \
                                          decides this task. The other bot sees the answer as the owner's own words." },
                },
                "required": ["target_bot_id", "task"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false,
                             "idempotentHint": true, "openWorldHint": false },
        },
        {
            "name": "delegation_update",
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
        },
        {
            "name": "delegation_cancel",
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
        },
        {
            "name": "create_bot",
            "description": "Register a new else.bot specialist for an owner request, with no shared memory or \
                access. Pass its purpose, which is saved to its memory, and model only when the \
                owner named one. Returns its bot_id at once: delegate its first task to it in this \
                attempt. Repeating the same name in this request returns the same bot. category_id \
                comes from list_bots.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "minLength": 1,
                              "maxLength": crate::app::bots::MAX_NAME_CHARS,
                              "description": "Short display name, for example GitHub." },
                    "purpose": { "type": "string", "minLength": 1, "maxLength": crate::app::bots::MAX_PURPOSE_CHARS },
                    "category_id": { "type": "string", "format": "uuid" },
                    "model": { "type": "string", "maxLength": super::routines::MAX_MODEL_CHARS,
                               "description": "Only when the owner named the model: its ID or name, such as grok-4.7 or Claude Opus \
                                   5.5; it selects the provider. Omit it for the default. An unmatched name creates \
                                   nothing; the error lists models." },
                    "provider_id": { "type": "string",
                                     "description": format!(
                                         "With model, only when the owner named the provider ({}).",
                                         crate::providers::definitions::id_list()
                                     ) },
                    "reasoning_effort": { "type": "string",
                                          "description": "With model, only when the owner asked for \
                                              one (low, medium, high, xhigh, max)." },
                    "service_tier": { "type": "string",
                                      "description": "With model, only when the owner asked: \
                                          priority is the Fast tier, default the standard one." },
                },
                "required": ["name"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false,
                             "idempotentHint": true, "openWorldHint": false },
        },
        {
            "name": "rename_bot",
            "description": "Rename an existing else.bot when asked, including yourself or another bot. \
                Available to any bot for work rooted in an owner request. Use the exact target_bot_id \
                from your registry, task, or a create_bot result. Changes only the display name; \
                repeating the same name changes nothing. Confirm the saved name from the result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target_bot_id": { "type": "string", "format": "uuid" },
                    "name": { "type": "string", "minLength": 1,
                              "maxLength": crate::app::bots::MAX_NAME_CHARS },
                },
                "required": ["target_bot_id", "name"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false,
                             "idempotentHint": true, "openWorldHint": false },
        },
        {
            "name": "set_bot_model",
            "description": "Set an else.bot's own model, reasoning effort, and service tier when the owner asks, \
                including yours or another bot's, by exact target_bot_id. Its next turn uses it; a \
                running turn and routines with their own model keep theirs. An unmatched model \
                changes nothing and the error lists models. Confirm the saved model from the result.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target_bot_id": { "type": "string", "format": "uuid" },
                    "model": { "type": "string", "minLength": 1,
                               "maxLength": super::routines::MAX_MODEL_CHARS,
                               "description": "Its ID or name, such as gpt-6.1-sol or Claude Opus 5.5; it selects the provider. Omit to keep the bot's model." },
                    "provider_id": { "type": "string",
                                     "description": format!(
                                         "With model, only when the owner named the provider ({}).",
                                         crate::providers::definitions::id_list()
                                     ) },
                    "reasoning_effort": { "type": "string",
                                          "description": "low, medium, high, xhigh, or max. Omit to keep the bot's current effort for that provider." },
                    "service_tier": { "type": "string",
                                      "description": "priority is the Fast tier, default the standard one. Omit to keep the bot's current tier." },
                },
                "required": ["target_bot_id"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false,
                             "idempotentHint": true, "openWorldHint": false },
        },
        super::rules::schema(),
        super::visual_tools::schema(),
        super::owner_tools::secret_schema(),
        super::owner_tools::handoff_schema(),
        super::owner_tools::choice_schema(),
        super::owner_tools::permission_schema(),
        super::owner_tools::confirm_permission_schema(),
        super::routines::create_schema(),
        super::routines::list_schema(),
        super::routines::update_schema(),
        super::routines::delete_schema(),
        super::routines::run_schema(),
        super::routines::reminder_schema(),
        super::webhooks::create_schema(),
        super::webhooks::list_schema(),
        super::webhooks::update_schema(),
        super::webhooks::delete_schema(),
        {
            "name": "else_status",
            "description": "This else.bot installation's version and the state of each part \
                the owner sees: every provider CLI (available, or the exact reason it is not, \
                such as a path that fails or a missing login), whether it has a saved model \
                profile, bot computers, and backups. Call it before reporting that a provider, \
                model, computer, or backup is missing or broken, and quote its reasons.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": read_only,
        },
        {
            "name": "memory_date",
            "description": "When entry seq (and optionally end_seq) was recorded, with its source.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "seq": { "type": "integer", "minimum": 0 },
                    "end_seq": { "type": "integer", "minimum": 0 },
                    "bot_id": bot,
                },
                "required": ["seq"],
                "additionalProperties": false,
            },
            "annotations": read_only,
        }
    ])
}

pub(crate) fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

pub(crate) fn tool_image(meta: Value, png: &[u8]) -> Value {
    use base64::Engine;
    json!({ "content": [
        { "type": "text", "text": meta.to_string() },
        { "type": "image", "mimeType": "image/png",
          "data": base64::engine::general_purpose::STANDARD.encode(png) },
    ] })
}

/// Every else.bot tool definition, listed or deferred.
pub(super) fn all_tools() -> Vec<Value> {
    let mut tools = tool_list().as_array().cloned().unwrap_or_default();
    tools.extend(super::outgoing_tools::schemas());
    tools.extend(super::guest_tools::tool_list());
    tools.extend(super::connectors::tool_list());
    tools.extend(super::social_tools::schemas());
    tools.extend(super::shared_mcp::schemas());
    tools.push(super::mcp_wait::schema());
    tools.extend(super::skills::schemas());
    tools
}

pub(crate) fn tool_result(value: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": value.to_string() }] })
}

pub(crate) fn wire_size(value: &Value) -> usize {
    tool_result(value.clone()).to_string().len()
}

fn fill(
    result: &mut Value,
    key: &str,
    count: usize,
    first_body: usize,
    mut make: impl FnMut(usize, usize) -> Value,
) -> usize {
    let mut added = 0;
    for index in 0..count {
        let remaining = RESPONSE_BUDGET.saturating_sub(wire_size(result));
        let mut max = first_body.min(remaining / (count - index).max(1)).max(64);
        loop {
            let item = make(index, max);
            result[key].as_array_mut().unwrap().push(item);
            let size = wire_size(result);
            if size <= RESPONSE_BUDGET {
                break;
            }
            result[key].as_array_mut().unwrap().pop();
            if max == 0 {
                return added;
            }
            max = max.saturating_sub(size - RESPONSE_BUDGET);
        }
        added += 1;
    }
    added
}

async fn target(state: &AppState, grant: &Grant, args: &Value) -> Result<Uuid, Value> {
    let target = match args.get("bot_id") {
        None | Some(Value::Null) => grant.bot_id,
        Some(value) => value
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| tool_error("bot_id must be a bot ID from memory_bots."))?,
    };
    if grant
        .memory_scope
        .as_ref()
        .is_some_and(|scope| !scope.contains(&target))
    {
        return Err(tool_error(
            "No readable memory for that bot. Use memory_bots.",
        ));
    }
    match memory::readable(&state.db, grant.bot_id, target).await {
        Ok(true) => Ok(target),
        Ok(false) => Err(tool_error(
            "No readable memory for that bot. Use memory_bots.",
        )),
        Err(err) => {
            tracing::warn!(error = %err, "memory access check failed");
            Err(tool_error("Memory is unavailable right now."))
        }
    }
}

pub(crate) fn slice(text: &str, start: usize, max: usize) -> (usize, &str) {
    let mut from = start.min(text.len());
    while !text.is_char_boundary(from) {
        from -= 1;
    }
    let mut to = from.saturating_add(max).min(text.len());
    while !text.is_char_boundary(to) {
        to -= 1;
    }
    (from, &text[from..to])
}

const MAX_LINKS: usize = 10;

fn entry_json(bot_id: Uuid, entry: &EntryRow, offset: usize, max: usize) -> Value {
    let (from, body) = slice(&entry.body, offset, max);
    let end = from + body.len();
    let links = &entry.superseded_by[..entry.superseded_by.len().min(MAX_LINKS)];
    json!({
        "bot_id": bot_id,
        "seq": entry.seq,
        "kind": entry.kind,
        "authority": entry.authority,
        "created_at": entry.created_at,
        "source": entry.source,
        "part": { "index": entry.part_index, "count": entry.part_count },
        "supersedes_seq": entry.supersedes_seq,
        "superseded_by": links,
        "more_superseded_by": entry.superseded_by.len() > links.len(),
        "body": body,
        "body_bytes": entry.body.len(),
        "offset": from,
        "truncated": end < entry.body.len(),
        "next_offset": (end < entry.body.len()).then_some(end),
    })
}

async fn call(state: &AppState, grant: &Grant, name: &str, args: &Value) -> Value {
    use crate::db::owner_questions::{self, UseOutcome};
    let ((result, log), used) =
        owner_questions::track_use(dispatch(state, grant, name, args)).await;
    // A parked call may or may not have run its effect: its use stays started.
    if let Some(question) = used.filter(|_| !crate::app::tools::parks(name, &result)) {
        let outcome = if result["isError"] == true {
            UseOutcome::Error
        } else {
            UseOutcome::Completed
        };
        if let Err(err) = owner_questions::record_use(&state.db, question, outcome).await {
            tracing::warn!(error = %err, %question, "an approved call's outcome was not recorded");
        }
    }
    if let Some(log) = log {
        let logged = crate::app::tools::loggable_args(name, args);
        record(state, grant, name, &logged, &log).await;
    }
    with_next(name, result)
}

async fn dispatch(state: &AppState, grant: &Grant, name: &str, args: &Value) -> Outcome {
    match name {
        "memory_bots" => bots(state, grant).await,
        "mcp_list" | "mcp_install" | "mcp_request" | "mcp_connect" | "mcp_connection_status" => {
            super::shared_mcp::call(state, grant, name, args).await
        }
        super::mcp_wait::NAME => super::mcp_wait::call(state, grant, args).await,
        super::skills::INSTALL | super::skills::READ => {
            super::skills::call(state, grant, name, args).await
        }
        "memory_search" => search(state, grant, args).await,
        "memory_read" => read(state, grant, args).await,
        "memory_view" => view(state, grant, args).await,
        "memory_zoom" => zoom(state, grant, args).await,
        "memory_date" => date(state, grant, args).await,
        "else_status" => (tool_result(super::status(state).await), None),
        "delegate" => delegate(state, grant, args).await,
        "council" => council(state, grant, args).await,
        "delegation_update" => delegation_update(state, grant, args).await,
        "delegation_cancel" => delegation_cancel(state, grant, args).await,
        "list_bots" => list_bots(state, grant, args).await,
        "create_bot" => create_bot(state, grant, args).await,
        "rename_bot" => rename_bot(state, grant, args).await,
        "set_bot_model" => set_bot_model(state, grant, args).await,
        "request_secret" => super::owner_tools::request_secret(state, grant, args).await,
        "social_connect"
        | "social_connections"
        | "social_action"
        | "social_action_status"
        | "social_trigger_set"
        | "social_trigger_list"
        | "social_trigger_delete" => super::social_tools::call(state, grant, name, args).await,
        "ask_owner" => super::owner_tools::ask_owner(state, grant, args).await,
        "ask_permission" => super::owner_tools::ask_permission(state, grant, args).await,
        "confirm_permission" => super::owner_tools::confirm_permission(state, grant, args).await,
        name if super::outgoing_tools::TOOL_NAMES.contains(&name) => {
            super::outgoing_tools::call(state, grant, name, args).await
        }
        "routine_create" => super::routines::create(state, grant, args).await,
        "routine_list" => super::routines::list(state, grant).await,
        "routine_update" => super::routines::update(state, grant, args).await,
        "routine_delete" => super::routines::delete(state, grant, args).await,
        "routine_run" => super::routines::run(state, grant, args).await,
        "reminder_create" => super::routines::create_reminder(state, grant, args).await,
        "rules_update" => super::rules::update(state, grant, args).await,
        super::visual_tools::NAME => super::visual_tools::call(state, grant, args).await,
        "webhook_create" => super::webhooks::create(state, grant, args).await,
        "webhook_list" => super::webhooks::list(state, grant).await,
        "webhook_update" => super::webhooks::update(state, grant, args).await,
        "webhook_delete" => super::webhooks::delete(state, grant, args).await,
        "mail_accounts"
        | "mail_connect"
        | "mail_action"
        | "mail_action_status"
        | "mail_trigger_set"
        | "mail_trigger_delete" => super::connectors::call(state, grant, name, args).await,
        "computer_handoff" => super::owner_tools::computer_handoff(state, grant, args).await,
        name if name.starts_with("computer_") => {
            super::guest_tools::call(state, grant, name, args).await
        }
        _ => (tool_error("Unknown tool."), None),
    }
}

/// What the server tells a client about itself on initialize. Codex appends
/// it to every tool's description, so it stays a label; the system prompt
/// says how to treat results.
pub(crate) const SERVER_INSTRUCTIONS: &str = "else.bot's tools.";

/// What to do after a tool parked the turn, for results that do not say: the
/// guidance arrives when it applies instead of in every turn's descriptions.
pub(crate) const PARKED_NEXT: &str = "Tell the owner briefly what you are waiting for and end this \
    attempt now, without more tool calls. You continue automatically when it is settled.";

fn with_next(name: &str, mut result: Value) -> Value {
    if !crate::app::tools::parks(name, &result) {
        return result;
    }
    let Some(block) = result["content"]
        .as_array_mut()
        .and_then(|blocks| blocks.iter_mut().find(|b| b["type"] == "text"))
    else {
        return result;
    };
    let Some(mut payload) = block["text"]
        .as_str()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
    else {
        return result;
    };
    if payload.get("next").is_none() {
        payload["next"] = json!(PARKED_NEXT);
        block["text"] = json!(payload.to_string());
    }
    result
}

pub(crate) type Outcome = (Value, Option<String>);

async fn bots(state: &AppState, grant: &Grant) -> Outcome {
    match memory::readable_bots(&state.db, grant.bot_id).await {
        Ok(mut rows) => {
            if let Some(scope) = &grant.memory_scope {
                rows.retain(|(id, _)| scope.contains(id));
            }
            let mut result = json!({ "bots": [], "more": false });
            let shown = fill(&mut result, "bots", rows.len(), 2_000, |i, _| {
                let (id, name) = &rows[i];
                json!({ "bot_id": id, "name": name, "self": *id == grant.bot_id })
            });
            result["more"] = json!(shown < rows.len());
            let log = format!("{shown} readable bots");
            (tool_result(result), Some(log))
        }
        Err(_) => (tool_error("Memory is unavailable right now."), None),
    }
}

fn parse_cursor(cursor: &str) -> Option<(i64, i64)> {
    let (watermark, offset) = cursor.split_once('.')?;
    let pair = (watermark.parse().ok()?, offset.parse().ok()?);
    (pair.0 >= 0 && pair.1 >= 0).then_some(pair)
}

async fn search(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Some(query) = args["query"]
        .as_str()
        .map(str::trim)
        .filter(|q| !q.is_empty())
    else {
        return (tool_error("query is required."), None);
    };
    if query.chars().count() > MAX_QUERY {
        return (
            tool_error("query is too long (200 characters at most)."),
            None,
        );
    }
    let bot_id = match target(state, grant, args).await {
        Ok(bot) => bot,
        Err(err) => return (err, None),
    };
    let (watermark, offset) = match args.get("cursor").and_then(Value::as_str) {
        None => (None, 0),
        Some(cursor) => match parse_cursor(cursor) {
            Some((w, o)) => (Some(w), o),
            None => {
                return (
                    tool_error("cursor is invalid; search again without it."),
                    None,
                );
            }
        },
    };
    let include = args["include_superseded"].as_bool().unwrap_or(false);
    let page = match memory::search(&state.db, bot_id, query, include, watermark, offset).await {
        Ok(page) => page,
        Err(err) => {
            tracing::warn!(error = %err, "memory search failed");
            return (tool_error("Memory is unavailable right now."), None);
        }
    };
    let mut result = json!({ "bot_id": bot_id, "hits": [], "next_cursor": null });
    let shown = fill(&mut result, "hits", page.hits.len(), HIT_BODY, |i, max| {
        entry_json(bot_id, &page.hits[i], 0, max.min(HIT_BODY))
    });
    let next = if shown < page.hits.len() {
        Some(offset + shown as i64)
    } else {
        page.next_offset
    };
    let seqs: Vec<String> = page.hits[..shown]
        .iter()
        .map(|e| e.seq.to_string())
        .collect();
    let log = format!(
        "query {:?}{}: {} hits [{}]",
        query,
        if bot_id == grant.bot_id {
            String::new()
        } else {
            format!(" in bot {bot_id}")
        },
        shown,
        seqs.join(", ")
    );
    result["next_cursor"] = json!(next.map(|o| format!("{}.{}", page.watermark, o)));
    (tool_result(result), Some(log))
}

async fn read(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Some(start) = args["start_seq"].as_i64().filter(|s| *s >= 0) else {
        return (
            tool_error("start_seq must be a non-negative integer."),
            None,
        );
    };
    let count = args.get("count").and_then(Value::as_i64).unwrap_or(5);
    if !(1..=MAX_READ).contains(&count) {
        return (tool_error("count must be between 1 and 20."), None);
    }
    let offset = args.get("offset").and_then(Value::as_i64).unwrap_or(0);
    if offset < 0 || (offset > 0 && count != 1) {
        return (
            tool_error("offset needs count 1 and must not be negative."),
            None,
        );
    }
    let bot_id = match target(state, grant, args).await {
        Ok(bot) => bot,
        Err(err) => return (err, None),
    };
    let (rows, length) = match tokio::try_join!(
        memory::read(&state.db, bot_id, start, count),
        memory::length(&state.db, bot_id)
    ) {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(error = %err, "memory read failed");
            return (tool_error("Memory is unavailable right now."), None);
        }
    };
    let mut result = json!({ "bot_id": bot_id, "entries": [], "length": length,
                             "next_start_seq": null });
    let shown = fill(
        &mut result,
        "entries",
        rows.len(),
        RESPONSE_BUDGET,
        |i, max| entry_json(bot_id, &rows[i], offset as usize, max),
    );
    if shown < rows.len() {
        result["next_start_seq"] = json!(rows[shown].seq);
    }
    let log = format!(
        "read{} from seq {start}: {} entries",
        if bot_id == grant.bot_id {
            String::new()
        } else {
            format!(" bot {bot_id}")
        },
        shown
    );
    (tool_result(result), Some(log))
}

async fn record(state: &AppState, grant: &Grant, name: &str, args: &Value, log: &str) {
    let key = format!("tool:{}:{}", grant.turn_id, Uuid::new_v4());
    let body = format!("{name} {args} -> {log}");
    let entry = NewEntry {
        request_key: &key,
        kind: Kind::ToolActivity,
        body: memory::split(&body, 4_000)[0],
        source: json!({
            "conversation_id": grant.conversation_id,
            "turn_id": grant.turn_id,
            "tool": name,
        }),
        supersedes_seq: None,
    };
    let result: Result<(), memory::AppendError> = async {
        let mut tx = state.db.begin_with("BEGIN IMMEDIATE").await?;
        match memory::append(&mut tx, grant.bot_id, &entry).await {
            Ok(_) => tx.commit().await.map_err(memory::AppendError::Db),
            Err(err) => Err(err),
        }
    }
    .await;
    if let Err(err) = result {
        tracing::warn!(error = %err, "tool activity was not logged");
    }
}

/// `GET` would open a server stream, which this endpoint does not offer.
pub async fn no_stream() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "This MCP endpoint answers POST requests only.",
    )
}

fn unavailable() -> Outcome {
    (tool_error("Memory is unavailable right now."), None)
}

fn suffix(grant: &Grant, bot_id: Uuid) -> String {
    if bot_id == grant.bot_id {
        String::new()
    } else {
        format!(" bot {bot_id}")
    }
}

async fn view(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let bot_id = match target(state, grant, args).await {
        Ok(bot) => bot,
        Err(err) => return (err, None),
    };
    let from = args.get("from_index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let Ok(saved) = memory_tree::load_view(&state.db, bot_id).await else {
        return unavailable();
    };
    let covered = (saved.published_seq >= 0)
        .then(|| json!({ "start_seq": 0, "end_seq": saved.published_seq }));
    let pending = (saved.published_seq + 1 < saved.length)
        .then(|| json!({ "start_seq": saved.published_seq + 1, "end_seq": saved.length - 1 }));
    let blocked: Vec<Value> = saved
        .blocked
        .iter()
        .map(|(reason, jobs)| json!({ "reason": reason, "jobs": jobs }))
        .collect();
    let mut result = json!({
        "bot_id": bot_id,
        "generation": saved.generation,
        "revision": saved.revision,
        "valid": saved.valid,
        "length": saved.length,
        "covered": covered,
        "pending": pending,
        "blocked": blocked,
        "compacting": saved.compacting,
        "nodes": [],
        "next_index": null,
    });
    let rest = saved.nodes.get(from..).unwrap_or_default();
    let shown = fill(&mut result, "nodes", rest.len(), 4_000, |i, max| {
        let node = &rest[i];
        let mut item = json!({
            "start_seq": node.node.start,
            "span": node.node.span,
            "method": node.method,
        });
        if let Some(body) = node.body.as_deref() {
            text_fields(&mut item, node.node, body, max);
        }
        item
    });
    if shown < rest.len() {
        result["next_index"] = json!(from + shown);
    }
    let log = format!(
        "view{} generation {} revision {}: {} nodes from {}",
        suffix(grant, bot_id),
        saved.generation,
        saved.revision,
        shown,
        from
    );
    (tool_result(result), Some(log))
}

fn text_fields(item: &mut Value, node: NodeId, body: &str, max: usize) {
    let text = slice(body, 0, max).1;
    item["text"] = json!(text);
    item["text_bytes"] = json!(body.len());
    item["truncated"] = json!(text.len() < body.len());
    if text.len() < body.len() {
        item["continue_with"] = if node.span == 1 {
            json!({ "tool": "memory_read", "start_seq": node.start, "count": 1, "offset": text.len() })
        } else {
            json!({ "tool": "memory_zoom", "start_seq": node.start, "span": node.span })
        };
    }
}

async fn zoom(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let (Some(start), Some(span)) = (args["start_seq"].as_i64(), args["span"].as_i64()) else {
        return (tool_error("start_seq and span are required."), None);
    };
    let id = NodeId { start, span };
    if !id.is_valid() {
        return (
            tool_error("span must be a power of two and start_seq a multiple of it."),
            None,
        );
    }
    let bot_id = match target(state, grant, args).await {
        Ok(bot) => bot,
        Err(err) => return (err, None),
    };
    let Ok(saved) = memory_tree::load_view(&state.db, bot_id).await else {
        return unavailable();
    };
    if id.end() > saved.length {
        return (tool_error("That range is beyond the end of memory."), None);
    }
    let generation = saved.generation;
    let mut result = json!({ "bot_id": bot_id, "generation": generation,
                             "start_seq": start, "span": span, "parts": [] });
    match id.children() {
        None => {
            let Ok(rows) = memory::read(&state.db, bot_id, start, 1).await else {
                return unavailable();
            };
            let Some(entry) = rows.first() else {
                return (tool_error("No such entry."), None);
            };
            fill(
                &mut result,
                "parts",
                1,
                RESPONSE_BUDGET,
                |_, max| json!({ "original": entry_json(bot_id, entry, 0, max) }),
            );
        }
        Some((left, right)) => {
            let mut parts = Vec::new();
            for child in [left, right] {
                let Ok(built) = memory_tree::node(&state.db, bot_id, generation, child).await
                else {
                    return unavailable();
                };
                parts.push((child, built));
            }
            fill(&mut result, "parts", 2, 6_000, |i, max| {
                let (child, built) = &parts[i];
                match built {
                    Some((body, method)) => {
                        let mut item = json!({
                            "start_seq": child.start, "span": child.span, "built": true,
                            "method": method,
                        });
                        text_fields(&mut item, *child, body, max);
                        item
                    }
                    None => json!({ "start_seq": child.start, "span": child.span, "built": false }),
                }
            });
        }
    }
    let log = format!("zoom{} {start}+{span}", suffix(grant, bot_id));
    (tool_result(result), Some(log))
}

async fn date(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Some(seq) = args["seq"].as_i64().filter(|s| *s >= 0) else {
        return (tool_error("seq must be a non-negative integer."), None);
    };
    let end = args.get("end_seq").and_then(Value::as_i64);
    if end.is_some_and(|e| e < seq) {
        return (tool_error("end_seq must not be before seq."), None);
    }
    let bot_id = match target(state, grant, args).await {
        Ok(bot) => bot,
        Err(err) => return (err, None),
    };
    let mut dates = Vec::new();
    for at in std::iter::once(seq).chain(end) {
        let Ok(rows) = memory::read(&state.db, bot_id, at, 1).await else {
            return unavailable();
        };
        match rows.first().filter(|e| e.seq == at) {
            Some(e) => dates.push(json!({
                "seq": e.seq, "created_at": e.created_at, "kind": e.kind,
                "authority": e.authority, "source": e.source,
            })),
            None => return (tool_error("No such entry."), None),
        }
    }
    let log = format!(
        "date{} {seq}{}",
        suffix(grant, bot_id),
        end.map(|e| format!("..{e}")).unwrap_or_default()
    );
    (
        tool_result(json!({ "bot_id": bot_id, "dates": dates })),
        Some(log),
    )
}

async fn delegate(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
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

async fn delegation_update(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
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

async fn delegation_cancel(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
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

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListBotsArgs {
    category_id: Option<Uuid>,
}

async fn list_bots(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let parsed = if args.is_null() {
        Ok(ListBotsArgs::default())
    } else {
        serde_json::from_value(args.clone())
    };
    let Ok(parsed) = parsed else {
        return (
            tool_error("Pass an optional category_id from list_bots."),
            None,
        );
    };
    match crate::db::bots::list(&state.db, false).await {
        Ok(listing) => {
            if parsed
                .category_id
                .is_some_and(|id| !listing.categories.iter().any(|c| c.id == id))
            {
                return (
                    tool_error("CATEGORY_NOT_FOUND: Use a category_id from list_bots."),
                    None,
                );
            }
            let bots: Vec<_> = listing
                .bots
                .iter()
                .filter(|b| {
                    b.id != grant.bot_id
                        && parsed
                            .category_id
                            .is_none_or(|id| b.category_id == Some(id))
                })
                .map(|b| json!({"bot_id": b.id, "name": b.name, "category_id": b.category_id}))
                .collect();
            (
                tool_result(
                    json!({"main_bot_id": listing.default_bot_id, "categories": listing.categories,
                "bots": bots}),
                ),
                None,
            )
        }
        Err(_) => (
            tool_error("UNAVAILABLE: Bot discovery is unavailable."),
            None,
        ),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CouncilArgs {
    target_bot_ids: Vec<Uuid>,
    task: String,
}

async fn council(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBotArgs {
    name: String,
    purpose: Option<String>,
    category_id: Option<Uuid>,
    model: Option<String>,
    provider_id: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

async fn create_bot(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    use crate::app::bots::{BotName, BotPurpose};
    use crate::db::bots::{self, BotError, Created, Refusal};
    let reject = |code: &str, message: &str| {
        (
            tool_error(&format!("{code}: {message}")),
            Some(format!("rejected {code}")),
        )
    };
    let Ok(parsed) = serde_json::from_value::<CreateBotArgs>(args.clone()) else {
        return reject(
            "INVALID_ARGUMENTS",
            "pass a name, and optionally purpose, category_id, and model with provider_id, \
             reasoning_effort, and service_tier.",
        );
    };
    let name = match BotName::parse(&parsed.name) {
        Ok(name) => name,
        Err(err) => return reject("INVALID_BOT", &err.to_string()),
    };
    let purpose = match parsed.purpose.as_deref().map(BotPurpose::parse).transpose() {
        Ok(purpose) => purpose,
        Err(err) => return reject("INVALID_BOT", err),
    };
    let model = match parsed.model.as_deref() {
        Some(model) => {
            let request = super::routines::ModelRequest {
                model,
                provider_id: parsed.provider_id.as_deref(),
                reasoning_effort: parsed.reasoning_effort.as_deref(),
                service_tier: parsed.service_tier.as_deref(),
            };
            match super::routines::resolve(state, None, &request).await {
                Ok(model) => Some((request.key(), model)),
                Err(reason) => {
                    return reject(
                        "MODEL_NOT_AVAILABLE",
                        &format!(
                            "the model \"{}\" was not applied: {reason}. No bot was created; \
                             retry with an available model or tell the owner.",
                            request.requested()
                        ),
                    );
                }
            }
        }
        None if parsed.provider_id.is_some()
            || parsed.reasoning_effort.is_some()
            || parsed.service_tier.is_some() =>
        {
            return reject(
                "INVALID_ARGUMENTS",
                "provider_id, reasoning_effort, and service_tier apply only together with model.",
            );
        }
        None => None,
    };
    let model = model.as_ref().map(|(request, model)| bots::TurnModel {
        request,
        provider_id: &model.provider_id,
        profile: &model.profile,
    });
    match bots::create_for_turn(
        &state.db,
        grant.turn_id,
        &name,
        purpose.as_ref(),
        model,
        parsed.category_id,
    )
    .await
    {
        Ok(created) => {
            let (bot, new) = match created {
                Created::New(bot) => (bot, true),
                Created::Existing(bot) => (bot, false),
            };
            let next = match bot.status {
                bots::BotStatus::Active => {
                    "It is registered. Delegate to this bot_id when it should work; it can use \
                     the task secrets stored on this server but has no other access until \
                     the owner grants it."
                }
                _ => "The owner deleted this bot; do not delegate to it or create it again.",
            };
            let log = format!(
                "{} {} ({}) {:?}",
                if new { "created" } else { "existing" },
                bot.name,
                bot.id,
                bot.status
            );
            let mut result = json!({
                "bot_id": bot.id,
                "name": bot.name,
                "status": bot.status,
                "created": new,
                "next": next,
            });
            if let Ok(Some(own)) = crate::db::bot_profiles::get(&state.db, bot.id).await {
                result["model"] = super::settings::choice_json(&own.provider_id, &own.profile);
            }
            (tool_result(result), Some(log))
        }
        Err(BotError::Refused(refusal)) => {
            let code = match refusal {
                Refusal::NotOwnerConversation | Refusal::NotOwnerRequest => "NOT_ALLOWED",
                Refusal::PurposeRequired => "PURPOSE_REQUIRED",
                Refusal::NotRunning => "NOT_RUNNING",
                Refusal::AttemptDelegated => "ATTEMPT_PARKED",
            };
            reject(code, &refusal.to_string())
        }
        Err(BotError::RequestConflict) => reject(
            "REQUEST_CONFLICT",
            "This name was already used with another purpose, category, or model in this request.",
        ),
        Err(BotError::CategoryNotFound) => {
            reject("CATEGORY_NOT_FOUND", "Use a category_id from list_bots.")
        }
        Err(err) => {
            tracing::warn!(error = %err, "creating a bot from a turn failed");
            reject(
                "UNAVAILABLE",
                "bots cannot be created right now; try again shortly.",
            )
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBotArgs {
    target_bot_id: Uuid,
    name: String,
}

async fn rename_bot(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    use crate::app::bots::BotName;
    use crate::db::bots;
    let reject = |code: &str, message: &str| {
        (
            tool_error(&format!("{code}: {message}")),
            Some(format!("rejected {code}")),
        )
    };
    let Ok(parsed) = serde_json::from_value::<RenameBotArgs>(args.clone()) else {
        return reject(
            "INVALID_ARGUMENTS",
            "pass a target_bot_id UUID and a name string.",
        );
    };
    let name = match BotName::parse(&parsed.name) {
        Ok(name) => name,
        Err(err) => return reject("INVALID_BOT", &err.to_string()),
    };
    match bots::rename_for_turn(&state.db, grant.turn_id, parsed.target_bot_id, &name).await {
        Ok(bot) => {
            let log = format!("renamed {} ({})", bot.name, bot.id);
            (
                tool_result(json!({ "bot_id": bot.id, "name": bot.name, "status": bot.status })),
                Some(log),
            )
        }
        Err(err) => {
            let Some(code) = edit_refusal(&err) else {
                tracing::warn!(error = %err, "renaming a bot from a turn failed");
                return reject(
                    "UNAVAILABLE",
                    "bots cannot be renamed right now; try again shortly.",
                );
            };
            reject(code, &err.to_string())
        }
    }
}

/// The provider and model a bot's next turn would use, if it has one.
async fn current_model(
    state: &AppState,
    bot_id: Uuid,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let Some(provider) = crate::db::bot_profiles::provider_for(&state.db, bot_id).await? else {
        return Ok(None);
    };
    let mut conn = state.db.acquire().await?;
    let profile = crate::db::bot_profiles::effective(&mut conn, bot_id, &provider).await?;
    Ok(profile.map(|(_, profile)| (provider, profile.0.model)))
}

/// The tool error code for a refused bot edit, or `None` for a failure to log.
fn edit_refusal(err: &crate::db::bots::BotError) -> Option<&'static str> {
    use crate::db::bots::{BotError, Refusal};
    Some(match err {
        BotError::NotFound => "BOT_NOT_FOUND",
        BotError::Deleted => "BOT_UNAVAILABLE",
        BotError::Refused(
            Refusal::NotOwnerConversation | Refusal::NotOwnerRequest | Refusal::PurposeRequired,
        ) => "NOT_ALLOWED",
        BotError::Refused(Refusal::NotRunning) => "NOT_RUNNING",
        BotError::Refused(Refusal::AttemptDelegated) => "ATTEMPT_PARKED",
        _ => return None,
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetBotModelArgs {
    target_bot_id: Uuid,
    model: Option<String>,
    provider_id: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

async fn set_bot_model(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    use crate::db::bots;
    let reject = |code: &str, message: &str| {
        (
            tool_error(&format!("{code}: {message}")),
            Some(format!("rejected {code}")),
        )
    };
    let Ok(parsed) = serde_json::from_value::<SetBotModelArgs>(args.clone()) else {
        return reject(
            "INVALID_ARGUMENTS",
            "pass a target_bot_id UUID, and model, provider_id, reasoning_effort, or \
             service_tier.",
        );
    };
    let current = match parsed.model {
        Some(_) => None,
        None => match current_model(state, parsed.target_bot_id).await {
            Ok(Some(current)) => Some(current),
            Ok(None) => {
                return reject(
                    "MODEL_REQUIRED",
                    "this bot has no model yet; pass the model the owner wants.",
                );
            }
            Err(err) => {
                tracing::warn!(error = %err, "reading a bot's model failed");
                return reject(
                    "UNAVAILABLE",
                    "bot models cannot be changed right now; try again shortly.",
                );
            }
        },
    };
    let request = super::routines::ModelRequest {
        model: parsed
            .model
            .as_deref()
            .or(current.as_ref().map(|(_, model)| model.as_str()))
            .unwrap_or_default(),
        provider_id: parsed
            .provider_id
            .as_deref()
            .or(current.as_ref().map(|(provider, _)| provider.as_str())),
        reasoning_effort: parsed.reasoning_effort.as_deref(),
        service_tier: parsed.service_tier.as_deref(),
    };
    let model = match super::routines::resolve(state, Some(parsed.target_bot_id), &request).await {
        Ok(model) => model,
        Err(reason) => {
            return reject(
                "MODEL_NOT_AVAILABLE",
                &format!(
                    "the model \"{}\" was not applied: {reason}. Nothing changed; retry with an \
                     available model or tell the owner.",
                    request.requested()
                ),
            );
        }
    };
    match bots::set_model_for_turn(
        &state.db,
        grant.turn_id,
        parsed.target_bot_id,
        &model.provider_id,
        &model.profile,
    )
    .await
    {
        Ok(saved) => {
            let log = format!(
                "set {} to {} on {} ({} reasoning, {} tier)",
                parsed.target_bot_id,
                saved.profile.model,
                saved.provider_id,
                saved.profile.reasoning_effort,
                saved.profile.service_tier,
            );
            (
                tool_result(json!({
                    "bot_id": parsed.target_bot_id,
                    "model": super::settings::choice_json(&saved.provider_id, &saved.profile),
                    "next": "Its next turn uses this model. Routines with their own model keep \
                        it; to change one, delegate to that bot asking it to update the routine.",
                })),
                Some(log),
            )
        }
        Err(err) => {
            let Some(code) = edit_refusal(&err) else {
                tracing::warn!(error = %err, "setting a bot's model from a turn failed");
                return reject(
                    "UNAVAILABLE",
                    "bot models cannot be changed right now; try again shortly.",
                );
            };
            reject(code, &err.to_string())
        }
    }
}

#[cfg(test)]
mod next_tests {
    use super::*;

    #[test]
    fn a_parked_result_without_guidance_gets_the_next_step() {
        let parked = tool_result(json!({ "status": "pending", "parked": true }));
        let text = with_next("delegate", parked)["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["next"],
            PARKED_NEXT
        );
        let own = tool_result(json!({ "parked": true, "next": "Own guidance." }));
        assert_eq!(with_next("ask_owner", own.clone()), own);
        let done = tool_result(json!({ "status": "done" }));
        assert_eq!(with_next("delegate", done.clone()), done);
    }
}
