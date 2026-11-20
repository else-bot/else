//! Policy checks shared by the stream-json dialects of Claude Code and
//! Grok, which report sub-agents, task lifecycles, and MCP servers the same
//! way. Each protocol words its own violations.

use serde_json::Value;

use super::attempt::Violation;
use super::json::Fields;
use crate::app::tools::SERVER_NAME;

/// The tool call an event is parented to. A native sub-agent's events carry
/// the call that started it.
pub fn parent_tool_use(fields: Fields, event: &Value) -> Result<Option<&str>, Violation> {
    fields.text(&event["parent_tool_use_id"], "parent tool use")
}

/// Whether a `system` event's subtype reports native agent activity: a
/// task lifecycle or an agent.
pub fn agent_activity(subtype: &str) -> bool {
    subtype.starts_with("task_") || subtype.contains("agent")
}

/// The MCP servers an `init` event reports, as name and status.
pub fn mcp_servers(fields: Fields, event: &Value) -> Result<Vec<(&str, &str)>, Violation> {
    fields
        .list(&event["mcp_servers"], "MCP inventory")?
        .iter()
        .map(|server| {
            Ok((
                fields.required(&server["name"], "MCP inventory")?,
                fields.required(&server["status"], "MCP inventory")?,
            ))
        })
        .collect()
}

/// Whether `servers` is else.bot's server alone, in one of the `statuses`,
/// when else.bot's tools are offered, and no server otherwise.
pub fn only_elsebot(servers: &[(&str, &str)], offered: bool, statuses: &[&str]) -> bool {
    if !offered {
        return servers.is_empty();
    }
    matches!(servers, [(name, status)] if *name == SERVER_NAME && statuses.contains(status))
}
