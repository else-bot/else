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
use super::memory_tools::{MAX_QUERY, MAX_READ};
use super::tool_defs::ToolDef;
use super::tool_result::{self, Refusal, text_result};
use super::tools::check;
use crate::db::memory::{self, Kind, NewEntry};
use crate::grants::{Grant, Tool};

use crate::app::tools::SERVER_NAME;
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
/// The owner reads a task by this line; the task text stays folded.
const TITLE_DESCRIPTION: &str =
    "A short name for the task, for the owner, e.g. Triage overnight WhatsApp DMs. No IDs.";

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
                    Ok(text_result(super::tool_search::find(&all_tools(), query)))
                }
                _ => Ok(refused("Give a query of at most 200 bytes.")),
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
                            json!({ "jsonrpc": "2.0", "id": id, "result": refused(message) }),
                        )
                        .into_response();
                    }
                }
            } else {
                (params["name"].as_str(), &params["arguments"])
            };
            let Some(name) = name else {
                return rpc_error(id, -32602, "Invalid params: name and object arguments");
            };
            let Some(tool) = super::tool_defs::get(name) else {
                return Json(
                    json!({ "jsonrpc": "2.0", "id": id, "result": refused("Unknown tool.") }),
                )
                .into_response();
            };
            if matches!(tool.name, "delegate" | "council")
                && !grant.allows(Tool::Delegate)
                && let Some(refusal) = task_cannot_delegate(&state, &grant).await
            {
                return Json(json!({ "jsonrpc": "2.0", "id": id, "result": refusal }))
                    .into_response();
            }
            if let Err(err) = check(&state, &grant, tool.needs).await {
                return err.into_response();
            }
            match arguments {
                args @ (Value::Object(_) | Value::Null) => {
                    Ok(call(&state, &grant, tool, args).await)
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
    let mut conn = state.db.acquire().await.ok()?;
    let origin = crate::db::delegations::origin(&mut conn, grant.turn_id)
        .await
        .ok()?;
    if origin == crate::app::providers::Origin::ConnectorEvent {
        return Some(refused(
            json!({
                "error": "NOT_ALLOWED: a task handed on from email cannot delegate or start a council.",
                "next": "Do not retry, and list no follow-ups for other bots. Reply with what you took from the task.",
            })
            .to_string(),
        ));
    }
    Some(refused(
        json!({
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
                "task": {"type": "string", "minLength": 1, "maxLength": 4000},
                "title": {"type": "string", "minLength": 1, "maxLength": crate::db::delegations::MAX_TITLE_CHARS, "description": TITLE_DESCRIPTION}
            }, "required": ["target_bot_ids", "task", "title"], "additionalProperties": false},
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false},
        },
        {
            "name": "delegate",
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
        {
            "name": "delete_bot",
            "description": "Ask the owner to delete another else.bot by exact target_bot_id, for an owner request. \
                The owner always approves on a card listing its routines, triggers and tasks that \
                stop; this parks. Once approved, call it again with the same target_bot_id. Never \
                yourself or the main bot.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target_bot_id": { "type": "string", "format": "uuid" },
                    "reason": { "type": "string", "maxLength": crate::app::questions::MAX_TITLE,
                                "description": "Why, in the owner's words. Shown on the card." },
                },
                "required": ["target_bot_id"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true,
                             "idempotentHint": true, "openWorldHint": false },
        },
        super::rules::schema(),
        super::visual_tools::schema(),
        super::owner_tools::secret_schema(),
        super::owner_tools::handoff_schema(),
        super::owner_tools::notice_schema(),
        super::owner_tools::choice_schema(),
        super::owner_tools::permission_schema(),
        super::owner_tools::confirm_permission_schema(),
        super::routines::create_schema(),
        super::routines::list_schema(),
        super::routines::update_schema(),
        super::routines::delete_schema(),
        super::routines::run_schema(),
        super::routines::reminder_schema(),
        super::routines::no_news_schema(),
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

/// A plain-text tool error outside a handler, rendered like a handler's.
fn refused(message: impl Into<String>) -> Value {
    tool_result::render(Err(Refusal::text(message))).0
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

pub(crate) fn wire_size(value: &Value) -> usize {
    text_result(value.clone()).to_string().len()
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

async fn call(state: &AppState, grant: &Grant, tool: &'static ToolDef, args: &Value) -> Value {
    use crate::db::owner_questions::{self, UseOutcome};
    let name = tool.name;
    let (result, used) = owner_questions::track_use(tool.run(state, grant, args)).await;
    let (result, log) = tool_result::render(result);
    // A parked call may or may not have run its effect: its use stays started.
    if let Some(question) = used.filter(|_| !crate::app::tools::parks(tool.name, &result)) {
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
        let logged = (tool.loggable)(args);
        record(state, grant, name, &logged, &log).await;
    }
    with_next(name, result)
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

#[cfg(test)]
mod next_tests {
    use super::*;

    #[test]
    fn a_parked_result_without_guidance_gets_the_next_step() {
        let parked = text_result(json!({ "status": "pending", "parked": true }));
        let text = with_next("delegate", parked)["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap()["next"],
            PARKED_NEXT
        );
        let own = text_result(json!({ "parked": true, "next": "Own guidance." }));
        assert_eq!(with_next("ask_owner", own.clone()), own);
        let done = text_result(json!({ "status": "done" }));
        assert_eq!(with_next("delegate", done.clone()), done);
    }
}
