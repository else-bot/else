//! Claude Code's `stream-json` events, validated against the turn's policy.

use std::collections::HashMap;

use serde_json::Value;

use crate::app::providers::TurnOutcome;
use crate::app::tools::SERVER_NAME;
use crate::providers::cli::attempt::{Event, Protocol, StopCause, Violation};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResultInfo {
    pub(super) success: bool,
    pub(super) message: String,
    /// Model turns the CLI ran; 0 means it ended before running any.
    pub(super) num_turns: Option<i64>,
}

pub(super) struct Tracker {
    expected_tools: Vec<String>,
    model: String,
    message_uuid: String,
    pub(super) init_seen: bool,
    tools_by_id: HashMap<String, String>,
    wrote_text: bool,
    separate: bool,
    pub(super) text: String,
    activity: bool,
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
            tools_by_id: HashMap::new(),
            wrote_text: false,
            separate: false,
            text: String::new(),
            activity: false,
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
        if text(&event["uuid"], "user message")? == Some(self.message_uuid.as_str())
            && event["isReplay"].as_bool() == Some(true)
        {
            events.push(Event::Accepted {
                model: self.model.clone(),
                native_turn: self.message_uuid.clone(),
            });
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
            let parks = self.tools_by_id.get(id).is_some_and(|name| {
                name.strip_prefix(&prefix)
                    .is_some_and(|t| crate::app::tools::parks(t, &block["content"]))
            });
            if parks {
                events.push(Event::Parked);
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

    fn on_assistant(&mut self, event: &Value) -> Result<(), Violation> {
        self.activity = true;
        self.check_model(text(&event["message"]["model"], "model")?)?;
        for block in list(&event["message"]["content"], "assistant message")? {
            if block["type"] != "tool_use" {
                continue;
            }
            let name = required(&block["name"], "tool call")?;
            if !self.expected_tools.iter().any(|t| t == name) {
                return Err(Violation::policy(format!(
                    "Claude called a tool outside else.bot's allowlist: {name}"
                )));
            }
            let id = required(&block["id"], "tool call")?;
            self.tools_by_id.insert(id.to_owned(), name.to_owned());
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

/// The turn's token totals from the result event. Claude counts cache
/// reads and writes apart from `input_tokens`.
fn result_usage(event: &Value) -> Option<crate::app::providers::Usage> {
    let usage = event.get("usage")?;
    let count = |key: &str| usage[key].as_i64().unwrap_or(0).max(0);
    let cached_input = count("cache_read_input_tokens");
    let cache_write = count("cache_creation_input_tokens");
    Some(crate::app::providers::Usage {
        input: count("input_tokens") + cached_input + cache_write,
        cached_input,
        cache_write,
        output: count("output_tokens"),
    })
}

impl Protocol for Tracker {
    type Message = Value;
    type Terminal = ResultInfo;

    fn on(&mut self, event: &Value) -> Result<Vec<Event<ResultInfo>>, Violation> {
        if text(&event["parent_tool_use_id"], "parent tool use")?.is_some() {
            return Err(Violation::policy("Claude ran a native sub-agent"));
        }
        let mut events = Vec::new();
        match required(&event["type"], "event")? {
            "system" => match text(&event["subtype"], "system event")?.unwrap_or_default() {
                "init" => self.on_init(event)?,
                "compact_boundary" => events.push(Event::Compacted),
                sub if sub.starts_with("task_") || sub.contains("agent") => {
                    return Err(Violation::policy(format!(
                        "Claude reported native agent activity ({sub})"
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
            "result" => {
                events.extend(result_usage(event).map(Event::Usage));
                events.push(Event::Finished(result_info(event)));
            }
            _ => {}
        }
        Ok(events)
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
