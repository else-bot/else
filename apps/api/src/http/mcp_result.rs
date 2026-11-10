//! What a bot reads back from a shared MCP: a compact tool index unless it
//! names the tools whose schemas it needs, and every result cut to a byte
//! budget so one large listing or document cannot fill its context.

use serde_json::{Value, json};

use super::mcp::slice;

/// Bytes of one `mcp_request` result, before the MCP envelope. A cut
/// result's whole copy is saved in the bot's project, so the preview stays
/// small: bots reread every result on each later model request.
pub const RESULT_BUDGET: usize = 10_000;
/// Bytes of a result whose whole copy could not be saved.
pub const UNSAVED_BUDGET: usize = 20_000;
/// Characters of a tool description in the index.
const INDEX_DESCRIPTION: usize = 160;

/// Bytes of tool entries in one index page; below [`RESULT_BUDGET`], which
/// also bounds the page.
const INDEX_BUDGET: usize = 9_000;
/// Bytes of named tool schemas in one response; one schema alone may use
/// up to [`SCHEMA_CAP`].
const SCHEMA_CAP: usize = 64 * 1024;
/// Marks a cursor that continues inside one upstream page.
const LOCAL_CURSOR: &str = "else:";

/// Where a `tools/list` cursor points: an offset into the upstream page
/// fetched with `upstream`.
pub struct IndexCursor {
    pub offset: usize,
    pub upstream: Option<String>,
}

impl IndexCursor {
    /// A cursor this index issued. Every cursor it issues is wrapped, so
    /// anything else is taken as an upstream cursor from the start.
    pub fn parse(cursor: Option<&str>) -> Self {
        let local = cursor
            .and_then(|c| c.strip_prefix(LOCAL_CURSOR))
            .and_then(|rest| {
                let (offset, upstream) = rest.split_once(':')?;
                Some(Self {
                    offset: offset.parse().ok()?,
                    upstream: (!upstream.is_empty()).then(|| upstream.to_owned()),
                })
            });
        local.unwrap_or(Self {
            offset: 0,
            upstream: cursor.map(str::to_owned),
        })
    }
}

/// `tools/list` without names: each tool's name and the start of its
/// description, from `at.offset` until the page budget, plus how to read
/// full schemas. A page with more tools than fit continues with a local
/// cursor; the last one passes the upstream cursor through.
pub fn tool_index(listed: Value, at: &IndexCursor) -> Value {
    let all = listed["tools"].as_array().cloned().unwrap_or_default();
    let mut tools = Vec::new();
    let mut used = 0;
    for tool in all.iter().skip(at.offset) {
        let description = tool["description"].as_str().unwrap_or_default();
        let line = description.lines().next().unwrap_or_default();
        let entry =
            json!({ "name": tool["name"], "description": slice(line, 0, INDEX_DESCRIPTION).1 });
        used += entry.to_string().len() + 1;
        if used > INDEX_BUDGET && !tools.is_empty() {
            break;
        }
        tools.push(entry);
    }
    let next = at.offset + tools.len();
    let mut index = json!({
        "tools": tools,
        "schemas": "Call tools/list again with params {\"names\": [...]} for the input schemas of the tools you will call.",
    });
    if next < all.len() {
        index["nextCursor"] = json!(format!(
            "{LOCAL_CURSOR}{next}:{}",
            at.upstream.as_deref().unwrap_or_default()
        ));
    } else if let Some(cursor) = listed["nextCursor"].as_str() {
        // Wrapped too, so an upstream cursor never parses as a local one.
        index["nextCursor"] = json!(format!("{LOCAL_CURSOR}0:{cursor}"));
    }
    index
}

/// Named schemas, whole: they fill the response budget in order, the first
/// one may reach [`SCHEMA_CAP`] alone, and the rest are named as omitted so
/// the bot can ask for them separately. Schemas are never cut.
pub fn named_response(found: Vec<Value>, names: &[String]) -> Value {
    let mut tools = Vec::new();
    let mut omitted = Vec::new();
    let mut used = 0;
    for tool in found.into_iter().map(advertised_tool) {
        let size = tool.to_string().len();
        let fits = used + size <= RESULT_BUDGET || (tools.is_empty() && size <= SCHEMA_CAP);
        if fits {
            used += size;
            tools.push(tool);
        } else {
            omitted.push(tool["name"].clone());
        }
    }
    let missing: Vec<&String> = names
        .iter()
        .filter(|n| {
            let name = n.as_str();
            !tools.iter().any(|t| t["name"] == name) && !omitted.iter().any(|o| o == name)
        })
        .collect();
    let mut response = json!({ "tools": tools, "missing": missing });
    if !omitted.is_empty() {
        response["omitted"] = json!(omitted);
        response["note"] = json!(
            "These schemas did not fit in this response; request them by name, fewer at a time."
        );
    }
    response
}

/// A named tool as the bot reads it: its input schema compacted, and its
/// output schema left out, since results arrive whole anyway. The MCP server
/// still validates every call against its own full schema.
fn advertised_tool(mut tool: Value) -> Value {
    if let Some(map) = tool.as_object_mut() {
        map.remove("outputSchema");
        if let Some(schema) = map.get_mut("inputSchema") {
            *schema = compact_schema(std::mem::take(schema));
        }
    }
    tool
}

/// Drops JSON Schema noise that generated schemas carry and that a model
/// does not need to call a tool: `$schema`, `$id`, schema titles,
/// `additionalProperties: true`, safe-integer bounds, and nested `anyOf` or
/// `oneOf`. A single-branch union is unwrapped only when its keys do not
/// collide with the parent's. Null branches stay: an optional nullable field
/// often means "omit to keep, null to clear". Literal values (`enum`,
/// `const`, `default`, `examples`) and property names are never rewritten.
fn compact_schema(schema: Value) -> Value {
    const SAFE_MAX: i64 = 9_007_199_254_740_991;
    const LITERAL: [&str; 4] = ["enum", "const", "default", "examples"];
    match schema {
        Value::Array(items) => Value::Array(items.into_iter().map(compact_schema).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                let noise = matches!(key.as_str(), "$schema" | "$id" | "title")
                    || (key == "additionalProperties" && value == Value::Bool(true))
                    || (key == "maximum" && value == json!(SAFE_MAX))
                    || (key == "minimum" && value == json!(-SAFE_MAX));
                if noise {
                    continue;
                }
                let value = match key.as_str() {
                    k if LITERAL.contains(&k) => value,
                    "properties" | "patternProperties" | "$defs" | "definitions" => match value {
                        Value::Object(props) => Value::Object(
                            props
                                .into_iter()
                                .map(|(name, prop)| (name, compact_schema(prop)))
                                .collect(),
                        ),
                        other => other,
                    },
                    _ => compact_schema(value),
                };
                out.insert(key, value);
            }
            for union in ["anyOf", "oneOf"] {
                let Some(Value::Array(branches)) = out.remove(union) else {
                    continue;
                };
                let flat: Vec<Value> = branches
                    .into_iter()
                    .flat_map(|b| match b {
                        Value::Object(ref o)
                            if o.len() == 1 && o.get(union).is_some_and(Value::is_array) =>
                        {
                            o[union].as_array().cloned().unwrap_or_default()
                        }
                        other => vec![other],
                    })
                    .collect();
                match <[Value; 1]>::try_from(flat) {
                    Ok([Value::Object(only)]) if only.keys().all(|k| !out.contains_key(k)) => {
                        out.extend(only);
                    }
                    Ok([only]) => {
                        out.insert(union.to_owned(), json!([only]));
                    }
                    Err(flat) => {
                        out.insert(union.to_owned(), Value::Array(flat));
                    }
                }
            }
            Value::Object(out)
        }
        other => other,
    }
}

/// The tools of one `tools/list` page whose names were asked for.
pub fn named_tools<'a>(page: &'a Value, names: &[String]) -> impl Iterator<Item = &'a Value> {
    page["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|tool| {
            tool["name"]
                .as_str()
                .is_some_and(|n| names.iter().any(|w| w == n))
        })
}

/// Cut `value` to `budget` serialized bytes. A tool result's structured
/// copy goes first when its text content already carries the same JSON;
/// then the longest string keeps its head, or the largest list its first
/// half, each cut marked so the bot knows to narrow its request. A value
/// that cutting cannot bring within budget becomes a bounded preview.
pub fn fit(mut value: Value, budget: usize) -> Value {
    let mut size = value.to_string().len();
    for _ in 0..64 {
        if size <= budget {
            return value;
        }
        if !drop_structured_copy(&mut value) {
            let over = size - budget;
            let (string, array) = largest(&value, String::new());
            // A string shorter than its cut marker cannot shrink the value.
            let string = string.filter(|(_, len)| *len > 256);
            let target = match (string, array) {
                (Some((path, len)), Some((list, list_size))) => {
                    if len * 2 >= over || len >= list_size / 2 {
                        Cut::Text(path, len)
                    } else {
                        Cut::List(list)
                    }
                }
                (Some((path, len)), None) => Cut::Text(path, len),
                (None, Some((list, _))) => Cut::List(list),
                (None, None) => break,
            };
            cut(&mut value, target, over);
        }
        let shrunk = value.to_string().len();
        if shrunk >= size {
            break;
        }
        size = shrunk;
    }
    if size <= budget {
        return value;
    }
    preview(&value, budget)
}

fn cut(value: &mut Value, target: Cut, over: usize) {
    match target {
        Cut::Text(path, len) => {
            let node = value.pointer_mut(&path).expect("path from largest");
            let text = node.as_str().unwrap_or_default();
            let kept = slice(text, 0, len.saturating_sub(over + 96)).1;
            *node = json!(format!(
                "{kept}… [{} bytes cut to fit; narrow the request for the rest]",
                len - kept.len()
            ));
        }
        Cut::List(path) => {
            let list = value
                .pointer_mut(&path)
                .and_then(Value::as_array_mut)
                .expect("path from largest");
            let keep = list.len() / 2;
            let dropped = list.len() - keep;
            list.truncate(keep);
            list.push(json!(format!(
                "[{dropped} more items cut to fit; page or filter the request]"
            )));
        }
    }
}

/// The start of a value's JSON as text, within `budget` once serialized.
fn preview(value: &Value, budget: usize) -> Value {
    let raw = value.to_string();
    let mut keep = budget;
    loop {
        let bounded = json!({
            "mcp_id": value.get("mcp_id").cloned().unwrap_or(Value::Null),
            "result_cut": true,
            "preview": slice(&raw, 0, keep).1,
            "note": "The result did not fit even after cutting; this is the start of its JSON. Narrow, filter, or page the request.",
        });
        if bounded.to_string().len() <= budget {
            return bounded;
        }
        if keep == 0 {
            return json!({ "result_cut": true });
        }
        keep = keep * 3 / 4;
    }
}

enum Cut {
    Text(String, usize),
    List(String),
}

pub fn drop_structured_copy(value: &mut Value) -> bool {
    let Some(result) = value.get_mut("result").and_then(Value::as_object_mut) else {
        return false;
    };
    let Some(structured) = result.get("structuredContent") else {
        return false;
    };
    let duplicated = result
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|b| {
                b["type"] == "text"
                    && b["text"]
                        .as_str()
                        .and_then(|t| serde_json::from_str::<Value>(t).ok())
                        .is_some_and(|parsed| &parsed == structured)
            })
        });
    if !duplicated {
        return false;
    }
    result.remove("structuredContent");
    result.insert("structuredContent_omitted".into(), json!(true));
    true
}

type Found = (Option<(String, usize)>, Option<(String, usize)>);

/// The JSON pointers of the longest string and of the largest list with
/// more than one item, with their sizes.
fn largest(value: &Value, path: String) -> Found {
    let better = |a: Option<(String, usize)>, b: Option<(String, usize)>| match (a, b) {
        (Some(a), Some(b)) => Some(if b.1 > a.1 { b } else { a }),
        (a, b) => a.or(b),
    };
    match value {
        Value::String(text) => (Some((path, text.len())), None),
        Value::Array(items) => {
            let own = (items.len() > 1).then(|| (path.clone(), value.to_string().len()));
            items
                .iter()
                .enumerate()
                .fold((None, own), |(s, a), (i, item)| {
                    let (is, ia) = largest(item, format!("{path}/{i}"));
                    (better(s, is), better(a, ia))
                })
        }
        Value::Object(map) => map.iter().fold((None, None), |(s, a), (key, item)| {
            let key = key.replace('~', "~0").replace('/', "~1");
            let (is, ia) = largest(item, format!("{path}/{key}"));
            (better(s, is), better(a, ia))
        }),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_keeps_names_and_a_short_description() {
        let long = "Search issues. ".repeat(40);
        let listed = json!({ "tools": [
            { "name": "search", "description": format!("{long}\nMore lines."), "inputSchema": { "type": "object" } },
            { "name": "bare", "inputSchema": { "type": "object" } },
        ], "nextCursor": "p2" });
        let index = tool_index(listed, &IndexCursor::parse(None));
        assert_eq!(index["tools"][0]["name"], "search");
        assert!(index["tools"][0]["description"].as_str().unwrap().len() <= INDEX_DESCRIPTION);
        assert_eq!(index["tools"][1]["description"], "");
        assert!(index["tools"][0].get("inputSchema").is_none());
        assert_eq!(index["nextCursor"], "else:0:p2");
    }

    #[test]
    fn named_tools_keep_their_schemas() {
        let page = json!({ "tools": [
            { "name": "a", "inputSchema": { "type": "object" } },
            { "name": "b", "inputSchema": { "type": "object" } },
        ] });
        let found: Vec<_> = named_tools(&page, &["b".into()]).collect();
        assert_eq!(found, vec![&page["tools"][1]]);
    }

    #[test]
    fn a_small_result_is_unchanged() {
        let value =
            json!({ "mcp_id": "x", "result": { "content": [{ "type": "text", "text": "ok" }] } });
        assert_eq!(fit(value.clone(), RESULT_BUDGET), value);
    }

    #[test]
    fn a_long_text_keeps_its_head_and_says_it_was_cut() {
        let value =
            json!({ "result": { "content": [{ "type": "text", "text": "é".repeat(30_000) }] } });
        let fitted = fit(value, RESULT_BUDGET);
        assert!(fitted.to_string().len() <= RESULT_BUDGET);
        let text = fitted["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("éé"));
        assert!(text.ends_with("narrow the request for the rest]"));
    }

    #[test]
    fn a_structured_copy_goes_only_when_the_text_carries_the_same_json() {
        let rows: Vec<Value> = (0..250)
            .map(|i| json!({ "id": i, "title": "row" }))
            .collect();
        let text = serde_json::to_string(&json!({ "rows": rows })).unwrap();
        let value = json!({ "result": { "content": [{ "type": "text", "text": text }], "structuredContent": { "rows": rows } } });
        let fitted = fit(value, RESULT_BUDGET);
        assert_eq!(fitted["result"]["structuredContent_omitted"], true);
        assert_eq!(fitted["result"]["content"][0]["text"], text);

        let unique = json!({ "result": {
            "content": [{ "type": "text", "text": "Created job" }],
            "structuredContent": { "jobId": "j-42", "log": "x".repeat(30_000) } } });
        let fitted = fit(unique, RESULT_BUDGET);
        assert_eq!(fitted["result"]["structuredContent"]["jobId"], "j-42");
        assert!(fitted.to_string().len() <= RESULT_BUDGET);
    }

    #[test]
    fn a_value_cutting_cannot_shrink_becomes_a_bounded_preview() {
        let short: serde_json::Map<String, Value> = (0..2_000)
            .map(|i| (format!("key{i}"), json!("value")))
            .collect();
        let fitted = fit(json!({ "mcp_id": "x", "result": short }), RESULT_BUDGET);
        assert!(fitted.to_string().len() <= RESULT_BUDGET);
        assert_eq!(fitted["result_cut"], true);
        assert_eq!(fitted["mcp_id"], "x");
        let pair = json!({ "result": [{ "a": "b".repeat(200) }, { "c": "d".repeat(200) }] });
        assert!(fit(pair, 300).to_string().len() <= 300);
    }

    #[test]
    fn a_large_page_is_indexed_in_parts_without_losing_tools() {
        let tools: Vec<Value> = (0..500)
            .map(|i| json!({ "name": format!("tool_{i}"), "description": "d".repeat(150), "inputSchema": {} }))
            .collect();
        let listed = json!({ "tools": tools, "nextCursor": "up2" });
        let mut at = IndexCursor::parse(Some("up1"));
        let mut seen = 0;
        let last = loop {
            let index = tool_index(listed.clone(), &at);
            assert!(index.to_string().len() <= RESULT_BUDGET);
            assert_eq!(index["tools"][0]["name"], format!("tool_{seen}"));
            seen += index["tools"].as_array().unwrap().len();
            let cursor = index["nextCursor"].as_str().unwrap().to_owned();
            if cursor == "else:0:up2" {
                break seen;
            }
            at = IndexCursor::parse(Some(&cursor));
            assert_eq!(at.upstream.as_deref(), Some("up1"));
        };
        assert_eq!(last, 500);
    }

    #[test]
    fn named_schemas_are_whole_or_named_as_omitted() {
        let big = json!({ "name": "big", "inputSchema": { "type": "object", "properties": {
            "kind": { "enum": (0..3_000).map(|i| format!("value_{i}")).collect::<Vec<_>>() } } } });
        let small = json!({ "name": "small", "inputSchema": { "type": "object" } });
        let names = vec!["big".to_owned(), "small".to_owned(), "gone".to_owned()];
        let response = named_response(vec![big.clone(), small.clone()], &names);
        assert_eq!(response["tools"][0], big, "never cut");
        assert_eq!(response["omitted"], json!(["small"]));
        assert_eq!(response["missing"], json!(["gone"]));
        let alone = named_response(vec![small.clone()], &["small".to_owned()]);
        assert_eq!(alone["tools"][0], small);
        assert!(alone.get("omitted").is_none());
    }

    #[test]
    fn a_long_list_keeps_its_first_items() {
        let rows: Vec<Value> = (0..2_000)
            .map(|i| json!({ "id": i, "title": "an issue" }))
            .collect();
        let fitted = fit(json!({ "result": { "issues": rows } }), RESULT_BUDGET);
        assert!(fitted.to_string().len() <= RESULT_BUDGET);
        let issues = fitted["result"]["issues"].as_array().unwrap();
        assert_eq!(issues[0]["id"], 0);
        assert!(
            issues
                .last()
                .unwrap()
                .as_str()
                .unwrap()
                .contains("more items cut to fit")
        );
    }

    #[test]
    fn an_upstream_cursor_that_looks_local_survives_the_round_trip() {
        let listed = json!({ "tools": [{ "name": "a" }], "nextCursor": "else:10:page2" });
        let index = tool_index(listed, &IndexCursor::parse(None));
        let at = IndexCursor::parse(index["nextCursor"].as_str());
        assert_eq!(
            (at.offset, at.upstream.as_deref()),
            (0, Some("else:10:page2"))
        );
    }

    #[test]
    fn a_budget_below_the_preview_envelope_still_holds() {
        let value = json!({ "result": "x".repeat(1_000) });
        assert!(fit(value, 40).to_string().len() <= 40);
    }

    #[test]
    fn named_schemas_lose_generated_noise_but_keep_their_meaning() {
        let tool = json!({ "name": "search", "outputSchema": { "type": "object" }, "inputSchema": {
            "$schema": "http://json-schema.org/draft-07/schema#", "type": "object",
            "additionalProperties": true, "required": ["query"],
            "properties": {
                "query": { "title": "Query", "type": "string" },
                "limit": { "type": "integer", "maximum": 9_007_199_254_740_991_i64 },
                "state": { "anyOf": [{ "anyOf": [{ "enum": ["open"] }, { "enum": ["closed"] }] }] },
                "cursor": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                "title": { "const": { "title": "Report" }, "default": { "$id": "kept" } },
            } } });
        let compact = advertised_tool(tool);
        assert!(compact.get("outputSchema").is_none());
        assert_eq!(
            compact["inputSchema"],
            json!({ "type": "object", "required": ["query"],
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer" },
                "state": { "anyOf": [{ "enum": ["open"] }, { "enum": ["closed"] }] },
                "cursor": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                "title": { "const": { "title": "Report" }, "default": { "$id": "kept" } },
            } }),
            "a property named title stays; literals are untouched; null branches stay"
        );
        let colliding = json!({ "type": "object", "required": ["a"],
            "oneOf": [{ "required": ["b"], "properties": { "b": { "type": "string" } } }] });
        assert_eq!(
            compact_schema(colliding.clone()),
            colliding,
            "a colliding branch stays a union"
        );
        let lone = json!({ "oneOf": [{ "type": "string" }], "description": "Id." });
        assert_eq!(
            compact_schema(lone),
            json!({ "type": "string", "description": "Id." })
        );
    }
}
