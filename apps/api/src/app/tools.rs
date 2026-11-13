//! The tool inventory shared by provider configuration and MCP transport.
//! Adapters verify this exact allowlist before running a turn.

pub const SERVER_NAME: &str = "elsebot";
/// The tools this server lists. Adapters allow exactly these. The else.bot
/// tools a turn lists directly. Rarely used tools are reached
/// through `find_tool` and `use_tool` instead (see [`DEFERRED_TOOLS`]).
pub const TOOL_NAMES: &[&str] = &[
    "mcp_list",
    "mcp_install",
    "mcp_request",
    "mcp_wait",
    "mcp_connect",
    "skill_install",
    "skill_read",
    "memory_bots",
    "memory_search",
    "memory_read",
    "memory_view",
    "delegate",
    "council",
    "request_secret",
    "ask_owner",
    "ask_permission",
    "draft_email",
    "draft_message",
    "webhook_create",
    "mail_trigger_set",
    "webhook_update",
    "social_connect",
    "social_action",
    "computer_run",
    "computer_job_start",
    "computer_job",
    "computer_write_file",
    "computer_read_file",
    "computer_fetch",
    "computer_browser",
    "computer_screenshot",
    "computer_input",
    "computer_handoff",
    "mail_accounts",
    "mail_connect",
    "mail_action",
    "mail_action_status",
    "social_connections",
    "social_action_status",
    "rules_update",
    "visual_preview",
    "find_tool",
    "use_tool",
];
/// Tools reached only through `use_tool`, after `find_tool` returns their
/// schema. The mail and social tools an incoming event turn works with stay
/// listed. None may park a turn: providers recognize parking by the name
/// of the tool they called, which for these is `use_tool`.
pub const DEFERRED_TOOLS: &[&str] = &[
    "mcp_connection_status",
    "memory_zoom",
    "memory_date",
    "else_status",
    "list_bots",
    "create_bot",
    "delegation_update",
    "delegation_cancel",
    "rename_bot",
    "set_bot_model",
    "confirm_permission",
    "confirm_draft_approval",
    "start_draft_send",
    "send_draft",
    "report_draft_send",
    "routine_create",
    "routine_list",
    "routine_update",
    "routine_delete",
    "routine_run",
    "reminder_create",
    "webhook_list",
    "webhook_delete",
    "social_trigger_set",
    "social_trigger_list",
    "social_trigger_delete",
    "mail_trigger_delete",
    "computer_job_cancel",
    "computer_secrets",
];
pub const PARKING_TOOLS: &[&str] = &[
    "mcp_request",
    "mcp_wait",
    "computer_job",
    "mcp_install",
    "mcp_connect",
    "skill_install",
    "computer_handoff",
    "social_connect",
    "social_action",
    "mail_connect",
    "delegate",
    "council",
    "request_secret",
    "ask_owner",
    "ask_permission",
    "draft_email",
    "draft_message",
    "webhook_create",
    "mail_trigger_set",
    "webhook_update",
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

pub fn summarize_input(actions: &[serde_json::Value]) -> String {
    actions
        .iter()
        .map(|a| {
            if let Some(t) = a["type"].as_str() {
                format!("type({} chars)", t.chars().count())
            } else if let Some(k) = a["key"].as_str() {
                format!("key({})", &k[..k.floor_char_boundary(k.len().min(40))])
            } else if let Some((kind, v)) = a.as_object().and_then(|o| o.iter().next()) {
                format!("{kind}({v})")
            } else {
                "?".into()
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn loggable_args(name: &str, args: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Value, json};

    match name {
        "mcp_install" => json!({"id":args["id"],"name":args["name"]}),
        "mcp_connect" | "mcp_connection_status" => json!({"id":args["id"]}),
        "mcp_request" => {
            json!({"id":args["id"],"method":args["method"],"tool":args["params"]["name"],"effect":args["effect"]})
        }
        "mcp_wait" => json!({"id":args["id"],"tool":args["tool"]}),
        "mail_accounts" | "social_connections" => json!({}),
        "social_connect" => json!({"provider":args["provider"],"reason":args["reason"]}),
        "mail_connect" => json!({"provider":args["provider"]}),
        "social_action" | "social_action_status" => {
            json!({"connection_id":args["connection_id"],"action_id":args["action_id"],"operation":args["command"]["operation"]})
        }
        "mail_action" => {
            json!({ "account_id": args["account_id"], "action_id": args["action_id"], "operation": args["command"]["operation"] })
        }
        "computer_input" => json!({
            "frame": args["frame"],
            "actions": summarize_input(args["actions"].as_array().map(Vec::as_slice).unwrap_or_default()),
        }),
        "computer_browser" if args["action"] == "type" => {
            let mut safe = args.clone();
            if let Some(text) = args["text"].as_str() {
                safe["text"] = Value::String(format!("type({} chars)", text.chars().count()));
            }
            safe
        }
        _ => args.clone(),
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
