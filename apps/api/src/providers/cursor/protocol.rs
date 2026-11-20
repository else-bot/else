//! Cursor CLI's `stream-json` events, validated against the turn's policy.
//!
//! `--allowed-tools` limits what the model is offered; each call is still
//! checked when it starts. The CLI writes a `result` only on success and
//! otherwise just exits; [`super::Running`] reports how it exited.
//!
//! With `--stream-partial-output` each text delta arrives as an `assistant`
//! event with a timestamp, and the text since the last flush arrives again
//! before each tool call, retry, question and the result. A flush has a
//! `model_call_id` or no timestamp, except when its model call has no ID;
//! such an event that repeats the text since the last flush is held until
//! the next event shows which it was.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::app::providers::{FailureCode, TurnOutcome, Usage};
use crate::app::tools::{SERVER_NAME, TOOL_NAMES};
use crate::providers::cli::attempt::{Event, Protocol, StopCause, Violation};

/// The host shell and file tools an owner's turn may use, as
/// `--allowed-tools` names them and as their calls report them.
const HOST_TOOLS: &[(&str, &str)] = &[
    ("shell_tool_call", "shellToolCall"),
    ("read_tool_call", "readToolCall"),
    ("edit_tool_call", "editToolCall"),
    ("delete_tool_call", "deleteToolCall"),
    ("grep_tool_call", "grepToolCall"),
    ("glob_tool_call", "globToolCall"),
    ("ls_tool_call", "lsToolCall"),
    ("sem_search_tool_call", "semSearchToolCall"),
    ("read_lints_tool_call", "readLintsToolCall"),
];
/// The agent's own to-do list, which touches nothing outside the turn.
const OWN_TOOLS: &[(&str, &str)] = &[
    ("update_todos_tool_call", "updateTodosToolCall"),
    ("read_todos_tool_call", "readTodosToolCall"),
];
const MCP_TOOLS: &[(&str, &str)] = &[
    ("mcp_tool_call", "mcpToolCall"),
    ("get_mcp_tools_tool_call", "getMcpToolsToolCall"),
];

/// The `--allowed-tools` value for a turn.
pub(super) fn allowed_tools(host: bool, mcp: bool) -> String {
    let mut tools = OWN_TOOLS.to_vec();
    if host {
        tools.extend(HOST_TOOLS);
    }
    if mcp {
        tools.extend(MCP_TOOLS);
    }
    let names: Vec<&str> = tools.iter().map(|(flag, _)| *flag).collect();
    names.join(",")
}

fn reports(tools: &[(&str, &str)], kind: &str) -> bool {
    tools.iter().any(|(_, call)| *call == kind)
}

/// How the CLI's turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Terminal {
    Result {
        success: bool,
        text: String,
    },
    /// The CLI exited without a result: its exit code, or the signal that
    /// ended it, and the tail of its stderr.
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
        stderr: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    Pending,
    Opened,
    Accepted,
}

pub(super) struct Tracker {
    session: String,
    model: String,
    host: bool,
    mcp: bool,
    start: Start,
    since_flush: String,
    /// An event that is a delta unless a flush boundary follows it.
    held: Option<String>,
    calls: HashMap<String, String>,
    /// Whether a failure may have left effects: a tool call started, or an
    /// owner's turn took the prompt, when the project's hooks may run.
    acted: bool,
    /// Whether the CLI exited cleanly after its result, so its chat is
    /// stored.
    clean: bool,
}

fn malformed(what: &str) -> Violation {
    Violation::policy(format!("Cursor sent a malformed {what}"))
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

/// The one `<kind>ToolCall` entry of a reported tool call.
fn tool_call(event: &Value) -> Result<(&str, &Value), Violation> {
    match event["tool_call"]
        .as_object()
        .map(|o| o.iter().collect::<Vec<_>>())
    {
        Some(entries) if entries.len() == 1 => Ok((entries[0].0.as_str(), entries[0].1)),
        _ => Err(malformed("tool call")),
    }
}

/// An MCP result in the shape [`crate::app::tools::parks`] reads.
fn mcp_result(result: &Value) -> Value {
    let success = &result["success"];
    let content: Vec<Value> = success["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["text"]["text"].as_str())
        .map(|text| json!({ "type": "text", "text": text }))
        .collect();
    json!({ "content": content, "isError": success.is_null() || success["isError"] == true })
}

impl Tracker {
    pub fn new(model: &str, session: &str, host: bool, mcp: bool) -> Self {
        Self {
            session: session.into(),
            model: model.into(),
            host,
            mcp,
            start: Start::Pending,
            since_flush: String::new(),
            held: None,
            calls: HashMap::new(),
            acted: false,
            clean: false,
        }
    }

    fn on_assistant(&mut self, event: &Value) -> Result<Vec<Event<Terminal>>, Violation> {
        let mut chunk = String::new();
        let blocks = event["message"]["content"]
            .as_array()
            .ok_or_else(|| malformed("assistant message"))?;
        for block in blocks {
            if required(&block["type"], "content block")? == "text" {
                chunk.push_str(required(&block["text"], "text block")?);
            }
        }
        let flush = event.get("timestamp_ms").is_none() || event.get("model_call_id").is_some();
        if flush {
            self.since_flush.clear();
            return Ok(vec![Event::EndMessage]);
        }
        if !self.since_flush.is_empty() && chunk == self.since_flush {
            self.held = Some(chunk);
            return Ok(Vec::new());
        }
        self.since_flush.push_str(&chunk);
        Ok(vec![Event::Text(chunk)])
    }

    fn release(&mut self, kind: &str) -> Vec<Event<Terminal>> {
        let Some(chunk) = self.held.take() else {
            return Vec::new();
        };
        if matches!(kind, "retry" | "tool_call" | "interaction_query") {
            self.since_flush.clear();
            return vec![Event::EndMessage];
        }
        self.since_flush.push_str(&chunk);
        vec![Event::Text(chunk)]
    }

    fn on_tool_call(&mut self, event: &Value) -> Result<Vec<Event<Terminal>>, Violation> {
        let id = required(&event["call_id"], "tool call")?;
        let (kind, call) = tool_call(event)?;
        match required(&event["subtype"], "tool call")? {
            "started" => {
                self.acted = true;
                let args = &call["args"];
                if kind == "mcpToolCall" {
                    let server = required(&args["providerIdentifier"], "MCP call")?;
                    let tool = required(&args["toolName"], "MCP call")?;
                    if !self.mcp || server != SERVER_NAME || !TOOL_NAMES.contains(&tool) {
                        return Err(Violation::policy(format!(
                            "Cursor called an MCP tool outside else.bot: {server}:{tool}"
                        )));
                    }
                    self.calls.insert(id.to_owned(), tool.to_owned());
                } else if kind == "getMcpToolsToolCall" {
                    let server = text(&args["server"], "MCP listing")?;
                    if !self.mcp || server.is_some_and(|server| server != SERVER_NAME) {
                        return Err(Violation::policy(format!(
                            "Cursor listed MCP tools outside else.bot: {}",
                            server.unwrap_or_default()
                        )));
                    }
                } else if !(reports(OWN_TOOLS, kind) || self.host && reports(HOST_TOOLS, kind)) {
                    return Err(Violation::policy(format!(
                        "Cursor called a tool this turn does not allow: {kind}"
                    )));
                }
                Ok(Vec::new())
            }
            "completed" => {
                let parked = self.calls.get(id).is_some_and(|tool| {
                    crate::app::tools::parks(tool, &mcp_result(&call["result"]))
                });
                Ok(if parked {
                    vec![Event::Parked]
                } else {
                    Vec::new()
                })
            }
            _ => Ok(Vec::new()),
        }
    }
}

impl Protocol for Tracker {
    type Message = Value;
    type Terminal = Terminal;

    fn on(&mut self, event: &Value) -> Result<Vec<Event<Terminal>>, Violation> {
        let kind = required(&event["type"], "event")?;
        if kind == "exit" {
            let mut events = self.release(kind);
            let number = |key: &str| event[key].as_i64().and_then(|n| i32::try_from(n).ok());
            events.push(Event::Finished(Terminal::Exit {
                code: number("code"),
                signal: number("signal"),
                stderr: event["stderr"].as_str().unwrap_or_default().to_owned(),
            }));
            return Ok(events);
        }
        if let Some(id) = text(&event["session_id"], "session ID")?
            && id != self.session
        {
            return Err(Violation::policy("Cursor reported a different chat"));
        }
        let mut events = self.release(kind);
        match (kind, self.start) {
            ("system", Start::Pending)
                if text(&event["subtype"], "system event")? == Some("init") =>
            {
                if text(&event["session_id"], "session ID")? != Some(&self.session) {
                    return Err(Violation::policy("Cursor did not open the requested chat"));
                }
                self.start = Start::Opened;
            }
            ("system", _) => {}
            (_, Start::Pending) => {
                return Err(Violation::policy("Cursor sent output before its chat"));
            }
            ("user", Start::Opened) => {
                self.start = Start::Accepted;
                self.acted |= self.host;
                events.push(Event::Accepted {
                    model: self.model.clone(),
                    native_turn: self.session.clone(),
                });
            }
            ("assistant", Start::Accepted) => events.extend(self.on_assistant(event)?),
            ("tool_call", Start::Accepted) => events.extend(self.on_tool_call(event)?),
            ("interaction_query", _)
                if event["subtype"] == "request"
                    && matches!(
                        event["query_type"].as_str(),
                        Some("webSearchRequestQuery" | "webFetchRequestQuery")
                    ) =>
            {
                return Err(Violation::policy("Cursor asked to use the web"));
            }
            ("result", _) => {
                self.clean = event["exit_code"] == 0;
                let usage = &event["usage"];
                let count = |key: &str| usage[key].as_i64().unwrap_or(0).max(0);
                if usage.is_object() {
                    let cached_input = count("cacheReadTokens");
                    let cache_write = count("cacheWriteTokens");
                    events.push(Event::Usage(Usage {
                        input: count("inputTokens") + cached_input + cache_write,
                        cached_input,
                        cache_write,
                        output: count("outputTokens"),
                    }));
                }
                events.push(Event::Finished(Terminal::Result {
                    success: text(&event["subtype"], "result")? == Some("success")
                        && event["is_error"] == false,
                    text: event["result"].as_str().unwrap_or_default().to_owned(),
                }));
            }
            ("assistant" | "tool_call", _) => {
                return Err(Violation::policy("Cursor ran before taking the prompt"));
            }
            _ => {}
        }
        Ok(events)
    }

    fn outcome(
        &self,
        terminal: Terminal,
        stopping: Option<StopCause>,
        accepted: bool,
    ) -> TurnOutcome {
        match terminal {
            Terminal::Result { success: true, .. } if accepted => TurnOutcome::Completed,
            Terminal::Result { success: true, .. } => TurnOutcome::Uncertain {
                message: "Cursor finished without taking the prompt.".into(),
            },
            Terminal::Result { text, .. } => TurnOutcome::failed(
                FailureCode::ProviderFailed,
                format!("Cursor failed: {text}"),
            ),
            Terminal::Exit {
                code,
                signal,
                stderr,
            } if stopping.is_some() && cancelled(code, signal, &stderr) => TurnOutcome::Interrupted,
            Terminal::Exit {
                code: Some(code), ..
            } if self.acted => TurnOutcome::Uncertain {
                message: format!("Cursor CLI exited with {code} partway through the turn."),
            },
            Terminal::Exit { stderr, .. } if super::cursor_home::refused_login(&stderr) => {
                TurnOutcome::failed(
                    FailureCode::ProviderUnavailable,
                    super::cursor_home::SIGNED_OUT,
                )
            }
            Terminal::Exit { stderr, .. } if usage_limit(&stderr) => TurnOutcome::failed(
                FailureCode::ProviderUnavailable,
                format!("Cursor's usage limit is reached: {}", last_line(&stderr)),
            ),
            Terminal::Exit {
                code: Some(code),
                stderr,
                ..
            } if code != 0 => TurnOutcome::failed(
                FailureCode::ProviderFailed,
                format!("Cursor CLI exited with {code}: {}", last_line(&stderr)),
            ),
            Terminal::Exit { code, signal, .. } => TurnOutcome::Uncertain {
                message: format!(
                    "Cursor CLI ended without a result ({}).",
                    match (code, signal) {
                        (_, Some(signal)) => format!("signal {signal}"),
                        (Some(code), _) => format!("exit {code}"),
                        _ => "still running".into(),
                    }
                ),
            },
        }
    }

    fn resumable(&self, outcome: &TurnOutcome) -> bool {
        *outcome == TurnOutcome::Completed && self.clean
    }
}

/// Whether the CLI ended as SIGINT asks: it prints "Operation cancelled"
/// and exits 130, unless the signal ended it first.
fn cancelled(code: Option<i32>, signal: Option<i32>, stderr: &str) -> bool {
    code == Some(130)
        || signal.is_some_and(|s| [2, 9, 15].contains(&s))
        || stderr.contains("Operation cancelled")
}

fn usage_limit(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    ["resource_exhausted", "usage limit", "rate limit"]
        .iter()
        .any(|needle| lower.contains(needle))
}

fn last_line(stderr: &str) -> &str {
    stderr
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no message")
}
