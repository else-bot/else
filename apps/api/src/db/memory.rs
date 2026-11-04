//! Each bot's immutable original memory log (POC-013).
//!
//! Appends run inside the caller's transaction, so a memory entry commits
//! with the conversation change it records. The per-bot `memory_store` row
//! is protected by immediate transactions: appends are serialized and
//! a rolled-back append consumes no sequence. Originals are immutable.

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

/// Longest single entry body. Longer originals are split into parts.
pub const MAX_PART_BYTES: usize = 64_000;
/// Search hits per page.
pub const SEARCH_PAGE: i64 = 20;
/// Serialized result budget for one retrieval response.
pub const RESPONSE_BUDGET: usize = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    OwnerMessage,
    AssistantMessage,
    OwnerCorrection,
    ToolActivity,
    /// A task another bot delegated: model-authored, never owner text.
    DelegatedTask,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::OwnerMessage => "owner_message",
            Kind::AssistantMessage => "assistant_message",
            Kind::OwnerCorrection => "owner_correction",
            Kind::ToolActivity => "tool_activity",
            Kind::DelegatedTask => "delegated_task",
        }
    }

    /// Authority follows from the kind: only the owner's own words are
    /// owner authority, so model or tool text can never become one.
    pub fn authority(self) -> &'static str {
        match self {
            Kind::OwnerMessage | Kind::OwnerCorrection => "owner",
            Kind::AssistantMessage | Kind::DelegatedTask => "assistant",
            Kind::ToolActivity => "tool",
        }
    }
}

pub struct NewEntry<'a> {
    pub request_key: &'a str,
    pub kind: Kind,
    pub body: &'a str,
    pub source: Value,
    /// Only for `OwnerCorrection`: the owner entry it replaces.
    pub supersedes_seq: Option<i64>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppendError {
    #[error("the request key was already used for different content")]
    Conflict,
    #[error("the bot is not active")]
    BotInactive,
    #[error("a correction must supersede an earlier owner entry of the same bot")]
    InvalidCorrection,
    #[error("an empty entry cannot be saved")]
    Empty,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Split at UTF-8 boundaries into parts of at most `max` bytes.
pub fn split(body: &str, max: usize) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = body;
    while rest.len() > max {
        let mut cut = max;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        let (head, tail) = rest.split_at(cut);
        parts.push(head);
        rest = tail;
    }
    parts.push(rest);
    parts
}

fn fingerprint(entry: &NewEntry<'_>) -> Vec<u8> {
    let canonical = json!({
        "kind": entry.kind.as_str(),
        "body": entry.body,
        "source": entry.source,
        "supersedes_seq": entry.supersedes_seq,
    });
    Sha256::digest(canonical.to_string().as_bytes()).to_vec()
}

/// Append one original (possibly as several parts) and return its sequence
/// numbers. Runs in the caller's transaction.
pub async fn append(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
    entry: &NewEntry<'_>,
) -> Result<Vec<i64>, AppendError> {
    if entry.body.is_empty() {
        return Err(AppendError::Empty);
    }
    if (entry.kind == Kind::OwnerCorrection) != entry.supersedes_seq.is_some() {
        return Err(AppendError::InvalidCorrection);
    }
    // The caller's immediate transaction fences concurrent deletion and appends.
    let active: Option<bool> =
        sqlx::query_scalar("SELECT status = 'active' FROM bots WHERE id = ?1")
            .bind(bot_id)
            .fetch_optional(&mut *conn)
            .await?;
    if active != Some(true) {
        return Err(AppendError::BotInactive);
    }
    sqlx::query("INSERT INTO memory_store (bot_id) VALUES (?1) ON CONFLICT DO NOTHING")
        .bind(bot_id)
        .execute(&mut *conn)
        .await?;
    let (next, generation, building): (i64, i32, Option<i32>) = sqlx::query_as(
        "SELECT next_seq, generation, building_generation FROM memory_store
         WHERE bot_id = ?1",
    )
    .bind(bot_id)
    .fetch_one(&mut *conn)
    .await?;
    // The first generation takes the installation's current recipe.
    sqlx::query(
        "INSERT INTO memory_generation (bot_id, generation, recipe)
         VALUES (?1, 1, coalesce((SELECT memory_recipe FROM installation), ?2))
         ON CONFLICT DO NOTHING",
    )
    .bind(bot_id)
    .bind(sqlx::types::Json(crate::app::memory_tree::Recipe::default()))
    .execute(&mut *conn)
    .await?;
    let print = fingerprint(entry);
    let existing: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT seq, request_fingerprint FROM memory_entry
         WHERE bot_id = ?1 AND request_key = ?2 ORDER BY part_index",
    )
    .bind(bot_id)
    .bind(entry.request_key)
    .fetch_all(&mut *conn)
    .await?;
    if let Some((_, stored)) = existing.first() {
        if *stored != print {
            return Err(AppendError::Conflict);
        }
        return Ok(existing.into_iter().map(|(seq, _)| seq).collect());
    }
    if let Some(target) = entry.supersedes_seq {
        let authority: Option<String> =
            sqlx::query_scalar("SELECT authority FROM memory_entry WHERE bot_id = ?1 AND seq = ?2")
                .bind(bot_id)
                .bind(target)
                .fetch_optional(&mut *conn)
                .await?;
        if authority.as_deref() != Some("owner") {
            return Err(AppendError::InvalidCorrection);
        }
    }
    let original: Vec<u8> = Sha256::digest(entry.body.as_bytes()).to_vec();
    let parts = split(entry.body, MAX_PART_BYTES);
    let count = parts.len() as i32;
    let mut seqs = Vec::with_capacity(parts.len());
    for (index, part) in parts.into_iter().enumerate() {
        let seq = next + index as i64;
        sqlx::query(
            "INSERT INTO memory_entry (bot_id, seq, request_key, request_fingerprint, kind,
                 authority, body, body_sha256, source, part_index, part_count, original_sha256,
                 supersedes_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .bind(bot_id)
        .bind(seq)
        .bind(entry.request_key)
        .bind(&print)
        .bind(entry.kind.as_str())
        .bind(entry.kind.authority())
        .bind(part)
        .bind(Sha256::digest(part.as_bytes()).to_vec())
        .bind(&entry.source)
        .bind(index as i32)
        .bind(count)
        .bind(&original)
        .bind(entry.supersedes_seq)
        .execute(&mut *conn)
        .await?;
        seqs.push(seq);
    }
    sqlx::query("UPDATE memory_store SET next_seq = ?2 WHERE bot_id = ?1")
        .bind(bot_id)
        .bind(next + seqs.len() as i64)
        .execute(&mut *conn)
        .await?;
    // Each new entry needs its leaf in the active generation and in a
    // rebuild in progress.
    let generations: Vec<i32> = std::iter::once(generation).chain(building).collect();
    sqlx::query(
        "INSERT INTO memory_job (bot_id, generation, start_seq, span)
         SELECT ?1, g.value, s.value, 1 FROM json_each(?2) g, json_each(?3) s WHERE true
         ON CONFLICT DO NOTHING",
    )
    .bind(bot_id)
    .bind(sqlx::types::Json(&generations))
    .bind(sqlx::types::Json(&seqs))
    .execute(&mut *conn)
    .await?;
    Ok(seqs)
}

/// Log a conversation item's final text, once. Missing or empty items and
/// inactive bots are skipped: memory follows visible history.
pub async fn log_item(conn: &mut SqliteConnection, item_id: Uuid) -> Result<(), sqlx::Error> {
    let row: Option<(Uuid, Uuid, Uuid, String, String, Option<String>, String)> = sqlx::query_as(
        "SELECT c.bot_id, i.conversation_id, i.turn_id, i.kind, i.text, i.provider_id, c.kind
         FROM items i JOIN conversations c ON c.id = i.conversation_id WHERE i.id = ?1",
    )
    .bind(item_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((bot_id, conversation_id, turn_id, kind, text, provider_id, conversation_kind)) = row
    else {
        return Ok(());
    };
    let kind = match kind.as_str() {
        // The first message of a task conversation is the delegated task.
        "user" if conversation_kind == "task" => Kind::DelegatedTask,
        "user" => Kind::OwnerMessage,
        "assistant" => Kind::AssistantMessage,
        _ => return Ok(()),
    };
    let key = format!("item:{item_id}");
    let entry = NewEntry {
        request_key: &key,
        kind,
        body: &text,
        source: json!({
            "conversation_id": conversation_id,
            "item_id": item_id,
            "turn_id": turn_id,
            "provider_id": provider_id,
        }),
        supersedes_seq: None,
    };
    match append(conn, bot_id, &entry).await {
        Ok(_) | Err(AppendError::Empty | AppendError::BotInactive) => Ok(()),
        // Text never changes after its turn finishes, so a conflict means
        // the item was logged earlier with its final text.
        Err(AppendError::Conflict) => Ok(()),
        Err(AppendError::Db(err)) => Err(err),
        Err(AppendError::InvalidCorrection) => unreachable!("items are never corrections"),
    }
}

/// Log the assistant text of a turn that has just ended.
pub async fn log_turn_end(conn: &mut SqliteConnection, turn_id: Uuid) -> Result<(), sqlx::Error> {
    let items: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM items WHERE turn_id = ?1 AND kind = 'assistant' ORDER BY seq",
    )
    .bind(turn_id)
    .fetch_all(&mut *conn)
    .await?;
    for item in items {
        log_item(conn, item).await?;
    }
    Ok(())
}

/// Whether `reader` may read `target`'s memory: the target is active, and
/// it is the reader itself or the owner granted the read.
pub async fn readable(pool: &SqlitePool, reader: Uuid, target: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1 FROM bots b WHERE b.id = ?2 AND b.status = 'active'
               AND (?1 = ?2 OR EXISTS (
                   SELECT 1 FROM memory_read_grants g
                   WHERE g.reader_bot_id = ?1 AND g.target_bot_id = ?2)))",
    )
    .bind(reader)
    .bind(target)
    .fetch_one(pool)
    .await
}

/// Active bots `reader` may read, itself first.
pub async fn readable_bots(
    pool: &SqlitePool,
    reader: Uuid,
) -> Result<Vec<(Uuid, String, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT b.id, b.name, b.role FROM bots b
         WHERE b.status = 'active' AND (b.id = ?1 OR EXISTS (
             SELECT 1 FROM memory_read_grants g WHERE g.reader_bot_id = ?1 AND g.target_bot_id = b.id))
         ORDER BY b.id <> ?1, b.created_at, b.id",
    )
    .bind(reader)
    .fetch_all(pool)
    .await
}

#[derive(Debug, sqlx::FromRow)]
pub struct EntryRow {
    pub seq: i64,
    pub kind: String,
    pub authority: String,
    pub body: String,
    pub source: Value,
    pub part_index: i32,
    pub part_count: i32,
    pub supersedes_seq: Option<i64>,
    #[sqlx(json)]
    pub superseded_by: Vec<i64>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Entry columns; `bound` limits which corrections count (a search page
/// sees only corrections below its watermark).
fn entry_columns(bound: &str) -> String {
    format!(
        "e.seq, e.kind, e.authority, e.body, e.source, e.part_index,
         e.part_count, e.supersedes_seq,
         coalesce((SELECT json_group_array(c.seq ORDER BY c.seq) FROM memory_entry c
                   WHERE c.bot_id = e.bot_id AND c.supersedes_seq = e.seq {bound}), '[]')
             AS superseded_by,
         e.created_at"
    )
}

/// Entries from `start_seq` on, in order.
pub async fn read(
    pool: &SqlitePool,
    bot_id: Uuid,
    start_seq: i64,
    count: i64,
) -> Result<Vec<EntryRow>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM memory_entry e JOIN memory_search ON memory_search.rowid = e.rowid
         WHERE e.bot_id = ?1 AND e.seq >= ?2 ORDER BY e.seq LIMIT ?3",
        entry_columns("")
    )))
    .bind(bot_id)
    .bind(start_seq)
    .bind(count)
    .fetch_all(pool)
    .await
}

/// The number of entries, which is also the next sequence number.
pub async fn length(pool: &SqlitePool, bot_id: Uuid) -> Result<i64, sqlx::Error> {
    let next: Option<i64> =
        sqlx::query_scalar("SELECT next_seq FROM memory_store WHERE bot_id = ?1")
            .bind(bot_id)
            .fetch_optional(pool)
            .await?;
    Ok(next.unwrap_or(0))
}

pub struct SearchPage {
    pub hits: Vec<EntryRow>,
    /// Entries at or after this sequence are not considered by this search
    /// and its continuation pages, so pages stay consistent while the log grows.
    pub watermark: i64,
    pub next_offset: Option<i64>,
}

/// Lexical search over originals with the fixed `simple` configuration,
/// ordered by rank, then newest first. Entries replaced by an explicit owner
/// correction are left out unless `include_superseded`.
pub async fn search(
    pool: &SqlitePool,
    bot_id: Uuid,
    query: &str,
    include_superseded: bool,
    watermark: Option<i64>,
    offset: i64,
) -> Result<SearchPage, sqlx::Error> {
    let watermark = match watermark {
        Some(w) => w,
        None => length(pool, bot_id).await?,
    };
    let mut hits: Vec<EntryRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM memory_entry e JOIN memory_search ON memory_search.rowid = e.rowid
         WHERE e.bot_id = ?1 AND e.seq < ?2
           AND memory_search MATCH ?3
           AND (?4 OR NOT EXISTS (SELECT 1 FROM memory_entry c
                                  WHERE c.bot_id = e.bot_id AND c.supersedes_seq = e.seq
                                    AND c.seq < ?2))
         ORDER BY memory_search.rank, e.seq DESC
         LIMIT ?6 OFFSET ?5",
        entry_columns("AND c.seq < ?2")
    )))
    .bind(bot_id)
    .bind(watermark)
    .bind(search_query(query))
    .bind(include_superseded)
    .bind(offset)
    .bind(SEARCH_PAGE + 1)
    .fetch_all(pool)
    .await?;
    let more = hits.len() as i64 > SEARCH_PAGE;
    hits.truncate(SEARCH_PAGE as usize);
    let next_offset = more.then_some(offset + SEARCH_PAGE);
    Ok(SearchPage {
        hits,
        watermark,
        next_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_respects_utf8_boundaries_and_limits() {
        let text = "é".repeat(10); // 20 bytes
        let parts = split(&text, 7);
        assert!(parts.iter().all(|p| p.len() <= 7 && !p.is_empty()));
        assert_eq!(parts.concat(), text);
        assert_eq!(split("short", 64), vec!["short"]);
        assert_eq!(split("", 64), vec![""]);
    }

    #[test]
    fn only_owner_kinds_carry_owner_authority() {
        for kind in [Kind::AssistantMessage, Kind::ToolActivity] {
            assert_ne!(kind.authority(), "owner");
        }
        assert_eq!(Kind::OwnerCorrection.authority(), "owner");
    }
}

/// Quote user input rather than exposing FTS operators. Preserve web-search
/// phrases, OR alternatives, and exclusions; malformed quotes remain safe.
fn search_query(query: &str) -> String {
    let mut tokens = Vec::new();
    let mut chars = query.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        let mut token = String::new();
        token.push(c);
        if c == '"' || (c == '-' && chars.peek() == Some(&'"')) {
            if c == '-' {
                token.push(chars.next().unwrap());
            }
            for c in chars.by_ref() {
                token.push(c);
                if c == '"' {
                    break;
                }
            }
        } else {
            while chars.peek().is_some_and(|c| !c.is_whitespace()) {
                token.push(chars.next().unwrap());
            }
        }
        tokens.push(token);
    }
    let quote = |s: &str| format!("\"{}\"", s.trim_matches('"').replace('"', "\"\""));
    let mut alternatives = Vec::new();
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for token in tokens.into_iter().chain(std::iter::once("OR".into())) {
        if token == "OR" {
            if !positive.is_empty() {
                let mut group = positive.join(" AND ");
                for term in negative.drain(..) {
                    group = format!("({group}) NOT {term}");
                }
                alternatives.push(format!("({group})"));
            }
            positive.clear();
            negative.clear();
        } else if let Some(term) = token.strip_prefix('-').filter(|s| !s.is_empty()) {
            negative.push(quote(term));
        } else if token.chars().any(char::is_alphanumeric) {
            positive.push(quote(&token));
        }
    }
    if alternatives.is_empty() {
        "\"\"".into()
    } else {
        alternatives.join(" OR ")
    }
}
