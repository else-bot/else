//! The tool inventory shared by provider configuration and MCP transport.
//! Adapters verify this exact allowlist before running a turn.

pub const SERVER_NAME: &str = "elsebot";
/// The tools a turn lists, in the order adapters hash and allowlist. Rarely
/// used tools are reached through `find_tool` and `use_tool` instead (see
/// [`DEFERRED_TOOLS`]). Each tool is defined once in `http::tool_defs`.
pub const TOOL_NAMES: &[&str] = &crate::http::tool_defs::LISTED;
/// Tools reached only through `use_tool`, after `find_tool` returns their
/// schema. None may park a turn: providers recognize parking by the name of
/// the tool they called, which for these is `use_tool`.
pub const DEFERRED_TOOLS: &[&str] = &crate::http::tool_defs::DEFERRED;
pub const PARKING_TOOLS: &[&str] = &crate::http::tool_defs::PARKING;

pub fn parks(tool: &str, result: &serde_json::Value) -> bool {
    use serde_json::Value;

    let parking = crate::http::tool_defs::get(tool).is_some_and(|tool| tool.parks());
    if !parking || result["isError"] == true {
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
