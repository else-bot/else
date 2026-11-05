use super::{MAX_LINE, failed};
use crate::app::providers::TurnOutcome;
use serde_json::Value;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ResultInfo {
    pub(super) success: bool,
    pub(super) message: String,
    /// Model turns the CLI ran; 0 means it ended before running any.
    pub(super) num_turns: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Step {
    Text(String),
    EndMessage,
    Accepted,
    Control(String),
    Parked,
    Result(ResultInfo),
}

pub(super) struct Tracker {
    expected_tools: Vec<String>,
    model: String,
    message_uuid: String,
    pub(super) init_seen: bool,
    accepted: bool,
    pub(super) violation: Option<(&'static str, String)>,
    tools_by_id: HashMap<String, String>,
    wrote_text: bool,
    separate: bool,
    pub(super) text: String,
    activity: bool,
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
            accepted: false,
            violation: None,
            tools_by_id: HashMap::new(),
            wrote_text: false,
            separate: false,
            text: String::new(),
            activity: false,
        }
    }

    fn flag(&mut self, code: &'static str, reason: String) {
        if self.violation.is_none() {
            self.violation = Some((code, reason));
        }
    }

    fn check_model(&mut self, model: Option<&str>) {
        if let Some(model) = model.filter(|m| *m != self.model) {
            let reason = format!("Claude used {model} instead of {}.", self.model);
            self.flag("profile_mismatch", reason);
        }
    }

    fn on_init(&mut self, event: &Value) {
        self.init_seen = true;
        self.check_model(Some(event["model"].as_str().unwrap_or("(none)")));
        let mut tools: Vec<String> = event["tools"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|t| t.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        tools.sort();
        if tools != self.expected_tools {
            let reason = format!("Claude offered unexpected tools: {tools:?}");
            self.flag("policy_violation", reason);
        }
        let servers: Vec<(String, String)> = event["mcp_servers"]
            .as_array()
            .map(|list| {
                list.iter()
                    .map(|s| {
                        (
                            s["name"].as_str().unwrap_or_default().to_owned(),
                            s["status"].as_str().unwrap_or_default().to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let ok = if self.expected_tools.is_empty() {
            servers.is_empty()
        } else {
            matches!(servers.as_slice(), [(name, status)]
                if name == crate::app::tools::SERVER_NAME && status == "connected")
        };
        if !ok {
            let reason = format!("Claude reported MCP servers {servers:?}");
            self.flag("policy_violation", reason);
        }
    }

    pub(super) fn on(&mut self, event: &Value) -> Vec<Step> {
        let mut steps = Vec::new();
        if event["parent_tool_use_id"].is_string() {
            self.flag(
                "policy_violation",
                "Claude ran a native sub-agent".to_owned(),
            );
            return steps;
        }
        match event["type"].as_str().unwrap_or_default() {
            "system" => match event["subtype"].as_str().unwrap_or_default() {
                "init" => self.on_init(event),
                sub if sub.starts_with("task_") || sub.contains("agent") => {
                    self.flag(
                        "policy_violation",
                        format!("Claude reported native agent activity ({sub})"),
                    );
                }
                _ => {}
            },
            "user" => {
                if !self.accepted
                    && event["uuid"].as_str() == Some(self.message_uuid.as_str())
                    && event["isReplay"].as_bool() == Some(true)
                {
                    self.accepted = true;
                    steps.push(Step::Accepted);
                }
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] != "tool_result" {
                        continue;
                    }
                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                    let prefix = format!("mcp__{}__", crate::app::tools::SERVER_NAME);
                    let parks = self.tools_by_id.get(id).is_some_and(|name| {
                        name.strip_prefix(&prefix)
                            .is_some_and(|t| crate::app::tools::parks(t, &block["content"]))
                    });
                    if parks {
                        steps.push(Step::Parked);
                    }
                }
            }
            "stream_event" => {
                self.activity = true;
                let inner = &event["event"];
                match inner["type"].as_str().unwrap_or_default() {
                    "message_start" => self.check_model(inner["message"]["model"].as_str()),
                    "content_block_start" if inner["content_block"]["type"] == "text" => {
                        self.separate = self.wrote_text;
                    }
                    "content_block_delta" if inner["delta"]["type"] == "text_delta" => {
                        if let Some(text) = inner["delta"]["text"].as_str() {
                            if self.separate {
                                self.separate = false;
                                steps.push(Step::EndMessage);
                                self.text.push_str("\n\n");
                            }
                            self.wrote_text = true;
                            self.text.push_str(text);
                            steps.push(Step::Text(text.to_owned()));
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                self.activity = true;
                self.check_model(event["message"]["model"].as_str());
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "tool_use" {
                        let name = block["name"].as_str().unwrap_or_default().to_owned();
                        if !self.expected_tools.contains(&name) {
                            self.flag(
                                "policy_violation",
                                format!(
                                    "Claude called a tool outside else.bot's allowlist: {name}"
                                ),
                            );
                        }
                        let id = block["id"].as_str().unwrap_or_default().to_owned();
                        self.tools_by_id.insert(id, name);
                    }
                }
            }
            "control_request" => {
                let request = &event["request"];
                let what = request["tool_name"]
                    .as_str()
                    .or(request["subtype"].as_str())
                    .unwrap_or("?")
                    .to_owned();
                self.flag(
                    "policy_violation",
                    format!("Claude asked for permission: {what}"),
                );
                steps.push(Step::Control(
                    event["request_id"].as_str().unwrap_or_default().to_owned(),
                ));
            }
            "result" => {
                let success =
                    event["subtype"] == "success" && event["is_error"].as_bool() != Some(true);
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
                steps.push(Step::Result(ResultInfo {
                    success,
                    message,
                    num_turns: event["num_turns"].as_i64(),
                }));
            }
            _ => {}
        }
        steps
    }
}

/// Reads bounded event lines from the CLI. Partially read bytes stay in
/// `buf`, so a read abandoned by a `select!` (a timer or cancellation) loses
/// nothing: the next call continues the same line.
pub(super) struct Events<R> {
    reader: R,
    buf: Vec<u8>,
}

impl<R: tokio::io::AsyncBufRead + Unpin> Events<R> {
    pub(super) fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
        }
    }

    pub(super) async fn next(&mut self) -> Result<Option<Value>, String> {
        loop {
            let room = (MAX_LINE as usize).saturating_sub(self.buf.len()) as u64;
            if room == 0 {
                return Err("Claude sent an oversized event".into());
            }
            let read = (&mut self.reader)
                .take(room)
                .read_until(b'\n', &mut self.buf)
                .await
                .map_err(|e| e.to_string())?;
            if read == 0 && self.buf.is_empty() {
                return Ok(None);
            }
            if read != 0 && !self.buf.ends_with(b"\n") {
                continue;
            }
            let line = std::mem::take(&mut self.buf);
            if line.iter().all(u8::is_ascii_whitespace) {
                if read == 0 {
                    return Ok(None);
                }
                continue;
            }
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| format!("Claude sent malformed output: {e}"));
        }
    }
}

/// The turn's outcome from its terminal `result` and what was observed.
pub(super) fn decide(
    tracker: &Tracker,
    info: &ResultInfo,
    interrupted: bool,
    deadline_hit: bool,
) -> TurnOutcome {
    if let Some((code, reason)) = tracker.violation.clone() {
        return failed(code, reason);
    }
    if !tracker.accepted {
        let refused = !info.success
            && !interrupted
            && !tracker.activity
            && !tracker.accepted
            && info.num_turns == Some(0);
        return if refused {
            failed("provider_refused", info.message.clone())
        } else {
            TurnOutcome::Uncertain {
                message: "Claude ended without acknowledging the message.".into(),
            }
        };
    }
    if !tracker.init_seen {
        return failed(
            "policy_violation",
            "Claude did not report its tool set and model.",
        );
    }
    if deadline_hit {
        failed("turn_deadline", "The turn exceeded its 30 minute deadline.")
    } else if info.success {
        TurnOutcome::Completed
    } else if interrupted {
        TurnOutcome::Interrupted
    } else {
        failed("provider_failed", info.message.clone())
    }
}

pub(super) fn event_model(event: &Value) -> Option<&str> {
    event["message"]["model"].as_str()
}
