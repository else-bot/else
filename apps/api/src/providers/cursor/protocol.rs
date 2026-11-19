//! The sidecar's notifications for one Cursor run, validated against the
//! turn's tool policy, and the model and message shapes the SDK takes.
//!
//! The SDK streams `run/message` notifications (its own `SDKMessage`s, with
//! tool arguments reduced to the MCP server and tool names) and then one
//! `run/result`. It reports no tool inventory up front, so each tool call
//! is checked as it arrives.

use std::collections::HashSet;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::app::providers::{Profile, Steer, TurnOutcome, Usage};
use crate::app::tools::{SERVER_NAME, TOOL_NAMES};
use crate::providers::cli::attempt::{Event, Protocol, StopCause, Violation};
use crate::providers::codex_rpc::Incoming;
use crate::providers::models::Listed;
use crate::providers::{attachments, prompt};

/// Efforts else.bot names, applied through a model's `effort` or
/// `reasoning` parameter. `default` leaves the parameter to Cursor, and is
/// the only effort of a model without one.
pub const EFFORTS: &[&str] = &["default", "low", "medium", "high", "xhigh", "max"];
/// `priority` is the Fast tier: the model's `fast` parameter.
pub const TIERS: &[&str] = &["default", "priority"];
const EFFORT_PARAMS: &[&str] = &["effort", "reasoning"];
const FAST_PARAM: &str = "fast";

/// The SDK's host tools a turn started by the owner may use.
const HOST_TOOLS: &[&str] = &["shell", "read", "edit", "delete", "grep", "glob", "ls"];
/// What the `edit` group reports when it writes a whole file.
const HOST_REPORTED: &[&str] = &["write"];
/// Never offered: `task` starts subagents with their own toolsets.
pub const DISALLOWED: &[&str] = &["task"];

/// The `tools` allow-list, sent on create and again on every resume.
pub fn allowed_tools(host: bool, mcp: bool) -> Vec<&'static str> {
    let mut tools = Vec::new();
    if host {
        tools.extend(HOST_TOOLS);
    }
    if mcp {
        tools.push("mcp");
    }
    tools
}

/// The models `Cursor.models.list()` returned, with the efforts and tiers
/// their variants take, else their parameters.
pub fn listed_models(models: &Value) -> Result<Vec<Listed>, String> {
    let models = models
        .as_array()
        .ok_or("Cursor listed its models in an unexpected shape")?;
    Ok(models
        .iter()
        .filter_map(|model| {
            let id = model["id"].as_str()?;
            let mut efforts = vec!["default".to_owned()];
            let mut fast = false;
            let mut add = |param: &str, value: &str| {
                if EFFORT_PARAMS.contains(&param) && !efforts.iter().any(|e| e == value) {
                    efforts.push(value.to_owned());
                }
                fast |= param == FAST_PARAM && value == "true";
            };
            let list = |value: &Value| value.as_array().cloned().unwrap_or_default();
            let variants = list(&model["variants"]);
            if variants.is_empty() {
                for parameter in list(&model["parameters"]) {
                    for value in list(&parameter["values"]) {
                        if let (Some(param), Some(value)) =
                            (parameter["id"].as_str(), value["value"].as_str())
                        {
                            add(param, value);
                        }
                    }
                }
            } else {
                for param in variants.iter().flat_map(|v| list(&v["params"])) {
                    if let (Some(id), Some(value)) = (param["id"].as_str(), param["value"].as_str())
                    {
                        add(id, value);
                    }
                }
            }
            let mut tiers = vec!["default".to_owned()];
            if fast {
                tiers.push("priority".to_owned());
            }
            Some(Listed {
                id: id.to_owned(),
                efforts: Some(efforts),
                tiers: Some(tiers),
            })
        })
        .collect())
}

/// The profile as the sidecar resolves it against the account's catalog:
/// the parameter IDs differ per model.
pub fn selection(profile: &Profile) -> Value {
    json!({
        "id": profile.model,
        "effort": (profile.reasoning_effort != "default").then_some(&profile.reasoning_effort),
        "fast": profile.service_tier == "priority",
    })
}

/// The packet as an `SDKUserMessage`: its text, with each attached image
/// also given inline.
pub fn message(segments: &[prompt::Segment<'_>]) -> Value {
    use base64::Engine;
    let images: Vec<Value> = segments
        .iter()
        .filter_map(|segment| match segment {
            prompt::Segment::File(file) if attachments::is_image(&file.attachment) => Some(json!({
                "data": base64::engine::general_purpose::STANDARD.encode(&file.data),
                "mimeType": file.attachment.media_type,
            })),
            _ => None,
        })
        .collect();
    let mut message = json!({ "text": prompt::segments_text(segments) });
    if !images.is_empty() {
        message["images"] = Value::Array(images);
    }
    message
}

/// How the run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    /// The SDK's result. `kind` is the sidecar's reading of its error:
    /// `usage_limit`, `signed_out`, or none.
    Result {
        status: String,
        kind: Option<String>,
        message: Option<String>,
    },
    /// The stream broke without a result: the run may still be going.
    Lost(String),
}

pub struct Tracker {
    host: bool,
    mcp: bool,
    /// Text since the last tool call, so later text starts a new message.
    text: bool,
    after_tool: bool,
    /// Steered messages the run took whose echo has not arrived, oldest
    /// first.
    steered: std::collections::VecDeque<Uuid>,
    /// The packet's own echo arrived, so later user messages are steers.
    packet_echoed: bool,
    parked_calls: HashSet<String>,
    model: String,
}

impl Tracker {
    pub fn new(profile: &Profile, host: bool, mcp: bool) -> Self {
        Self {
            host,
            mcp,
            text: false,
            after_tool: false,
            steered: Default::default(),
            packet_echoed: false,
            parked_calls: HashSet::new(),
            model: profile.model.clone(),
        }
    }

    fn on_message(&mut self, message: &Value) -> Result<Vec<Event<Terminal>>, Violation> {
        let mut events = Vec::new();
        match message["type"].as_str() {
            Some("assistant") => {
                let blocks = message["message"]["content"].as_array().ok_or_else(|| {
                    Violation::policy("Cursor sent a malformed assistant message")
                })?;
                for block in blocks {
                    let Some(text) = block["text"].as_str().filter(|_| block["type"] == "text")
                    else {
                        continue;
                    };
                    if text.is_empty() {
                        continue;
                    }
                    if self.text && self.after_tool {
                        events.push(Event::EndMessage);
                    }
                    self.text = true;
                    self.after_tool = false;
                    events.push(Event::Text(text.to_owned()));
                }
            }
            Some("tool_call") => {
                self.after_tool = true;
                if self.on_tool_call(message)? {
                    events.push(Event::Parked);
                }
            }
            Some("usage") => {
                let usage = &message["usage"];
                let count = |key: &str| usage[key].as_i64().unwrap_or(0).max(0);
                events.push(Event::Usage(Usage {
                    input: count("inputTokens")
                        + count("cacheReadTokens")
                        + count("cacheWriteTokens"),
                    cached_input: count("cacheReadTokens"),
                    cache_write: count("cacheWriteTokens"),
                    output: count("outputTokens"),
                }));
            }
            Some("user") if !self.packet_echoed => self.packet_echoed = true,
            Some("user") => {
                if let Some(turn_id) = self.steered.pop_front() {
                    events.push(Event::Steered(turn_id));
                }
            }
            Some("summary") if message["status"] == "completed" => events.push(Event::Compacted),
            Some("system") => {
                if let Some(model) = message["model"]["id"].as_str()
                    && model != self.model
                {
                    return Err(Violation::profile(format!(
                        "Cursor used {model} instead of {}.",
                        self.model
                    )));
                }
            }
            _ => {}
        }
        Ok(events)
    }

    /// Check a tool call against the turn's tools; true when an else.bot
    /// tool's result parked the turn.
    fn on_tool_call(&mut self, call: &Value) -> Result<bool, Violation> {
        let name = call["name"]
            .as_str()
            .ok_or_else(|| Violation::policy("Cursor sent a malformed tool call"))?;
        if name != "mcp" {
            let host = HOST_TOOLS.contains(&name) || HOST_REPORTED.contains(&name);
            if self.host && host {
                return Ok(false);
            }
            return Err(Violation::policy(format!(
                "Cursor called a tool else.bot does not allow: {name}"
            )));
        }
        if !self.mcp {
            return Err(Violation::policy(
                "Cursor called an MCP tool on a turn without them",
            ));
        }
        // Updates before the call is parsed may not name it yet.
        let server = call["args"]["providerIdentifier"].as_str();
        let tool = call["args"]["toolName"].as_str();
        if let Some(server) = server.filter(|s| *s != SERVER_NAME) {
            return Err(Violation::policy(format!(
                "Cursor called an MCP tool outside else.bot: {server}"
            )));
        }
        if let Some(tool) = tool.filter(|t| !TOOL_NAMES.contains(t)) {
            return Err(Violation::policy(format!(
                "Cursor called an else.bot tool that does not exist: {tool}"
            )));
        }
        let (Some(tool), "completed") = (tool, call["status"].as_str().unwrap_or_default()) else {
            return Ok(false);
        };
        let id = call["call_id"].as_str().unwrap_or_default();
        let result = mcp_result(&call["result"]);
        Ok(crate::app::tools::parks(tool, &result) && self.parked_calls.insert(id.to_owned()))
    }
}

/// An SDK MCP result (`{status, value: {content: [{text: {text}}], isError}}`)
/// in the MCP shape else.bot's tools return.
fn mcp_result(result: &Value) -> Value {
    if result["status"] != "success" {
        return json!({ "isError": true });
    }
    let value = &result["value"];
    let content: Vec<Value> = value["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["text"]["text"].as_str())
        .map(|text| json!({ "type": "text", "text": text }))
        .collect();
    json!({ "content": content, "isError": value["isError"] == true })
}

impl Protocol for Tracker {
    type Message = Incoming;
    type Terminal = Terminal;

    fn on(&mut self, incoming: &Incoming) -> Result<Vec<Event<Terminal>>, Violation> {
        let Incoming::Notification { method, params } = incoming else {
            return Ok(Vec::new());
        };
        let mut events = Vec::new();
        if method != "run/message" {
            // The run took each acknowledged steer even when no echo came.
            events.extend(self.steered.drain(..).map(Event::Steered));
        }
        match method.as_str() {
            "run/message" => events.extend(self.on_message(&params["message"])?),
            "run/result" => events.push(Event::Finished(Terminal::Result {
                status: params["status"].as_str().unwrap_or("error").to_owned(),
                kind: params["kind"].as_str().map(str::to_owned),
                message: params["error"]["message"].as_str().map(str::to_owned),
            })),
            "run/lost" => events.push(Event::Finished(Terminal::Lost(
                params["message"].as_str().unwrap_or_default().to_owned(),
            ))),
            _ => {}
        }
        Ok(events)
    }

    fn outcome(&self, terminal: Terminal, _: Option<StopCause>, _: bool) -> TurnOutcome {
        let (status, kind, message) = match terminal {
            Terminal::Result {
                status,
                kind,
                message,
            } => (status, kind, message.unwrap_or_default()),
            Terminal::Lost(why) => {
                return TurnOutcome::Uncertain {
                    message: format!("Cursor's run stopped reporting without a result: {why}"),
                };
            }
        };
        match (status.as_str(), kind.as_deref()) {
            ("finished", _) => TurnOutcome::Completed,
            ("cancelled", _) => TurnOutcome::Interrupted,
            (_, Some("usage_limit")) => TurnOutcome::Failed {
                code: "provider_unavailable".into(),
                message: format!("Cursor's usage limit is reached: {message}"),
            },
            (_, Some("signed_out")) => TurnOutcome::Failed {
                code: "provider_unavailable".into(),
                message: format!(
                    "{} ({message})",
                    crate::providers::cursor_home::signed_out()
                ),
            },
            _ if message.is_empty() => TurnOutcome::Failed {
                code: "provider_failed".into(),
                message: "Cursor reported a failed run.".into(),
            },
            _ => TurnOutcome::Failed {
                code: "provider_failed".into(),
                message,
            },
        }
    }

    fn steer_input(&self, steer: &Steer) -> Option<Value> {
        Some(json!({ "text": prompt::segments_text(&prompt::steer_segments(steer)) }))
    }

    /// The SDK answers a steer only once the run holds it; the split in the
    /// reply waits for its echo.
    fn steer_sent(&mut self, steer: &Steer) {
        self.steered.push_back(steer.turn_id);
    }
}
