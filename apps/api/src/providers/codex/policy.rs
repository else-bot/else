//! What Codex may do in a turn, checked from its own reports.

use serde_json::{Value, json};

use super::super::codex_rpc::{AppServer, Incoming};
use super::{Codex, RPC_TIMEOUT};
use crate::app::providers::{FailureCode, LimitWindow, Profile, Steer, TurnOutcome};
use crate::app::tools::{SERVER_NAME, TOOL_NAMES};
use crate::providers::cli::attempt::{Event, Protocol, StopCause, Violation};
use crate::providers::cli::turn::ToolSet;
use crate::providers::prompt;

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

/// Why Codex's reported MCP servers were refused.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// else.bot's server reported no tools: it did not start or connect.
    Unavailable(String),
    /// The servers or their tools are outside the allowlist.
    Policy(String),
}

impl From<Refusal> for String {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Unavailable(reason) | Refusal::Policy(reason) => reason,
        }
    }
}

impl Codex {
    /// The MCP servers Codex reports: none, or only else.bot's with exactly
    /// its tools.
    pub(super) fn check_servers(&self, servers: &Value, with_tools: bool) -> Result<(), Refusal> {
        let policy = |reason: String| Err(Refusal::Policy(reason));
        let servers = servers.as_array().map(Vec::as_slice).unwrap_or_default();
        if !with_tools || self.tool_url.is_none() {
            return if servers.is_empty() {
                Ok(())
            } else {
                policy("Codex has MCP servers configured.".into())
            };
        }
        let [server] = servers else {
            return policy(format!(
                "Codex reported {} MCP servers; expected only else.bot's.",
                servers.len()
            ));
        };
        if server["name"].as_str() != Some(SERVER_NAME) {
            return policy("Codex reported an MCP server other than else.bot's.".into());
        }
        let malformed =
            || Refusal::Policy("Codex reported a malformed else.bot tool inventory.".into());
        let mut tools: Vec<String> = match &server["tools"] {
            Value::Object(map) => map.keys().cloned().collect(),
            Value::Array(list) => list
                .iter()
                .map(|t| t["name"].as_str().map(str::to_owned).ok_or_else(malformed))
                .collect::<Result<_, _>>()?,
            _ => return Err(malformed()),
        };
        if tools.is_empty() {
            let cause = server["toolsError"].as_str().unwrap_or("it did not start");
            return Err(Refusal::Unavailable(format!(
                "else.bot's MCP server reported no tools: {cause}"
            )));
        }
        tools.sort();
        let mut expected: Vec<String> = TOOL_NAMES.iter().map(|t| (*t).to_owned()).collect();
        expected.sort();
        if tools != expected {
            return policy(format!(
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
    pub status: Option<String>,
    error: Option<String>,
}

/// What a Codex attempt may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Allowed {
    /// A turn: any item but the forbidden ones, with tools from this set.
    Tools(ToolSet),
    /// A summary or checkpoint: messages and reasoning only. Its text is
    /// each final message, read whole.
    Messages,
}

/// One running Codex turn's notifications, checked against its policy.
pub(super) struct TurnEvents<'a> {
    pub thread_id: &'a str,
    pub native_turn: &'a str,
    pub profile: &'a Profile,
    pub allowed: Allowed,
    pub current_message: Option<String>,
    /// Where a steered message's files are staged.
    pub project: &'a std::path::Path,
    /// Steered messages given to the turn and not echoed yet.
    pub steers: std::collections::HashSet<uuid::Uuid>,
}

impl TurnEvents<'_> {
    fn allowed_tool(tools: ToolSet, item: &Value) -> bool {
        tools.elsebot()
            && item["server"].as_str() == Some(SERVER_NAME)
            && item["tool"]
                .as_str()
                .is_some_and(|t| TOOL_NAMES.contains(&t))
    }

    fn check_item(&self, item: &Value) -> Result<(), Violation> {
        let kind = item["type"].as_str().unwrap_or_default();
        let tools = match self.allowed {
            Allowed::Tools(tools) => tools,
            Allowed::Messages if matches!(kind, "userMessage" | "agentMessage" | "reasoning") => {
                return Ok(());
            }
            Allowed::Messages => {
                return Err(Violation::policy(format!("Codex produced a {kind} item")));
            }
        };
        if FORBIDDEN_ITEMS.contains(&kind) || (!tools.host() && HOST_TOOL_ITEMS.contains(&kind)) {
            return Err(Violation::policy(format!("Codex attempted a {kind} item")));
        }
        if kind == "mcpToolCall" && !Self::allowed_tool(tools, item) {
            return Err(Violation::policy(format!(
                "Codex called an MCP tool outside else.bot's allowlist: {}/{}",
                item["server"].as_str().unwrap_or("?"),
                item["tool"].as_str().unwrap_or("?"),
            )));
        }
        Ok(())
    }
}

/// The windows of a Codex rate-limit snapshot. Updates are sparse, so a
/// missing duration falls back to Codex's usual 5-hour primary and weekly
/// secondary windows. Snapshots of other quotas (another model's limit ID)
/// are skipped so they don't interleave with the account's own.
fn limit_windows(snapshot: &Value) -> Vec<LimitWindow> {
    if snapshot["limitId"].as_str().is_some_and(|id| id != "codex") {
        return Vec::new();
    }
    [
        ("primary", LimitWindow::FIVE_HOURS),
        ("secondary", LimitWindow::WEEK),
    ]
    .into_iter()
    .filter_map(|(key, fallback)| {
        let window = &snapshot[key];
        let used = window["usedPercent"].as_f64()?;
        Some(LimitWindow {
            minutes: window["windowDurationMins"].as_i64().unwrap_or(fallback),
            used_basis_points: (used * 100.0).round() as i64,
            // Seconds per the protocol; milliseconds are tolerated.
            resets_at: window["resetsAt"]
                .as_i64()
                .map(|at| if at > 100_000_000_000 { at / 1000 } else { at })
                .and_then(|at| chrono::DateTime::from_timestamp(at, 0))?,
        })
    })
    .collect()
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
        // A summary allows no other item anywhere, not even in another
        // thread or turn.
        if self.allowed == Allowed::Messages && matches!(method, "item/started" | "item/completed")
        {
            self.check_item(&params["item"])?;
        }
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
            // `turn/steer` input comes back as a user message carrying the
            // `clientUserMessageId` it was given, once started and once
            // completed.
            "item/started" | "item/completed" if params["item"]["type"] == "userMessage" => {
                if let Some(turn) = params["item"]["clientId"]
                    .as_str()
                    .and_then(|id| id.parse().ok())
                    .filter(|turn| self.steers.remove(turn))
                {
                    events.push(Event::Steered(turn));
                }
            }
            "item/started" => self.check_item(&params["item"])?,
            "item/completed" if self.allowed == Allowed::Messages => {
                let item = &params["item"];
                if item["type"] == "agentMessage" && item["phase"] != "commentary" {
                    let text = item["text"].as_str().unwrap_or_default();
                    events.extend([Event::EndMessage, Event::Text(text.to_owned())]);
                }
            }
            "item/agentMessage/delta" if self.allowed == Allowed::Messages => {}
            "thread/compacted" => events.push(Event::Compacted),
            "item/completed" if params["item"]["type"] == "contextCompaction" => {
                events.push(Event::Compacted);
            }
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
            "thread/tokenUsage/updated" => {
                // `last` is one model request; a turn sums its requests.
                let last = &params["tokenUsage"]["last"];
                let count = |key: &str| last[key].as_i64().unwrap_or(0).max(0);
                events.push(Event::Usage(crate::app::providers::Usage {
                    input: count("inputTokens"),
                    cached_input: count("cachedInputTokens"),
                    cache_write: count("cacheWriteInputTokens"),
                    output: count("outputTokens"),
                }));
            }
            "account/rateLimits/updated" => {
                let windows = limit_windows(&params["rateLimits"]);
                if !windows.is_empty() {
                    events.push(Event::Limits(windows));
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

    /// The turn's own `turn/steer` request; it fails once the turn ended.
    fn steer_input(&self, steer: &Steer) -> Option<Value> {
        Some(json!({
            "threadId": self.thread_id,
            "expectedTurnId": self.native_turn,
            "clientUserMessageId": steer.turn_id,
            "input": super::turn::turn_input(&prompt::steer_segments(steer), self.project),
        }))
    }

    fn steer_sent(&mut self, steer: &Steer) {
        self.steers.insert(steer.turn_id);
    }

    /// A notice is a `turn/steer` request of its own; its echo carries an
    /// ID no steered message has, so it reports nothing.
    fn notice_input(&self, text: &str) -> Option<Value> {
        Some(json!({
            "threadId": self.thread_id,
            "expectedTurnId": self.native_turn,
            "clientUserMessageId": uuid::Uuid::new_v4(),
            "input": super::turn::turn_input(&[prompt::Segment::Text(text.to_owned())], self.project),
        }))
    }

    fn outcome(&self, completed: Completed, _: Option<StopCause>, _: bool) -> TurnOutcome {
        match completed.status.as_deref() {
            Some("completed") => TurnOutcome::Completed,
            Some("interrupted") => TurnOutcome::Interrupted,
            _ => TurnOutcome::failed(
                FailureCode::ProviderFailed,
                completed
                    .error
                    .unwrap_or_else(|| "Codex reported a failed turn.".to_owned()),
            ),
        }
    }
}
