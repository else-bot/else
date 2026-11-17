//! Claude Code's `stream-json` events, validated against the turn's policy.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::app::providers::{LimitWindow, Steer, TurnOutcome, Usage};
use crate::app::tools::SERVER_NAME;
use crate::providers::cli::attempt::{Event, Protocol, StopCause, Violation};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResultInfo {
    pub(super) success: bool,
    pub(super) message: String,
    /// Model turns the CLI ran; 0 means it ended before running any.
    pub(super) num_turns: Option<i64>,
}

/// A tool call the model made. The CLI answers a name outside its verified
/// inventory itself, with an error result and without running anything.
enum Call {
    Offered(String),
    Unoffered(String),
}

pub(super) struct Tracker {
    expected_tools: Vec<String>,
    model: String,
    message_uuid: String,
    pub(super) init_seen: bool,
    calls: HashMap<String, Call>,
    wrote_text: bool,
    separate: bool,
    pub(super) text: String,
    activity: bool,
    /// Steered turns whose message was written to the CLI and not echoed
    /// yet. Each message's UUID is its turn's ID.
    steers: HashSet<uuid::Uuid>,
}

fn malformed(what: &str) -> Violation {
    Violation::policy(format!("Claude sent a malformed {what}"))
}

fn text<'v>(value: &'v Value, what: &str) -> Result<Option<&'v str>, Violation> {
    match value {
        Value::Null => Ok(None),
        Value::String(text) => Ok(Some(text)),
        _ => Err(malformed(what)),
    }
}

fn required<'v>(value: &'v Value, what: &str) -> Result<&'v str, Violation> {
    text(value, what)?.ok_or_else(|| malformed(what))
}

fn list<'v>(value: &'v Value, what: &str) -> Result<&'v [Value], Violation> {
    match value {
        Value::Null => Ok(&[]),
        Value::Array(items) => Ok(items),
        _ => Err(malformed(what)),
    }
}

fn inventory<'v>(value: &'v Value, what: &str) -> Result<&'v [Value], Violation> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| malformed(what))
}

impl Tracker {
    pub(super) fn new(expected_tools: Vec<String>, model: &str, message_uuid: &str) -> Self {
        let mut expected_tools = expected_tools;
        expected_tools.sort();
        Self {
            expected_tools,
            model: model.to_owned(),
            message_uuid: message_uuid.to_owned(),
            init_seen: false,
            calls: HashMap::new(),
            wrote_text: false,
            separate: false,
            text: String::new(),
            activity: false,
            steers: HashSet::new(),
        }
    }

    fn check_model(&self, model: Option<&str>) -> Result<(), Violation> {
        match model {
            Some(model) if model != self.model => Err(Violation::profile(format!(
                "Claude used {model} instead of {}.",
                self.model
            ))),
            _ => Ok(()),
        }
    }

    fn on_init(&mut self, event: &Value) -> Result<(), Violation> {
        self.init_seen = true;
        self.check_model(Some(text(&event["model"], "model")?.unwrap_or("(none)")))?;
        let mut tools = inventory(&event["tools"], "tool inventory")?
            .iter()
            .map(|tool| required(tool, "tool inventory").map(str::to_owned))
            .collect::<Result<Vec<_>, _>>()?;
        tools.sort();
        if tools != self.expected_tools {
            return Err(Violation::policy(format!(
                "Claude offered unexpected tools: {tools:?}"
            )));
        }
        let servers = inventory(&event["mcp_servers"], "MCP inventory")?
            .iter()
            .map(|server| {
                Ok((
                    required(&server["name"], "MCP inventory")?,
                    required(&server["status"], "MCP inventory")?,
                ))
            })
            .collect::<Result<Vec<_>, Violation>>()?;
        let ok = if self.expected_tools.is_empty() {
            servers.is_empty()
        } else {
            matches!(servers.as_slice(), [(name, "connected")] if *name == SERVER_NAME)
        };
        if !ok {
            return Err(Violation::policy(format!(
                "Claude reported MCP servers {servers:?}"
            )));
        }
        Ok(())
    }

    fn on_user(
        &mut self,
        event: &Value,
        events: &mut Vec<Event<ResultInfo>>,
    ) -> Result<(), Violation> {
        let uuid = text(&event["uuid"], "user message")?;
        if event["isReplay"].as_bool() == Some(true) {
            if uuid == Some(self.message_uuid.as_str()) {
                events.push(Event::Accepted {
                    model: self.model.clone(),
                    native_turn: self.message_uuid.clone(),
                });
            } else if let Some(turn) = uuid
                .and_then(|uuid| uuid.parse().ok())
                .filter(|turn| self.steers.remove(turn))
            {
                events.push(Event::Steered(turn));
            }
        }
        let prefix = format!("mcp__{SERVER_NAME}__");
        let blocks = match &event["message"]["content"] {
            Value::String(_) => &[],
            content => list(content, "user message")?,
        };
        for block in blocks {
            if block["type"] != "tool_result" {
                continue;
            }
            let id = required(&block["tool_use_id"], "tool result")?;
            match self.calls.get(id) {
                Some(Call::Offered(name))
                    if name
                        .strip_prefix(&prefix)
                        .is_some_and(|t| crate::app::tools::parks(t, &block["content"])) =>
                {
                    events.push(Event::Parked);
                }
                Some(Call::Unoffered(name)) if block["is_error"].as_bool() != Some(true) => {
                    return Err(Violation::policy(format!(
                        "Claude ran a tool outside else.bot's allowlist: {name}"
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn on_stream(
        &mut self,
        event: &Value,
        events: &mut Vec<Event<ResultInfo>>,
    ) -> Result<(), Violation> {
        self.activity = true;
        let inner = &event["event"];
        match required(&inner["type"], "stream event")? {
            "message_start" => self.check_model(text(&inner["message"]["model"], "model")?)?,
            "content_block_start" if inner["content_block"]["type"] == "text" => {
                self.separate = self.wrote_text;
            }
            "content_block_delta" if inner["delta"]["type"] == "text_delta" => {
                let delta = required(&inner["delta"]["text"], "text delta")?;
                if self.separate {
                    self.separate = false;
                    events.push(Event::EndMessage);
                    self.text.push_str("\n\n");
                }
                self.wrote_text = true;
                self.text.push_str(delta);
                events.push(Event::Text(delta.to_owned()));
            }
            _ => {}
        }
        Ok(())
    }

    /// The CLI's progress heartbeat for a slow tool call is parented to that
    /// call. A sub-agent's events are parented to its `Agent` call, which is
    /// never offered.
    fn is_heartbeat(&self, event: &Value, parent: &str) -> Result<bool, Violation> {
        if event["type"] != "tool_progress" {
            return Ok(false);
        }
        let tool = required(&event["tool_name"], "tool progress")?;
        Ok(matches!(self.calls.get(parent), Some(Call::Offered(name)) if name == tool))
    }

    /// The CLI reports a failed API call, such as an expired login, as an
    /// assistant message of its own with the placeholder model `<synthetic>`.
    /// The result event that follows carries the same error.
    fn on_assistant(&mut self, event: &Value) -> Result<(), Violation> {
        self.activity = true;
        if event["is_api_error_message"].as_bool() != Some(true) {
            self.check_model(text(&event["message"]["model"], "model")?)?;
        }
        for block in list(&event["message"]["content"], "assistant message")? {
            if block["type"] != "tool_use" {
                continue;
            }
            let name = required(&block["name"], "tool call")?.to_owned();
            let id = required(&block["id"], "tool call")?;
            let call = if self.expected_tools.contains(&name) {
                Call::Offered(name)
            } else {
                Call::Unoffered(name)
            };
            self.calls.insert(id.to_owned(), call);
        }
        Ok(())
    }
}

fn result_info(event: &Value) -> ResultInfo {
    let success = event["subtype"] == "success" && event["is_error"].as_bool() != Some(true);
    let message = event["result"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            event["errors"].as_array().map(|errors| {
                errors
                    .iter()
                    .filter_map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            })
        })
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| {
            format!(
                "Claude ended the turn: {}",
                event["subtype"].as_str().unwrap_or("unknown")
            )
        });
    ResultInfo {
        success,
        message,
        num_turns: event["num_turns"].as_i64(),
    }
}

/// The 5-hour and weekly windows of a `rate_limit_event`. The CLI reports
/// each window's utilization as a fraction and its reset in Unix seconds.
fn limit_windows(event: &Value) -> Vec<LimitWindow> {
    let windows = &event["rate_limit_info"]["unifiedWindows"];
    [
        ("five_hour", LimitWindow::FIVE_HOURS),
        ("seven_day", LimitWindow::WEEK),
    ]
    .into_iter()
    .filter_map(|(key, minutes)| {
        let window = &windows[key];
        let used = window["utilization"].as_f64()?;
        Some(LimitWindow {
            minutes,
            used_basis_points: (used * 10_000.0).round() as i64,
            resets_at: chrono::DateTime::from_timestamp(window["resetsAt"].as_i64()?, 0)?,
        })
    })
    .collect()
}

impl Protocol for Tracker {
    type Message = Value;
    type Terminal = ResultInfo;

    fn on(&mut self, event: &Value) -> Result<Vec<Event<ResultInfo>>, Violation> {
        if let Some(parent) = text(&event["parent_tool_use_id"], "parent tool use")? {
            if self.is_heartbeat(event, parent)? {
                return Ok(Vec::new());
            }
            return Err(Violation::policy("Claude ran a native sub-agent"));
        }
        let mut events = Vec::new();
        match required(&event["type"], "event")? {
            "system" => match text(&event["subtype"], "system event")?.unwrap_or_default() {
                "init" => self.on_init(event)?,
                "compact_boundary" => events.push(Event::Compacted),
                sub if sub.starts_with("task_") || sub.contains("agent") => {
                    let detail = ["task_type", "description"]
                        .iter()
                        .filter_map(|key| event[key].as_str())
                        .map(|value| format!(", {value}"))
                        .collect::<String>();
                    return Err(Violation::policy(format!(
                        "Claude reported native agent activity ({sub}{detail})"
                    )));
                }
                _ => {}
            },
            "user" => self.on_user(event, &mut events)?,
            "stream_event" => self.on_stream(event, &mut events)?,
            "assistant" => self.on_assistant(event)?,
            "control_request" => {
                let request = &event["request"];
                let what = request["tool_name"]
                    .as_str()
                    .or(request["subtype"].as_str())
                    .unwrap_or("?");
                return Err(Violation::policy(format!(
                    "Claude asked for permission: {what}"
                )));
            }
            "rate_limit_event" => {
                let windows = limit_windows(event);
                if !windows.is_empty() {
                    events.push(Event::Limits(windows));
                }
            }
            "result" => {
                events.extend(Usage::from_result(event).map(Event::Usage));
                events.push(Event::Finished(result_info(event)));
            }
            _ => {}
        }
        Ok(events)
    }

    /// A steered message is a user line of its own, with its images inline
    /// as in a first message. The CLI takes it at the next tool boundary, or
    /// as one more native turn after the current result, and echoes it
    /// either way.
    fn steer_input(&self, steer: &Steer) -> Option<Value> {
        Some(super::user_value(
            &steer.turn_id.to_string(),
            super::message_content(&crate::providers::prompt::steer_segments(steer)),
        ))
    }

    fn steer_sent(&mut self, steer: &Steer) {
        self.steers.insert(steer.turn_id);
    }

    /// A result before a written steer's echo means the CLI will answer it
    /// in one more native turn.
    fn continues(&self, info: &ResultInfo) -> bool {
        info.success && !self.steers.is_empty()
    }

    fn outcome(
        &self,
        info: ResultInfo,
        stopping: Option<StopCause>,
        accepted: bool,
    ) -> TurnOutcome {
        if !accepted {
            let refused =
                !info.success && stopping.is_none() && !self.activity && info.num_turns == Some(0);
            return if refused {
                failed("provider_refused", info.message)
            } else {
                TurnOutcome::Uncertain {
                    message: "Claude ended without acknowledging the message.".into(),
                }
            };
        }
        if !self.init_seen {
            return failed(
                "policy_violation",
                "Claude did not report its tool set and model.",
            );
        }
        if info.success {
            TurnOutcome::Completed
        } else if stopping.is_some() {
            TurnOutcome::Interrupted
        } else {
            failed("provider_failed", info.message)
        }
    }
}

fn failed(code: &str, message: impl Into<String>) -> TurnOutcome {
    TurnOutcome::Failed {
        code: code.to_owned(),
        message: message.into(),
    }
}

/// Byte cap for the CLI's output before it answers `initialize`; the answer
/// itself lists every skill and command, so it is large.
pub const MODELS_LIMIT: usize = 4 * 1024 * 1024;

/// The `initialize` control request, whose answer lists the CLI's models.
pub fn initialize_request(id: &str) -> String {
    serde_json::json!({
        "type": "control_request",
        "request_id": id,
        "request": { "subtype": "initialize" },
    })
    .to_string()
}

/// The models in the answer to `initialize` request `id`, by the exact ID
/// each resolves to: `fable` and `default` are aliases that move. A model
/// without effort levels lists none, so the adapter, which always sends
/// one, cannot run it. `None` for any other line.
pub fn listed_models(
    line: &str,
    id: &str,
) -> Option<Result<Vec<crate::providers::models::Listed>, String>> {
    let message: Value = serde_json::from_str(line).ok()?;
    if message["type"] != "control_response" || message["response"]["request_id"] != id {
        return None;
    }
    let response = &message["response"];
    if response["subtype"] != "success" {
        let error = response["error"].as_str().unwrap_or("initialize failed");
        return Some(Err(error.to_owned()));
    }
    let Some(models) = response["response"]["models"].as_array() else {
        return Some(Err("its answer lists no models".into()));
    };
    let strings = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    };
    let listed = models
        .iter()
        .filter_map(|model| {
            let id = model["resolvedModel"]
                .as_str()
                .or(model["value"].as_str())?;
            let efforts = if model["supportsEffort"] == true {
                strings(&model["supportedEffortLevels"])
            } else {
                Vec::new()
            };
            Some(crate::providers::models::Listed {
                id: id.to_owned(),
                efforts: Some(efforts),
                tiers: None,
            })
        })
        .collect();
    Some(Ok(listed))
}
