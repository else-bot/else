//! Tool search over else.bot's rarely used tools. A turn lists the tools in
//! [`TOOL_NAMES`] directly; those in [`DEFERRED_TOOLS`] stay out of every
//! turn's tool definitions until `find_tool` returns their schema, and run
//! through `use_tool` with the same grant checks as a direct call.

use serde_json::{Value, json};

use super::tool_defs::definition;
use crate::app::tools::{DEFERRED_TOOLS, TOOL_NAMES, TOOLS, ToolDef};
pub use crate::app::tools::{FIND, USE};

/// Tools one `find_tool` call returns.
const MAX_FOUND: usize = 5;

/// The tool definitions a turn lists: the direct ones, then the two search
/// tools.
pub fn listed() -> Vec<Value> {
    let mut tools: Vec<Value> = TOOLS
        .iter()
        .filter(|tool| tool.is_listed())
        .map(definition)
        .collect();
    tools.extend(schemas());
    tools
}

/// The deferred tools' definitions, in row order.
pub fn deferred() -> impl Iterator<Item = (&'static ToolDef, Value)> {
    TOOLS
        .iter()
        .filter(|tool| !tool.is_listed())
        .map(|tool| (tool, definition(tool)))
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

/// The deferred tools that best match `query`: only the ones it names
/// exactly when it names any, else by words shared with each tool's name
/// and description.
pub fn find(query: &str) -> Value {
    let query = query.to_lowercase();
    let bare = query.trim().trim_start_matches("mcp__elsebot__");
    if TOOL_NAMES.contains(&bare) {
        return json!({ "tools": [], "listed": format!(
            "{bare} is in your tool list, as mcp__elsebot__{bare} where your tools carry that \
             prefix: call it directly."
        ) });
    }
    let all: Vec<(&ToolDef, Value)> = deferred().collect();
    let named: Vec<&Value> = all
        .iter()
        .filter(|(tool, _)| {
            query
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|token| token.trim_start_matches("mcp__elsebot__") == tool.name)
        })
        .map(|(_, definition)| definition)
        .collect();
    if !named.is_empty() {
        return json!({ "tools": named });
    }
    let words: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .collect();
    let mut scored: Vec<(usize, &Value)> = all
        .iter()
        .filter_map(|(tool, definition)| {
            let name = tool.name;
            let description = definition["description"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase();
            let score: usize = words
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
                .sum();
            (score > 0).then_some((score, definition))
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
pub fn target(args: &Value) -> Result<(&'static ToolDef, &Value), &'static str> {
    let name = args["name"].as_str().unwrap_or_default();
    let tool = match ToolDef::named(name) {
        Some(tool) if !tool.is_listed() => tool,
        Some(_) => return Err(LISTED),
        None if TOOL_NAMES.contains(&name) => return Err(LISTED),
        None => return Err("Name a tool that find_tool returned. Nothing was done."),
    };
    let Value::Object(arguments) = &args["arguments"] else {
        return Err("Give the tool's arguments as an object. Nothing was done.");
    };
    let definition = definition(tool);
    let declared = definition["inputSchema"]["properties"].as_object();
    if arguments
        .keys()
        .any(|key| declared.is_none_or(|props| !props.contains_key(key)))
    {
        return Err(
            "Use only the arguments in the tool's input schema from find_tool. Nothing was done.",
        );
    }
    Ok((tool, &args["arguments"]))
}

const LISTED: &str = "That tool is in your tool list: call it directly. Nothing was done.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tools::ToolId;

    #[test]
    fn find_returns_matching_deferred_schemas_only() {
        let exact = find("routine_create");
        assert_eq!(exact["tools"][0]["name"], "routine_create");
        assert!(exact["tools"][0]["inputSchema"].is_object());
        let words = find("save a recurring schedule");
        let names: Vec<&str> = words["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"routine_create"), "{names:?}");
        assert!(
            find("computer_run")["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t["name"] != "computer_run")
        );
        assert_eq!(find("zzzz")["available"], json!(DEFERRED_TOOLS));
        let listed = find("delegate");
        assert!(
            listed["listed"]
                .as_str()
                .unwrap()
                .contains("call it directly")
        );
    }

    #[test]
    fn find_returns_only_the_deferred_tools_a_query_names() {
        let names = |query: &str| -> Vec<String> {
            find(query)["tools"]
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
        assert_eq!(
            target(&json!({ "name": "routine_list", "arguments": {} }))
                .map(|(tool, args)| (tool.id, args)),
            Ok((ToolId::RoutineList, &json!({})))
        );
        assert!(target(&json!({ "name": "ask_permission", "arguments": {} })).is_err());
        assert!(target(&json!({ "name": "nope", "arguments": {} })).is_err());
        assert!(target(&json!({ "name": "routine_list", "arguments": [] })).is_err());
        let undeclared = json!({ "name": "computer_job_cancel",
                                 "arguments": { "job_id": "j", "effect": "destructive" } });
        assert!(target(&undeclared).is_err());
    }

    #[test]
    fn no_deferred_tool_declares_an_approval_effect() {
        for (tool, definition) in deferred() {
            let name = tool.name;
            let props = &definition["inputSchema"]["properties"];
            assert!(props.get("effect").is_none(), "{name} declares effect");
            assert!(
                props.get("approved_draft").is_none(),
                "{name} declares approved_draft"
            );
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
        for tool in listed()
            .into_iter()
            .chain(deferred().map(|(_, definition)| definition))
        {
            let name = tool["name"].as_str().unwrap();
            let text = tool["description"].as_str().unwrap();
            assert!(text.len() <= 400, "{name}: {} chars", text.len());
            properties(name, &tool["inputSchema"], "");
        }
    }
}
