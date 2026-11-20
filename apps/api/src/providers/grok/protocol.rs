//! Grok's `streaming-messages-json` events, validated against the turn's
//! policy. Known events must be well formed; unknown event types are
//! progress notes and are ignored.

use std::collections::HashMap;

use serde_json::Value;

use super::grok_home;
use crate::app::providers::{FailureCode, TurnOutcome, Usage};
use crate::app::tools::{DEFERRED_TOOLS, SERVER_NAME, TOOL_NAMES};
use crate::providers::cli::attempt::{Event, Protocol, StopCause, Violation};
use crate::providers::cli::json::Fields;
use crate::providers::cli::stream_json;
use crate::providers::cli::turn::ToolSet;

const FIELDS: Fields = Fields("Grok");

/// Grok's terminal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    Result {
        success: bool,
        stop_reason: Option<String>,
        errors: String,
    },
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    Pending,
    Opened,
    /// Grok stopped before opening any session, as it does without a valid
    /// login; it reports an empty session ID.
    Unopened,
}

pub struct Tracker {
    model: String,
    session: String,
    expected: Vec<String>,
    mcp: bool,
    start: Start,
    calls: HashMap<String, String>,
}

impl Tracker {
    pub fn new(model: &str, session: &str, tools: ToolSet) -> Self {
        let mcp = tools.elsebot();
        let mut expected: Vec<String> = grok_home::MCP_TOOLS.iter().map(|s| (*s).into()).collect();
        if tools.host() {
            expected.extend(grok_home::HOST_TOOLS.iter().map(|(_, name)| (*name).into()));
        }
        Self {
            model: model.into(),
            session: session.into(),
            expected,
            mcp,
            start: Start::Pending,
            calls: HashMap::new(),
        }
    }

    fn mcp_tool<'a>(&self, name: &'a str) -> Option<&'a str> {
        name.strip_prefix(SERVER_NAME)
            .and_then(|rest| rest.strip_prefix("__"))
            .filter(|tool| self.mcp && TOOL_NAMES.contains(tool))
    }

    /// An else.bot tool under a name Grok's catalog lacks: Claude's
    /// mcp__elsebot__<tool> from memory, a bare tool name, or a deferred tool
    /// named directly instead of through elsebot__use_tool. Grok rejects each
    /// without running it and the model can retry, so none is a policy
    /// breach.
    fn misnamed(&self, target: &str) -> bool {
        let tool = target.trim_prefix("mcp__");
        let tool = tool
            .strip_prefix(SERVER_NAME)
            .and_then(|rest| rest.strip_prefix("__"))
            .unwrap_or(tool);
        self.mcp && (TOOL_NAMES.contains(&tool) || DEFERRED_TOOLS.contains(&tool))
    }

    fn check_model(&self, model: &Value) -> Result<(), Violation> {
        match FIELDS.text(model, "model")? {
            Some(model) if model != self.model => Err(Violation::profile(format!(
                "Grok used {model} instead of {}.",
                self.model
            ))),
            _ => Ok(()),
        }
    }

    fn on_init(&mut self, event: &Value) -> Result<(), Violation> {
        if self.start != Start::Pending
            || FIELDS.text(&event["session_id"], "session ID")? != Some(&self.session)
        {
            return Err(Violation::policy(
                "Grok did not initialize the requested session",
            ));
        }
        if FIELDS.text(&event["model"], "model")?.is_none() {
            return Err(Violation::profile("Grok did not report its model."));
        }
        // Grok 1.0.44 leaves get_command_or_subagent_output and
        // kill_command_or_subagent out of the inventory; only a tool outside
        // --tools breaks the policy.
        let tools = FIELDS
            .list(&event["tools"], "tool inventory")?
            .iter()
            .map(|tool| FIELDS.required(tool, "tool inventory"))
            .collect::<Result<Vec<_>, _>>()?;
        let extra: Vec<_> = tools
            .iter()
            .filter(|tool| {
                self.mcp_tool(tool).is_none() && !self.expected.iter().any(|t| t == *tool)
            })
            .collect();
        if !extra.is_empty() {
            return Err(Violation::policy(format!(
                "Grok offered tools else.bot does not allow: {extra:?}"
            )));
        }
        let missing: Vec<_> = grok_home::MCP_TOOLS
            .iter()
            .filter(|tool| self.mcp && !tools.contains(tool))
            .collect();
        if !missing.is_empty() {
            return Err(Violation::policy(format!(
                "Grok did not offer the tools that reach else.bot: {missing:?}"
            )));
        }
        let servers = stream_json::mcp_servers(FIELDS, event)?;
        let valid = stream_json::only_elsebot(&servers, self.mcp, &["connected", "pending"]);
        if !valid {
            return Err(Violation::policy(format!(
                "Grok reported unexpected MCP servers: {servers:?}"
            )));
        }
        self.start = Start::Opened;
        Ok(())
    }

    fn unopened(&mut self, event: &Value) -> Result<Vec<Event<Terminal>>, Violation> {
        let kind = FIELDS.required(&event["type"], "event")?;
        if kind == "system"
            && FIELDS.text(&event["subtype"], "system event")? == Some("init")
            && self.start == Start::Pending
        {
            self.start = Start::Unopened;
            return Ok(Vec::new());
        }
        match terminal(kind, event)? {
            Some(terminal) => Ok(vec![Event::Finished(terminal)]),
            None => Err(Violation::policy("Grok sent output without a session")),
        }
    }

    fn on_assistant(&mut self, event: &Value) -> Result<(), Violation> {
        for block in FIELDS.list(&event["message"]["content"], "assistant message")? {
            match FIELDS.required(&block["type"], "content block")? {
                "server_tool_use" => return Err(Violation::policy("Grok used a backend tool")),
                "tool_use" => {}
                _ => continue,
            }
            let name = FIELDS.required(&block["name"], "tool call")?;
            let id = FIELDS.required(&block["id"], "tool call")?;
            let tool = if name == "use_tool" {
                let Some(target) = block["input"]["tool_name"].as_str() else {
                    return Err(Violation::policy(
                        "Grok called use_tool without a tool_name, so else.bot could not check which tool it ran",
                    ));
                };
                match self.mcp_tool(target) {
                    Some(tool) => Some(tool),
                    None if self.misnamed(target) => None,
                    None => {
                        return Err(Violation::policy(format!(
                            "Grok called an MCP tool outside else.bot: {target}"
                        )));
                    }
                }
            } else if let Some(tool) = self.mcp_tool(name) {
                Some(tool)
            } else if self.expected.iter().any(|t| t == name) {
                None
            } else {
                return Err(Violation::policy(format!(
                    "Grok called an unexpected tool: {name}"
                )));
            };
            if let Some(tool) = tool {
                self.calls.insert(id.to_owned(), tool.to_owned());
            }
        }
        Ok(())
    }

    fn parked(&self, event: &Value) -> Result<bool, Violation> {
        let mut parked = false;
        let blocks = match &event["message"]["content"] {
            Value::String(_) => &[],
            content => FIELDS.list(content, "user message")?,
        };
        for block in blocks {
            let Some(id) = FIELDS.text(&block["tool_use_id"], "tool result")? else {
                continue;
            };
            parked |= self
                .calls
                .get(id)
                .is_some_and(|tool| crate::app::tools::parks(tool, &block["content"]));
        }
        Ok(parked)
    }
}

impl Protocol for Tracker {
    type Message = Value;
    type Terminal = Terminal;

    fn on(&mut self, event: &Value) -> Result<Vec<Event<Terminal>>, Violation> {
        if stream_json::parent_tool_use(FIELDS, event)?.is_some() {
            return Err(Violation::policy("Grok ran a native subagent"));
        }
        let different = || Violation::policy("Grok reported a different native session");
        match (self.start, FIELDS.text(&event["session_id"], "session ID")?) {
            (_, Some(id)) if !id.is_empty() && id != self.session => return Err(different()),
            (Start::Unopened, _) | (Start::Pending, Some("")) => return self.unopened(event),
            (Start::Opened, Some("")) => return Err(different()),
            _ => {}
        }
        self.check_model(&event["model"])?;
        self.check_model(&event["message"]["model"])?;
        self.check_model(&event["event"]["message"]["model"])?;
        let kind = FIELDS.required(&event["type"], "event")?;
        let mut events = Vec::new();
        if let Some(terminal) = terminal(kind, event)? {
            events.extend(Usage::from_result(event).map(Event::Usage));
            events.push(Event::Finished(terminal));
            return Ok(events);
        }
        match kind {
            "system" => match FIELDS
                .text(&event["subtype"], "system event")?
                .unwrap_or_default()
            {
                "init" => self.on_init(event)?,
                sub if stream_json::agent_activity(sub) => {
                    return Err(Violation::policy("Grok reported native agent activity"));
                }
                _ => {}
            },
            "stream_event" => {
                if self.start != Start::Opened {
                    return Err(Violation::policy(
                        "Grok produced output before its inventory",
                    ));
                }
                let inner = &event["event"];
                match FIELDS.required(&inner["type"], "stream event")? {
                    "message_start" => events.push(Event::Accepted {
                        model: self.model.clone(),
                        native_turn: self.session.clone(),
                    }),
                    "content_block_delta"
                        if FIELDS.text(&inner["delta"]["type"], "delta")? == Some("text_delta") =>
                    {
                        let delta = FIELDS.required(&inner["delta"]["text"], "text delta")?;
                        events.push(Event::Text(delta.to_owned()));
                    }
                    "message_stop" => events.push(Event::EndMessage),
                    _ => {}
                }
            }
            "assistant" => self.on_assistant(event)?,
            "user" => {
                if self.parked(event)? {
                    events.push(Event::Parked);
                }
            }
            "control_request" => {
                return Err(Violation::policy("Grok requested interactive permission"));
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
        let (success, stop_reason, errors) = match terminal {
            Terminal::Error(message) => {
                return TurnOutcome::failed(FailureCode::ProviderFailed, message);
            }
            Terminal::Result {
                success,
                stop_reason,
                errors,
            } => (success, stop_reason, errors),
        };
        if self.start == Start::Unopened {
            return TurnOutcome::failed(
                FailureCode::ProviderFailed,
                format!("Grok stopped before opening a session: {errors}"),
            );
        }
        if self.start != Start::Opened || !accepted {
            return TurnOutcome::Uncertain {
                message: format!("Grok ended without acknowledging the prompt: {errors}"),
            };
        }
        match stop_reason.as_deref() {
            Some("end_turn") if success => TurnOutcome::Completed,
            Some("cancelled") if stopping.is_some() => TurnOutcome::Interrupted,
            Some("cancelled") => TurnOutcome::failed(
                FailureCode::ProviderRefused,
                "Grok refused to execute a tool without additional permission.",
            ),
            reason => TurnOutcome::failed(
                FailureCode::ProviderFailed,
                format!(
                    "Grok ended with {}: {errors}",
                    reason.unwrap_or("no stop reason")
                ),
            ),
        }
    }

    /// Only a completed native turn is known to have settled; Grok sessions
    /// that ended any other way are rebuilt from SQLite.
    fn resumable(&self, outcome: &TurnOutcome) -> bool {
        *outcome == TurnOutcome::Completed
    }
}

fn terminal(kind: &str, event: &Value) -> Result<Option<Terminal>, Violation> {
    Ok(Some(match kind {
        "result" => Terminal::Result {
            success: FIELDS.text(&event["subtype"], "result")? == Some("success")
                && event["is_error"] == false,
            stop_reason: FIELDS
                .text(&event["stop_reason"], "result")?
                .map(str::to_owned),
            errors: errors(&event["errors"]),
        },
        "error" => Terminal::Error(
            FIELDS
                .text(&event["message"], "error")?
                .unwrap_or("Grok failed")
                .to_owned(),
        ),
        _ => return Ok(None),
    }))
}

fn errors(value: &Value) -> String {
    let messages: Option<Vec<&str>> = value
        .as_array()
        .and_then(|list| list.iter().map(Value::as_str).collect());
    messages.map_or_else(|| value.to_string(), |messages| messages.join("\n"))
}
