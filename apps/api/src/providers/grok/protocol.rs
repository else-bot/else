use serde_json::Value;
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};

use super::grok_home;
use crate::app::providers::TurnOutcome;

pub(super) struct Tracker {
    model: String,
    session: String,
    expected: Vec<String>,
    mcp: bool,
    pub initialized: bool,
    pub accepted: bool,
    calls: HashMap<String, String>,
}

#[derive(Debug, PartialEq)]
pub(super) enum Step {
    Accepted,
    Text(String),
    EndMessage,
    Parked,
    Result(TurnOutcome),
}

impl Tracker {
    pub fn new(model: &str, session: &str, host: bool, mcp: bool) -> Self {
        let mut expected: Vec<String> = grok_home::MCP_TOOLS.iter().map(|s| (*s).into()).collect();
        if host {
            expected.extend(grok_home::HOST_TOOL_NAMES.iter().map(|s| (*s).into()));
        }
        expected.sort();
        Self {
            model: model.into(),
            session: session.into(),
            expected,
            mcp,
            initialized: false,
            accepted: false,
            calls: HashMap::new(),
        }
    }

    fn mcp_tool<'a>(&self, name: &'a str) -> Option<&'a str> {
        name.strip_prefix("elsebot__")
            .filter(|tool| self.mcp && crate::app::tools::TOOL_NAMES.contains(tool))
    }

    pub fn on(&mut self, event: &Value) -> Result<Vec<Step>, String> {
        let mut steps = Vec::new();
        if event["parent_tool_use_id"].is_string() {
            return Err("Grok ran a native subagent".into());
        }
        if event["session_id"]
            .as_str()
            .is_some_and(|id| id != self.session)
        {
            return Err("Grok reported a different native session".into());
        }
        for model in [
            event["model"].as_str(),
            event["message"]["model"].as_str(),
            event["event"]["message"]["model"].as_str(),
        ]
        .into_iter()
        .flatten()
        {
            if model != self.model {
                return Err(format!("Grok used {model} instead of {}", self.model));
            }
        }
        match event["type"].as_str().unwrap_or_default() {
            "system" if event["subtype"] == "init" => {
                if self.initialized
                    || event["model"] != self.model
                    || event["session_id"] != self.session
                {
                    return Err("Grok did not initialize the requested model and session".into());
                }
                let mut tools: Vec<_> = event["tools"]
                    .as_array()
                    .ok_or("Grok omitted its tool inventory")?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect();
                tools.retain(|tool| self.mcp_tool(tool).is_none());
                tools.sort();
                if tools != self.expected {
                    return Err(format!("Grok offered unexpected tools: {tools:?}"));
                }
                let servers = event["mcp_servers"]
                    .as_array()
                    .ok_or("Grok omitted its MCP inventory")?;
                let valid = if self.mcp {
                    matches!(servers.as_slice(), [server] if server["name"] == "elsebot" && matches!(server["status"].as_str(), Some("connected" | "pending")))
                } else {
                    servers.is_empty()
                };
                if !valid {
                    return Err(format!("Grok reported unexpected MCP servers: {servers:?}"));
                }
                self.initialized = true;
            }
            "stream_event" => {
                if !self.initialized {
                    return Err("Grok produced output before its inventory".into());
                }
                let inner = &event["event"];
                match inner["type"].as_str().unwrap_or_default() {
                    "message_start" => {
                        if !self.accepted {
                            self.accepted = true;
                            steps.push(Step::Accepted);
                        }
                    }
                    "content_block_delta" if inner["delta"]["type"] == "text_delta" => {
                        if let Some(text) = inner["delta"]["text"].as_str() {
                            steps.push(Step::Text(text.into()));
                        }
                    }
                    "message_stop" => steps.push(Step::EndMessage),
                    _ => {}
                }
            }
            "assistant" => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if block["type"] == "server_tool_use" {
                        return Err("Grok used a backend tool".into());
                    }
                    if block["type"] != "tool_use" {
                        continue;
                    }
                    let name = block["name"].as_str().unwrap_or_default();
                    let direct = self.mcp_tool(name);
                    if direct.is_none() && !self.expected.iter().any(|t| t == name) {
                        return Err(format!("Grok called an unexpected tool: {name}"));
                    }
                    let tool = if name == "use_tool" {
                        Some(
                            self.mcp_tool(block["input"]["tool_name"].as_str().unwrap_or_default())
                                .ok_or("Grok called an MCP tool outside else.bot")?,
                        )
                    } else {
                        direct
                    };
                    if let Some(tool) = tool {
                        self.calls
                            .insert(block["id"].as_str().unwrap_or_default().into(), tool.into());
                    }
                }
            }
            "user" => {
                for block in event["message"]["content"].as_array().into_iter().flatten() {
                    if let Some(tool) = block["tool_use_id"]
                        .as_str()
                        .and_then(|id| self.calls.get(id))
                        && crate::app::tools::parks(tool, &block["content"])
                    {
                        steps.push(Step::Parked);
                    }
                }
            }
            "control_request" => return Err("Grok requested interactive permission".into()),
            "system"
                if event["subtype"]
                    .as_str()
                    .is_some_and(|s| s.starts_with("task_") || s.contains("agent")) =>
            {
                return Err("Grok reported native agent activity".into());
            }
            "result" => {
                let outcome = if !self.initialized || !self.accepted {
                    TurnOutcome::Uncertain {
                        message: format!(
                            "Grok ended without acknowledging the prompt: {}",
                            event["errors"]
                        ),
                    }
                } else if event["subtype"] == "success"
                    && event["is_error"] == false
                    && event["stop_reason"] == "end_turn"
                {
                    TurnOutcome::Completed
                } else if event["stop_reason"] == "cancelled" {
                    TurnOutcome::Interrupted
                } else {
                    TurnOutcome::Failed {
                        code: "provider_failed".into(),
                        message: format!(
                            "Grok ended with {}: {}",
                            event["stop_reason"], event["errors"]
                        ),
                    }
                };
                steps.push(Step::Result(outcome));
            }
            "error" => steps.push(Step::Result(TurnOutcome::Failed {
                code: "provider_failed".into(),
                message: event["message"].as_str().unwrap_or("Grok failed").into(),
            })),
            _ => {}
        }
        Ok(steps)
    }
}

pub(super) struct Events<R> {
    reader: R,
    buf: Vec<u8>,
}
impl<R: tokio::io::AsyncBufRead + Unpin> Events<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            buf: Vec::new(),
        }
    }
    pub async fn next(&mut self) -> Result<Option<Value>, String> {
        loop {
            let room = (8 * 1024 * 1024usize).saturating_sub(self.buf.len());
            if room == 0 {
                return Err("Grok sent an oversized event".into());
            }
            let n = (&mut self.reader)
                .take(room as u64)
                .read_until(b'\n', &mut self.buf)
                .await
                .map_err(|e| e.to_string())?;
            if n == 0 && self.buf.is_empty() {
                return Ok(None);
            }
            if n != 0 && !self.buf.ends_with(b"\n") {
                continue;
            }
            let line = std::mem::take(&mut self.buf);
            if line.iter().all(u8::is_ascii_whitespace) {
                if n == 0 {
                    return Ok(None);
                }
                continue;
            }
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| format!("Grok sent malformed output: {e}"));
        }
    }
}
