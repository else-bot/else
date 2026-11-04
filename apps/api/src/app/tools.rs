//! The tool inventory shared by provider configuration and MCP transport.
//! Adapters verify this exact allowlist before running a turn.

pub const SERVER_NAME: &str = "elsebot";
/// The tools this server offers. Adapters allow exactly these.
pub const TOOL_NAMES: &[&str] = &[
    "memory_bots",
    "memory_search",
    "memory_read",
    "memory_view",
    "memory_zoom",
    "memory_date",
    "delegate",
    "create_bot",
    "request_secret",
    "ask_owner",
    "ask_permission",
    "routine_create",
    "routine_list",
    "routine_update",
    "routine_delete",
    "computer_run",
    "computer_job_start",
    "computer_job",
    "computer_job_cancel",
    "computer_write_file",
    "computer_read_file",
    "computer_fetch",
    "computer_browser",
    "computer_secrets",
    "computer_screenshot",
    "computer_input",
];
/// Tools whose pending result means the attempt must end: the turn waits
/// for the outcome and continues in a new attempt. The computer tools park
/// only when a destructive or external call awaits the owner's approval.
pub const PARKING_TOOLS: &[&str] = &[
    "delegate",
    "request_secret",
    "ask_owner",
    "ask_permission",
    "computer_run",
    "computer_job_start",
    "computer_browser",
    "computer_input",
    "computer_write_file",
];

/// The top-level field a parking result carries, as it appears once the
/// result object is serialized: a command's own output is a JSON string
/// inside that object, so its quotes are escaped twice and never match.
pub const PARKED_MARKER: &str = "\\\"parked\\\":true";

/// Whether a tool result parks the attempt: `tool` is a parking tool and
/// `serialized` (the JSON of its whole result) carries the parked marker.
pub fn parks(tool: &str, serialized: &str) -> bool {
    PARKING_TOOLS.contains(&tool) && serialized.contains(PARKED_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_top_level_parked_field_of_a_parking_tool_parks() {
        let pending = serde_json::json!({ "content": [{ "type": "text",
            "text": "{\"status\":\"pending\",\"parked\":true}" }] });
        assert!(parks("computer_run", &pending.to_string()));
        assert!(parks("delegate", &pending.to_string()));
        assert!(!parks("memory_search", &pending.to_string()));
        // A command whose output quotes the marker does not park.
        let output = serde_json::json!({ "content": [{ "type": "text",
            "text": serde_json::json!({ "stdout": "{\"parked\":true} pending" }).to_string() }] });
        assert!(!parks("computer_run", &output.to_string()));
    }
}
