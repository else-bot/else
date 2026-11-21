//! The memory tools: which bots' memory a turn may read, and searching,
//! reading and browsing it. Recalled text is data, never instructions.

use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::mcp::{slice, wire_size};
use super::tool_defs::read_only;
use super::tool_result::{Done, Refusal, ToolResult};
use crate::app::memory_tree::NodeId;
use crate::db::memory::{self, EntryRow, RESPONSE_BUDGET};
use crate::db::memory_tree;
use crate::grants::Grant;

pub(super) const MAX_QUERY: usize = 200;
pub(super) const MAX_READ: i64 = 20;
const HIT_BODY: usize = 600;
fn fill(
    result: &mut Value,
    key: &str,
    count: usize,
    first_body: usize,
    mut make: impl FnMut(usize, usize) -> Value,
) -> usize {
    let mut added = 0;
    for index in 0..count {
        let remaining = RESPONSE_BUDGET.saturating_sub(wire_size(result));
        let mut max = first_body.min(remaining / (count - index).max(1)).max(64);
        loop {
            let item = make(index, max);
            result[key].as_array_mut().unwrap().push(item);
            let size = wire_size(result);
            if size <= RESPONSE_BUDGET {
                break;
            }
            result[key].as_array_mut().unwrap().pop();
            if max == 0 {
                return added;
            }
            max = max.saturating_sub(size - RESPONSE_BUDGET);
        }
        added += 1;
    }
    added
}

fn bot() -> Value {
    json!({ "type": "string", "format": "uuid",
            "description": "Another bot's ID from memory_bots. Omit for your own memory." })
}

pub(super) fn bots_schema() -> Value {
    json!({
        "description": "List the bots whose memory you may read: yourself, and bots the owner shared.",
        "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        "annotations": read_only(),
    })
}

pub(super) fn search_schema() -> Value {
    json!({
        "description": "Search original memory entries (messages, corrections, tool activity) by words. \
            Returns up to 20 hits, best first, with sequence numbers to read in full. Entries an owner \
            correction replaced are hidden unless include_superseded is true.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1, "maxLength": MAX_QUERY },
                "bot_id": bot(),
                "include_superseded": { "type": "boolean" },
                "cursor": { "type": "string", "description": "next_cursor from a previous page" },
            },
            "required": ["query"],
            "additionalProperties": false,
        },
        "annotations": read_only(),
    })
}

pub(super) fn read_schema() -> Value {
    json!({
        "description": "Read original memory entries in order from start_seq (count up to 20). \
            With count 1, offset pages through a long entry.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "start_seq": { "type": "integer", "minimum": 0 },
                "count": { "type": "integer", "minimum": 1, "maximum": MAX_READ },
                "offset": { "type": "integer", "minimum": 0 },
                "bot_id": bot(),
            },
            "required": ["start_seq"],
            "additionalProperties": false,
        },
        "annotations": read_only(),
    })
}

pub(super) fn view_schema() -> Value {
    json!({
        "description": "The saved overview of a bot's memory: ordered nodes, each covering a range of \
            entries (start+span), oldest first. Summaries are exact copies or joins of originals; \
            zoom into a node, or read originals, before relying on it. Reports covered and pending \
            ranges explicitly.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "bot_id": bot(),
                "from_index": { "type": "integer", "minimum": 0,
                                "description": "next_index from a previous page" },
            },
            "additionalProperties": false,
        },
        "annotations": read_only(),
    })
}

pub(super) fn zoom_schema() -> Value {
    json!({
        "description": "Open one node from memory_view or a previous zoom: its two halves, or for \
            span 1 the original entry.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "start_seq": { "type": "integer", "minimum": 0 },
                "span": { "type": "integer", "minimum": 1 },
                "bot_id": bot(),
            },
            "required": ["start_seq", "span"],
            "additionalProperties": false,
        },
        "annotations": read_only(),
    })
}

pub(super) fn date_schema() -> Value {
    json!({
        "description": "When entry seq (and optionally end_seq) was recorded, with its source.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "seq": { "type": "integer", "minimum": 0 },
                "end_seq": { "type": "integer", "minimum": 0 },
                "bot_id": bot(),
            },
            "required": ["seq"],
            "additionalProperties": false,
        },
        "annotations": read_only(),
    })
}

async fn target(state: &AppState, grant: &Grant, args: &Value) -> Result<Uuid, Refusal> {
    let target = match args.get("bot_id") {
        None | Some(Value::Null) => grant.bot_id,
        Some(value) => value
            .as_str()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| Refusal::text("bot_id must be a bot ID from memory_bots."))?,
    };
    if grant
        .memory_scope
        .as_ref()
        .is_some_and(|scope| !scope.contains(&target))
    {
        return Err(Refusal::text(
            "No readable memory for that bot. Use memory_bots.",
        ));
    }
    match memory::readable(&state.db, grant.bot_id, target).await {
        Ok(true) => Ok(target),
        Ok(false) => Err(Refusal::text(
            "No readable memory for that bot. Use memory_bots.",
        )),
        Err(err) => {
            tracing::warn!(error = %err, "memory access check failed");
            Err(Refusal::text("Memory is unavailable right now."))
        }
    }
}

const MAX_LINKS: usize = 10;

fn entry_json(bot_id: Uuid, entry: &EntryRow, offset: usize, max: usize) -> Value {
    let (from, body) = slice(&entry.body, offset, max);
    let end = from + body.len();
    let links = &entry.superseded_by[..entry.superseded_by.len().min(MAX_LINKS)];
    json!({
        "bot_id": bot_id,
        "seq": entry.seq,
        "kind": entry.kind,
        "authority": entry.authority,
        "created_at": entry.created_at,
        "source": entry.source,
        "part": { "index": entry.part_index, "count": entry.part_count },
        "supersedes_seq": entry.supersedes_seq,
        "superseded_by": links,
        "more_superseded_by": entry.superseded_by.len() > links.len(),
        "body": body,
        "body_bytes": entry.body.len(),
        "offset": from,
        "truncated": end < entry.body.len(),
        "next_offset": (end < entry.body.len()).then_some(end),
    })
}

pub(super) async fn bots(state: &AppState, grant: &Grant) -> ToolResult {
    match memory::readable_bots(&state.db, grant.bot_id).await {
        Ok(mut rows) => {
            if let Some(scope) = &grant.memory_scope {
                rows.retain(|(id, _)| scope.contains(id));
            }
            let mut result = json!({ "bots": [], "more": false });
            let shown = fill(&mut result, "bots", rows.len(), 2_000, |i, _| {
                let (id, name) = &rows[i];
                json!({ "bot_id": id, "name": name, "self": *id == grant.bot_id })
            });
            result["more"] = json!(shown < rows.len());
            let log = format!("{shown} readable bots");
            Ok(Done::json(result).logged(log))
        }
        Err(_) => Err(Refusal::text("Memory is unavailable right now.")),
    }
}

fn parse_cursor(cursor: &str) -> Option<(i64, i64)> {
    let (watermark, offset) = cursor.split_once('.')?;
    let pair = (watermark.parse().ok()?, offset.parse().ok()?);
    (pair.0 >= 0 && pair.1 >= 0).then_some(pair)
}

pub(super) async fn search(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let Some(query) = args["query"]
        .as_str()
        .map(str::trim)
        .filter(|q| !q.is_empty())
    else {
        return Err(Refusal::text("query is required."));
    };
    if query.chars().count() > MAX_QUERY {
        return Err(Refusal::text("query is too long (200 characters at most)."));
    }
    let bot_id = target(state, grant, args).await?;
    let (watermark, offset) = match args.get("cursor").and_then(Value::as_str) {
        None => (None, 0),
        Some(cursor) => match parse_cursor(cursor) {
            Some((w, o)) => (Some(w), o),
            None => {
                return Err(Refusal::text("cursor is invalid; search again without it."));
            }
        },
    };
    let include = args["include_superseded"].as_bool().unwrap_or(false);
    let page = match memory::search(&state.db, bot_id, query, include, watermark, offset).await {
        Ok(page) => page,
        Err(err) => {
            tracing::warn!(error = %err, "memory search failed");
            return Err(Refusal::text("Memory is unavailable right now."));
        }
    };
    let mut result = json!({ "bot_id": bot_id, "hits": [], "next_cursor": null });
    let shown = fill(&mut result, "hits", page.hits.len(), HIT_BODY, |i, max| {
        entry_json(bot_id, &page.hits[i], 0, max.min(HIT_BODY))
    });
    let next = if shown < page.hits.len() {
        Some(offset + shown as i64)
    } else {
        page.next_offset
    };
    let seqs: Vec<String> = page.hits[..shown]
        .iter()
        .map(|e| e.seq.to_string())
        .collect();
    let log = format!(
        "query {:?}{}: {} hits [{}]",
        query,
        if bot_id == grant.bot_id {
            String::new()
        } else {
            format!(" in bot {bot_id}")
        },
        shown,
        seqs.join(", ")
    );
    result["next_cursor"] = json!(next.map(|o| format!("{}.{}", page.watermark, o)));
    Ok(Done::json(result).logged(log))
}

pub(super) async fn read(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let Some(start) = args["start_seq"].as_i64().filter(|s| *s >= 0) else {
        return Err(Refusal::text("start_seq must be a non-negative integer."));
    };
    let count = args.get("count").and_then(Value::as_i64).unwrap_or(5);
    if !(1..=MAX_READ).contains(&count) {
        return Err(Refusal::text("count must be between 1 and 20."));
    }
    let offset = args.get("offset").and_then(Value::as_i64).unwrap_or(0);
    if offset < 0 || (offset > 0 && count != 1) {
        return Err(Refusal::text(
            "offset needs count 1 and must not be negative.",
        ));
    }
    let bot_id = target(state, grant, args).await?;
    let (rows, length) = match tokio::try_join!(
        memory::read(&state.db, bot_id, start, count),
        memory::length(&state.db, bot_id)
    ) {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(error = %err, "memory read failed");
            return Err(Refusal::text("Memory is unavailable right now."));
        }
    };
    let mut result = json!({ "bot_id": bot_id, "entries": [], "length": length,
                             "next_start_seq": null });
    let shown = fill(
        &mut result,
        "entries",
        rows.len(),
        RESPONSE_BUDGET,
        |i, max| entry_json(bot_id, &rows[i], offset as usize, max),
    );
    if shown < rows.len() {
        result["next_start_seq"] = json!(rows[shown].seq);
    }
    let log = format!(
        "read{} from seq {start}: {} entries",
        if bot_id == grant.bot_id {
            String::new()
        } else {
            format!(" bot {bot_id}")
        },
        shown
    );
    Ok(Done::json(result).logged(log))
}

fn unavailable() -> ToolResult {
    Err(Refusal::text("Memory is unavailable right now."))
}

fn suffix(grant: &Grant, bot_id: Uuid) -> String {
    if bot_id == grant.bot_id {
        String::new()
    } else {
        format!(" bot {bot_id}")
    }
}

pub(super) async fn view(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let bot_id = target(state, grant, args).await?;
    let from = args.get("from_index").and_then(Value::as_u64).unwrap_or(0) as usize;
    let Ok(saved) = memory_tree::load_view(&state.db, bot_id).await else {
        return unavailable();
    };
    let covered = (saved.published_seq >= 0)
        .then(|| json!({ "start_seq": 0, "end_seq": saved.published_seq }));
    let pending = (saved.published_seq + 1 < saved.length)
        .then(|| json!({ "start_seq": saved.published_seq + 1, "end_seq": saved.length - 1 }));
    let blocked: Vec<Value> = saved
        .blocked
        .iter()
        .map(|(reason, jobs)| json!({ "reason": reason, "jobs": jobs }))
        .collect();
    let mut result = json!({
        "bot_id": bot_id,
        "generation": saved.generation,
        "revision": saved.revision,
        "valid": saved.valid,
        "length": saved.length,
        "covered": covered,
        "pending": pending,
        "blocked": blocked,
        "compacting": saved.compacting,
        "nodes": [],
        "next_index": null,
    });
    let rest = saved.nodes.get(from..).unwrap_or_default();
    let shown = fill(&mut result, "nodes", rest.len(), 4_000, |i, max| {
        let node = &rest[i];
        let mut item = json!({
            "start_seq": node.node.start,
            "span": node.node.span,
            "method": node.method,
        });
        if let Some(body) = node.body.as_deref() {
            text_fields(&mut item, node.node, body, max);
        }
        item
    });
    if shown < rest.len() {
        result["next_index"] = json!(from + shown);
    }
    let log = format!(
        "view{} generation {} revision {}: {} nodes from {}",
        suffix(grant, bot_id),
        saved.generation,
        saved.revision,
        shown,
        from
    );
    Ok(Done::json(result).logged(log))
}

fn text_fields(item: &mut Value, node: NodeId, body: &str, max: usize) {
    let text = slice(body, 0, max).1;
    item["text"] = json!(text);
    item["text_bytes"] = json!(body.len());
    item["truncated"] = json!(text.len() < body.len());
    if text.len() < body.len() {
        item["continue_with"] = if node.span == 1 {
            json!({ "tool": "memory_read", "start_seq": node.start, "count": 1, "offset": text.len() })
        } else {
            json!({ "tool": "memory_zoom", "start_seq": node.start, "span": node.span })
        };
    }
}

pub(super) async fn zoom(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let (Some(start), Some(span)) = (args["start_seq"].as_i64(), args["span"].as_i64()) else {
        return Err(Refusal::text("start_seq and span are required."));
    };
    let id = NodeId { start, span };
    if !id.is_valid() {
        return Err(Refusal::text(
            "span must be a power of two and start_seq a multiple of it.",
        ));
    }
    let bot_id = target(state, grant, args).await?;
    let Ok(saved) = memory_tree::load_view(&state.db, bot_id).await else {
        return unavailable();
    };
    if id.end() > saved.length {
        return Err(Refusal::text("That range is beyond the end of memory."));
    }
    let generation = saved.generation;
    let mut result = json!({ "bot_id": bot_id, "generation": generation,
                             "start_seq": start, "span": span, "parts": [] });
    match id.children() {
        None => {
            let Ok(rows) = memory::read(&state.db, bot_id, start, 1).await else {
                return unavailable();
            };
            let Some(entry) = rows.first() else {
                return Err(Refusal::text("No such entry."));
            };
            fill(
                &mut result,
                "parts",
                1,
                RESPONSE_BUDGET,
                |_, max| json!({ "original": entry_json(bot_id, entry, 0, max) }),
            );
        }
        Some((left, right)) => {
            let mut parts = Vec::new();
            for child in [left, right] {
                let Ok(built) = memory_tree::node(&state.db, bot_id, generation, child).await
                else {
                    return unavailable();
                };
                parts.push((child, built));
            }
            fill(&mut result, "parts", 2, 6_000, |i, max| {
                let (child, built) = &parts[i];
                match built {
                    Some((body, method)) => {
                        let mut item = json!({
                            "start_seq": child.start, "span": child.span, "built": true,
                            "method": method,
                        });
                        text_fields(&mut item, *child, body, max);
                        item
                    }
                    None => json!({ "start_seq": child.start, "span": child.span, "built": false }),
                }
            });
        }
    }
    let log = format!("zoom{} {start}+{span}", suffix(grant, bot_id));
    Ok(Done::json(result).logged(log))
}

pub(super) async fn date(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let Some(seq) = args["seq"].as_i64().filter(|s| *s >= 0) else {
        return Err(Refusal::text("seq must be a non-negative integer."));
    };
    let end = args.get("end_seq").and_then(Value::as_i64);
    if end.is_some_and(|e| e < seq) {
        return Err(Refusal::text("end_seq must not be before seq."));
    }
    let bot_id = target(state, grant, args).await?;
    let mut dates = Vec::new();
    for at in std::iter::once(seq).chain(end) {
        let Ok(rows) = memory::read(&state.db, bot_id, at, 1).await else {
            return unavailable();
        };
        match rows.first().filter(|e| e.seq == at) {
            Some(e) => dates.push(json!({
                "seq": e.seq, "created_at": e.created_at, "kind": e.kind,
                "authority": e.authority, "source": e.source,
            })),
            None => return Err(Refusal::text("No such entry.")),
        }
    }
    let log = format!(
        "date{} {seq}{}",
        suffix(grant, bot_id),
        end.map(|e| format!("..{e}")).unwrap_or_default()
    );
    Ok(Done::json(json!({ "bot_id": bot_id, "dates": dates })).logged(log))
}
