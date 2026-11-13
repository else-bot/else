//! Tool search over else.bot's rarely used tools. A turn lists the tools in
//! [`TOOL_NAMES`] directly; those in [`DEFERRED_TOOLS`] stay out of every
//! turn's tool definitions until `find_tool` returns their schema, and run
//! through `use_tool` with the same grant checks as a direct call.

use serde_json::{Value, json};

use crate::app::tools::{DEFERRED_TOOLS, TOOL_NAMES};

pub const FIND: &str = "find_tool";
pub const USE: &str = "use_tool";
/// Tools one `find_tool` call returns.
const MAX_FOUND: usize = 5;

/// The tool definitions a turn lists: the direct ones from `all`, then the
/// two search tools.
pub fn listed(all: Vec<Value>) -> Vec<Value> {
    let mut tools: Vec<Value> = all
        .into_iter()
        .filter(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|name| TOOL_NAMES.contains(&name))
        })
        .collect();
    tools.extend(schemas());
    tools
}

fn schemas() -> [Value; 2] {
    [
        json!({
            "name": FIND,
            "description": format!(
                "Find else.bot tools that are not in your tool list; use_tool's name lists them. \
                 Search by a tool's name or a few words about the need. Returns up to \
                 {MAX_FOUND} tools with their description and input schema; run one with use_tool."
            ),
            "inputSchema": {
                "type": "object",
                "properties": { "query": { "type": "string", "minLength": 1, "maxLength": 200 } },
                "required": ["query"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false,
                             "idempotentHint": true, "openWorldHint": false },
        }),
        json!({
            "name": USE,
            "description": "Run an else.bot tool that find_tool returned, with arguments that \
                match its input schema. It does exactly what calling that tool directly would, \
                with the same checks and result. Call tools in your tool list directly.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "enum": DEFERRED_TOOLS },
                    "arguments": { "type": "object" },
                },
                "required": ["name", "arguments"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false },
        }),
    ]
}

/// The deferred tools from `all` that best match `query`: an exact name
/// first, then by words shared with each tool's name and description.
pub fn find(all: &[Value], query: &str) -> Value {
    let query = query.to_lowercase();
    let bare = query.trim().trim_start_matches("mcp__elsebot__");
    if TOOL_NAMES.contains(&bare) {
        return json!({ "tools": [], "listed": format!(
            "{bare} is in your tool list, as mcp__elsebot__{bare} where your tools carry that \
             prefix: call it directly."
        ) });
    }
    let words: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .collect();
    let mut scored: Vec<(usize, &Value)> = all
        .iter()
        .filter_map(|tool| {
            let name = tool["name"].as_str()?;
            if !DEFERRED_TOOLS.contains(&name) {
                return None;
            }
            let description = tool["description"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase();
            let score = if query.contains(name) {
                100
            } else {
                words
                    .iter()
                    .map(|w| {
                        if name
                            .split('_')
                            .any(|part| part.starts_with(w) || w.starts_with(part))
                        {
                            3
                        } else {
                            usize::from(description.contains(w))
                        }
                    })
                    .sum()
            };
            (score > 0).then_some((score, tool))
        })
        .collect();
    scored.sort_by_key(|a| std::cmp::Reverse(a.0));
    let tools: Vec<&Value> = scored.into_iter().take(MAX_FOUND).map(|(_, t)| t).collect();
    if tools.is_empty() {
        return json!({ "tools": [], "available": DEFERRED_TOOLS });
    }
    json!({ "tools": tools })
}

/// The deferred tool a `use_tool` call names, and its arguments. Arguments
/// its schema does not declare are refused before dispatch: an undeclared
/// `effect` would otherwise reach the shared approval path and park the turn
/// under `use_tool`, a name no provider recognizes as parking.
pub fn target<'a>(all: &[Value], args: &'a Value) -> Result<(&'a str, &'a Value), &'static str> {
    let name = args["name"].as_str().unwrap_or_default();
    if TOOL_NAMES.contains(&name) {
        return Err("That tool is in your tool list: call it directly. Nothing was done.");
    }
    if !DEFERRED_TOOLS.contains(&name) {
        return Err("Name a tool that find_tool returned. Nothing was done.");
    }
    let Value::Object(arguments) = &args["arguments"] else {
        return Err("Give the tool's arguments as an object. Nothing was done.");
    };
    let declared = all
        .iter()
        .find(|tool| tool["name"] == name)
        .and_then(|tool| tool["inputSchema"]["properties"].as_object());
    if arguments
        .keys()
        .any(|key| declared.is_none_or(|props| !props.contains_key(key)))
    {
        return Err(
            "Use only the arguments in the tool's input schema from find_tool. Nothing was done.",
        );
    }
    Ok((name, &args["arguments"]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tools::PARKING_TOOLS;

    #[test]
    fn no_deferred_tool_parks_or_is_also_listed() {
        for name in DEFERRED_TOOLS {
            assert!(!PARKING_TOOLS.contains(name), "{name} parks");
            assert!(!TOOL_NAMES.contains(name), "{name} is listed");
        }
    }

    #[test]
    fn every_tool_is_listed_or_deferred_exactly_once() {
        let all = super::super::mcp::all_tools();
        let names: Vec<&str> = all.iter().filter_map(|t| t["name"].as_str()).collect();
        for name in &names {
            assert!(
                TOOL_NAMES.contains(name) ^ DEFERRED_TOOLS.contains(name),
                "{name}"
            );
        }
        for name in TOOL_NAMES.iter().chain(DEFERRED_TOOLS) {
            assert!(
                names.contains(name) || [FIND, USE].contains(name),
                "{name} has no schema"
            );
        }
        let listed: Vec<String> = listed(all)
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(listed.len(), TOOL_NAMES.len());
    }

    #[test]
    fn find_returns_matching_deferred_schemas_only() {
        let all = super::super::mcp::all_tools();
        let exact = find(&all, "routine_create");
        assert_eq!(exact["tools"][0]["name"], "routine_create");
        assert!(exact["tools"][0]["inputSchema"].is_object());
        let words = find(&all, "save a recurring schedule");
        let names: Vec<&str> = words["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"routine_create"), "{names:?}");
        assert!(
            find(&all, "computer_run")["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t["name"] != "computer_run")
        );
        assert_eq!(find(&all, "zzzz")["available"], json!(DEFERRED_TOOLS));
        let listed = find(&all, "delegate");
        assert!(
            listed["listed"]
                .as_str()
                .unwrap()
                .contains("call it directly")
        );
    }

    #[test]
    fn find_returns_only_the_deferred_tools_a_query_names() {
        let all = super::super::mcp::all_tools();
        let names = |query: &str| -> Vec<String> {
            find(&all, query)["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["name"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(names("routine_list"), ["routine_list"]);
        assert_eq!(names("mcp__elsebot__routine_list"), ["routine_list"]);
        assert_eq!(
            names("routine_list, routine_update"),
            ["routine_list", "routine_update"]
        );
        assert_eq!(
            names("schema for computer_job_cancel"),
            ["computer_job_cancel"]
        );
    }

    #[test]
    fn use_tool_runs_only_deferred_tools_with_declared_arguments() {
        let all = super::super::mcp::all_tools();
        assert_eq!(
            target(&all, &json!({ "name": "routine_list", "arguments": {} })),
            Ok(("routine_list", &json!({})))
        );
        assert!(target(&all, &json!({ "name": "ask_permission", "arguments": {} })).is_err());
        assert!(target(&all, &json!({ "name": "nope", "arguments": {} })).is_err());
        assert!(target(&all, &json!({ "name": "routine_list", "arguments": [] })).is_err());
        let undeclared = json!({ "name": "computer_job_cancel",
                                 "arguments": { "job_id": "j", "effect": "destructive" } });
        assert!(target(&all, &undeclared).is_err());
    }

    #[test]
    fn no_deferred_tool_declares_an_approval_effect() {
        for tool in super::super::mcp::all_tools() {
            let name = tool["name"].as_str().unwrap();
            if DEFERRED_TOOLS.contains(&name) {
                let props = &tool["inputSchema"]["properties"];
                assert!(props.get("effect").is_none(), "{name} declares effect");
                assert!(
                    props.get("approved_draft").is_none(),
                    "{name} declares approved_draft"
                );
            }
        }
    }

    /// AGENTS.md: a tool description says what the tool does and its one
    /// gotcha; policy lives in the instructions and situational guidance in
    /// results.
    #[test]
    fn every_description_stays_short() {
        fn properties(name: &str, schema: &Value, path: &str) {
            match schema {
                Value::Object(map) => {
                    if let Some(text) = map.get("description").and_then(Value::as_str) {
                        assert!(text.len() <= 200, "{name}{path}: {} chars", text.len());
                    }
                    for (key, value) in map {
                        properties(name, value, &format!("{path}/{key}"));
                    }
                }
                Value::Array(items) => items.iter().for_each(|v| properties(name, v, path)),
                _ => {}
            }
        }
        for tool in listed(super::super::mcp::all_tools())
            .into_iter()
            .chain(super::super::mcp::all_tools())
        {
            let name = tool["name"].as_str().unwrap();
            let text = tool["description"].as_str().unwrap();
            assert!(text.len() <= 400, "{name}: {} chars", text.len());
            properties(name, &tool["inputSchema"], "");
        }
    }
}
