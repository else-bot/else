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
use super::tool_result::{self, Refusal, text_result};
use super::tools::check;
use crate::app::tools::{SERVER_NAME, ToolDef, ToolId};
use crate::db::memory::{self, Kind, NewEntry};
use crate::grants::{Grant, Tool};

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

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
        "tools/list" => Ok(json!({ "tools": super::tool_search::listed() })),
        "tools/call" if params["name"] == super::tool_search::FIND => {
            match params["arguments"]["query"].as_str() {
                Some(query) if !query.trim().is_empty() && query.len() <= 200 => {
                    Ok(text_result(super::tool_search::find(query)))
                }
                _ => Ok(refused("Give a query of at most 200 bytes.")),
            }
        }
        "tools/call" => {
            // use_tool runs a deferred tool exactly as a direct call would:
            // the same grant check below, under the deferred tool's name.
            let (tool, arguments) = if params["name"] == super::tool_search::USE {
                match super::tool_search::target(&params["arguments"]) {
                    Ok(target) => target,
                    Err(message) => {
                        return Json(
                            json!({ "jsonrpc": "2.0", "id": id, "result": refused(message) }),
                        )
                        .into_response();
                    }
                }
            } else {
                let Some(name) = params["name"].as_str() else {
                    return rpc_error(id, -32602, "Invalid params: name and object arguments");
                };
                let Some(tool) = ToolDef::named(name) else {
                    return Json(
                        json!({ "jsonrpc": "2.0", "id": id, "result": refused("Unknown tool.") }),
                    )
                    .into_response();
                };
                (tool, &params["arguments"])
            };
            if matches!(tool.id, ToolId::Delegate | ToolId::Council)
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

/// A plain-text tool error outside a handler, rendered like a handler's.
fn refused(message: impl Into<String>) -> Value {
    tool_result::render(Err(Refusal::text(message))).0
}

async fn call(state: &AppState, grant: &Grant, tool: &'static ToolDef, args: &Value) -> Value {
    use crate::db::owner_questions::{self, UseOutcome};
    let name = tool.name;
    let (result, used) =
        owner_questions::track_use(super::tool_defs::run(tool, state, grant, args)).await;
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
