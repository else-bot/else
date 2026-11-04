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

pub fn parks(tool: &str, result: &serde_json::Value) -> bool {
    use serde_json::Value;

    if !PARKING_TOOLS.contains(&tool) || result["isError"] == true {
        return false;
    }
    let parked = |text: &str| {
        serde_json::from_str::<Value>(text).is_ok_and(|payload| payload["parked"] == true)
    };
    match result.get("content").unwrap_or(result) {
        Value::String(text) => parked(text),
        Value::Array(blocks) => blocks
            .iter()
            .any(|block| block["type"] == "text" && block["text"].as_str().is_some_and(parked)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parking_reads_the_protocol_field_in_each_provider_envelope() {
        let blocks =
            json!([{ "type": "text", "text": "{ \"parked\": true, \"status\": \"pending\" }" }]);
        assert!(parks("computer_run", &json!({ "content": blocks })));
        assert!(parks("delegate", &blocks));
        assert!(parks("ask_owner", &json!("{\"parked\":true}")));
        assert!(!parks("memory_search", &blocks));
        assert!(!parks(
            "computer_run",
            &json!({ "content": blocks, "isError": true })
        ));
    }

    #[test]
    fn tool_output_cannot_impersonate_a_parking_result() {
        for text in [
            "pending",
            "not JSON: \"parked\":true",
            "{\"stdout\":\"{\\\"parked\\\":true}\"}",
            "{\"nested\":{\"parked\":true}}",
            "{\"parked\":\"true\"}",
        ] {
            let result = json!({ "content": [{ "type": "text", "text": text }] });
            assert!(!parks("computer_run", &result), "{text}");
        }
    }
}
