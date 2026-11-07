//! What Codex may do in a turn, checked from its own reports.

use serde_json::{Value, json};

use super::super::codex_rpc::{AppServer, Incoming};
use super::{Codex, RPC_TIMEOUT, failed};
use crate::app::providers::{Profile, TurnOutcome};
use crate::app::tools::{SERVER_NAME, TOOL_NAMES};
use crate::providers::cli::attempt::{Event, Protocol, StopCause, Violation};

const FORBIDDEN_ITEMS: &[&str] = &[
    "dynamicToolCall",
    "collabAgentToolCall",
    "subAgentActivity",
    "webSearch",
    "imageGeneration",
    "imageView",
];

const HOST_TOOL_ITEMS: &[&str] = &["commandExecution", "fileChange"];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Reported {
    model: Option<String>,
    effort: Option<String>,
    tier: Option<String>,
}

impl Reported {
    pub(super) fn from_response(value: &Value) -> Self {
        let text = |key: &str| value[key].as_str().map(str::to_owned);
        Self {
            model: text("model"),
            effort: text("reasoningEffort").or_else(|| text("effort")),
            tier: text("serviceTier"),
        }
    }

    pub(super) fn mismatch(&self, profile: &Profile) -> Option<String> {
        let check = |field: &str, got: &Option<String>, want: &str| {
            got.as_ref()
                .filter(|got| *got != want)
                .map(|got| format!("{field} {got} instead of {want}"))
        };
        let issues: Vec<String> = [
            check("model", &self.model, &profile.model),
            check("reasoning effort", &self.effort, &profile.reasoning_effort),
            check("service tier", &self.tier, &profile.service_tier),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!issues.is_empty()).then(|| format!("Codex reported {}.", issues.join(", ")))
    }

    pub(super) fn to_json(&self) -> Value {
        json!({ "model": self.model, "reasoning_effort": self.effort, "service_tier": self.tier })
    }
}

impl Codex {
    /// The MCP servers Codex reports: none, or only else.bot's with exactly
    /// its tools.
    pub(super) fn check_servers(&self, servers: &Value, with_tools: bool) -> Result<(), String> {
        let servers = servers.as_array().map(Vec::as_slice).unwrap_or_default();
        if !with_tools || self.tool_url.is_none() {
            return if servers.is_empty() {
                Ok(())
            } else {
                Err("Codex has MCP servers configured.".into())
            };
        }
        let [server] = servers else {
            return Err(format!(
                "Codex reported {} MCP servers; expected only else.bot's.",
                servers.len()
            ));
        };
        if server["name"].as_str() != Some(SERVER_NAME) {
            return Err("Codex reported an MCP server other than else.bot's.".into());
        }
        let malformed = || "Codex reported a malformed else.bot tool inventory.".to_owned();
        let mut tools: Vec<String> = match &server["tools"] {
            Value::Object(map) => map.keys().cloned().collect(),
            Value::Array(list) => list
                .iter()
                .map(|t| t["name"].as_str().map(str::to_owned).ok_or_else(malformed))
                .collect::<Result<_, _>>()?,
            _ => return Err(malformed()),
        };
        tools.sort();
        let mut expected: Vec<String> = TOOL_NAMES.iter().map(|t| (*t).to_owned()).collect();
        expected.sort();
        if tools != expected {
            return Err(format!(
                "else.bot's MCP server offered unexpected tools: {tools:?}"
            ));
        }
        Ok(())
    }

    pub(super) async fn check_catalog(server: &AppServer, model: &str) -> Result<(), String> {
        let listed = server
            .request("model/list", json!({ "includeHidden": true }), RPC_TIMEOUT)
            .await
            .map_err(|err| err.to_string())?;
        match listed["data"].as_array().map(Vec::as_slice) {
            Some([only]) if only["model"] == model && listed["nextCursor"].is_null() => Ok(()),
            _ => Err("Codex is not running on the catalog without native tools.".into()),
        }
    }
}

/// The terminal `turn/completed` of the turn.
pub(super) struct Completed {
    status: Option<String>,
    error: Option<String>,
}

/// One running Codex turn's notifications, checked against its policy.
pub(super) struct TurnEvents<'a> {
    pub thread_id: &'a str,
    pub native_turn: &'a str,
    pub profile: &'a Profile,
    pub host_tools: bool,
    pub elsebot_tools: bool,
    pub current_message: Option<String>,
}

impl TurnEvents<'_> {
    fn allowed_tool(&self, item: &Value) -> bool {
        self.elsebot_tools
            && item["server"].as_str() == Some(SERVER_NAME)
            && item["tool"]
                .as_str()
                .is_some_and(|t| TOOL_NAMES.contains(&t))
    }

    fn check_item(&self, item: &Value) -> Result<(), Violation> {
        let kind = item["type"].as_str().unwrap_or_default();
        if FORBIDDEN_ITEMS.contains(&kind) || (!self.host_tools && HOST_TOOL_ITEMS.contains(&kind))
        {
            return Err(Violation::policy(format!("Codex attempted a {kind} item")));
        }
        if kind == "mcpToolCall" && !self.allowed_tool(item) {
            return Err(Violation::policy(format!(
                "Codex called an MCP tool outside else.bot's allowlist: {}/{}",
                item["server"].as_str().unwrap_or("?"),
                item["tool"].as_str().unwrap_or("?"),
            )));
        }
        Ok(())
    }
}

impl Protocol for TurnEvents<'_> {
    type Message = Incoming;
    type Terminal = Completed;

    fn on(&mut self, message: &Incoming) -> Result<Vec<Event<Completed>>, Violation> {
        let (method, params) = match message {
            Incoming::Refused { method } => {
                return Err(Violation::policy(format!("Codex requested {method}")));
            }
            Incoming::Notification { method, params } => (method.as_str(), params),
        };
        if !method.ends_with("/delta") {
            tracing::debug!(
                %method,
                item = params["item"]["type"].as_str().unwrap_or_default(),
                tool = params["item"]["tool"].as_str().unwrap_or_default(),
                "codex event"
            );
        }
        let thread = params["threadId"].as_str();
        let turn = params["turnId"].as_str().or(params["turn"]["id"].as_str());
        let other_thread = thread.is_some_and(|t| t != self.thread_id);
        let other_turn = turn.is_some_and(|t| t != self.native_turn);
        let mut events = Vec::new();
        match method {
            "thread/started" if params["thread"]["id"].as_str() != Some(self.thread_id) => {
                return Err(Violation::policy("Codex started a native sub-agent thread"));
            }
            _ if other_thread || other_turn => {}
            "item/completed"
                if params["item"]["type"] == "mcpToolCall"
                    && params["item"]["tool"].as_str().is_some_and(|t| {
                        crate::app::tools::parks(t, &params["item"]["result"])
                    }) =>
            {
                events.push(Event::Parked);
            }
            "item/started" => self.check_item(&params["item"])?,
            "model/rerouted" => {
                let to = params["toModel"].as_str().unwrap_or_default();
                if to != self.profile.model {
                    return Err(Violation::profile(format!(
                        "Codex rerouted the turn to {to}."
                    )));
                }
            }
            "thread/settings/updated" => {
                let settings = &params["threadSettings"];
                if let Some(mismatch) = Reported::from_response(settings).mismatch(self.profile) {
                    return Err(Violation::profile(mismatch));
                }
            }
            "item/agentMessage/delta" => {
                let item = params["itemId"].as_str().unwrap_or_default().to_owned();
                if self.current_message.as_ref().is_some_and(|c| *c != item) {
                    events.push(Event::EndMessage);
                }
                self.current_message = Some(item);
                if let Some(delta) = params["delta"].as_str() {
                    events.push(Event::Text(delta.to_owned()));
                }
            }
            "turn/completed" => {
                let turn = &params["turn"];
                events.push(Event::Finished(Completed {
                    status: turn["status"].as_str().map(str::to_owned),
                    error: turn["error"]["message"].as_str().map(str::to_owned),
                }));
            }
            _ => {}
        }
        Ok(events)
    }

    fn outcome(&self, completed: Completed, _: Option<StopCause>, _: bool) -> TurnOutcome {
        match completed.status.as_deref() {
            Some("completed") => TurnOutcome::Completed,
            Some("interrupted") => TurnOutcome::Interrupted,
            _ => failed(
                "provider_failed",
                completed
                    .error
                    .unwrap_or_else(|| "Codex reported a failed turn.".to_owned()),
            ),
        }
    }
}
