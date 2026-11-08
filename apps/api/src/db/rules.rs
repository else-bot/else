//! Saved versions of each bot's rules file. A bot edits its own rules only
//! from a turn answering the owner in its owner conversation; the owner can
//! restore any earlier version. Every save appends a version.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::events;
use crate::app::providers::Origin;
use crate::app::rules::{Applied, ChangeKind, DiffLine, Edit, InvalidRules, Rules, RulesUpdate};

#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("only a turn answering the owner in this bot's own conversation can change its rules")]
    NotAllowed,
    #[error("this turn is no longer running")]
    NotRunning,
    #[error("bot not found")]
    NotFound,
    #[error("bot deleted")]
    BotDeleted,
    #[error("no such rules version")]
    NoSuchVersion,
    #[error("the rules changed since this view; reload them first")]
    Stale,
    #[error(transparent)]
    Invalid(#[from] InvalidRules),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Version {
    pub version: i64,
    pub markdown: String,
    pub section: String,
    pub change: String,
    pub owner_words: String,
    pub author: String,
    pub restored_from: Option<i64>,
    pub diff: Json<Vec<DiffLine>>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
pub struct Caller {
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub turn_id: Uuid,
}

/// The current rules file and its version, if the bot saved any.
pub async fn current(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<Option<(i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT version, markdown FROM bot_rules WHERE bot_id = ?1
         ORDER BY version DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(conn)
    .await
}

/// The current version number; 0 before the first rule.
pub async fn version(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT coalesce(max(version), 0) FROM bot_rules WHERE bot_id = ?1")
        .bind(bot_id)
        .fetch_one(conn)
        .await
}

/// Every version, newest first.
pub async fn history(
    conn: &mut SqliteConnection,
    bot_id: Uuid,
) -> Result<Vec<Version>, sqlx::Error> {
    sqlx::query_as(
        "SELECT version, markdown, section, change, owner_words, author, restored_from, diff,
             created_at
         FROM bot_rules WHERE bot_id = ?1 ORDER BY version DESC",
    )
    .bind(bot_id)
    .fetch_all(conn)
    .await
}

async fn authorize_turn(conn: &mut SqliteConnection, caller: Caller) -> Result<(), RulesError> {
    let row: Option<(Uuid, String, String, Uuid)> = sqlx::query_as(
        "SELECT t.bot_id, t.status, b.status, o.id
         FROM turns t JOIN bots b ON b.id = t.bot_id
         JOIN conversations o ON o.bot_id = t.bot_id AND o.kind = 'owner'
         WHERE t.id = ?1 AND t.conversation_id = ?2",
    )
    .bind(caller.turn_id)
    .bind(caller.conversation_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((bot, status, bot_status, owner_conversation)) = row else {
        return Err(RulesError::NotRunning);
    };
    if bot != caller.bot_id || owner_conversation != caller.conversation_id {
        return Err(RulesError::NotAllowed);
    }
    if super::delegations::origin(conn, caller.turn_id).await? != Origin::OwnerRequest {
        return Err(RulesError::NotAllowed);
    }
    if !matches!(status.as_str(), "sending" | "running") || bot_status != "active" {
        return Err(RulesError::NotRunning);
    }
    Ok(())
}

struct NewVersion<'a> {
    bot_id: Uuid,
    markdown: &'a str,
    section: &'a str,
    change: ChangeKind,
    owner_words: &'a str,
    author: &'static str,
    restored_from: Option<i64>,
    diff: &'a [DiffLine],
    turn_id: Option<Uuid>,
}

async fn append(conn: &mut SqliteConnection, new: NewVersion<'_>) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO bot_rules (bot_id, version, markdown, section, change, owner_words, author,
             restored_from, diff, turn_id)
         SELECT ?1, coalesce(max(version), 0) + 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9
         FROM bot_rules WHERE bot_id = ?1
         RETURNING version",
    )
    .bind(new.bot_id)
    .bind(new.markdown)
    .bind(new.section)
    .bind(new.change.as_str())
    .bind(new.owner_words)
    .bind(new.author)
    .bind(new.restored_from)
    .bind(Json(new.diff))
    .bind(new.turn_id)
    .fetch_one(conn)
    .await
}

/// What a bot's edit saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Saved {
    Unchanged { version: i64 },
    Saved(RulesUpdate),
}

/// Set one section of the calling bot's rules and show the owner a card in
/// the turn that asked for it. Repeating an edit saves nothing.
pub async fn update_for_turn(
    pool: &SqlitePool,
    caller: Caller,
    edit: &Edit,
    owner_words: &str,
) -> Result<Saved, RulesError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize_turn(&mut tx, caller).await?;
    let current = current(&mut tx, caller.bot_id).await?;
    let (current_version, markdown) = current.unwrap_or_default();
    let Applied::Changed {
        rules,
        section,
        change,
        diff,
    } = Rules::parse(&markdown).apply(edit)?
    else {
        return Ok(Saved::Unchanged {
            version: current_version,
        });
    };
    let version = append(
        &mut tx,
        NewVersion {
            bot_id: caller.bot_id,
            markdown: &rules.render(),
            section: &section,
            change,
            owner_words,
            author: "bot",
            restored_from: None,
            diff: &diff,
            turn_id: Some(caller.turn_id),
        },
    )
    .await?;
    let update = RulesUpdate {
        version,
        section,
        change,
        owner_words: owner_words.to_owned(),
        diff,
    };
    let seq: i64 = sqlx::query_scalar(
        "UPDATE conversations SET last_item_seq = last_item_seq + 1
         WHERE id = ?1 RETURNING last_item_seq",
    )
    .bind(caller.conversation_id)
    .fetch_one(&mut *tx)
    .await?;
    let item_id = Uuid::new_v4();
    let verb = match change {
        ChangeKind::Added => "Added",
        ChangeKind::Removed => "Removed",
        ChangeKind::Changed | ChangeKind::Restored => "Updated",
    };
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text, rules_update)
         VALUES (?1, ?2, ?3, ?4, 'status', ?5, ?6)",
    )
    .bind(item_id)
    .bind(caller.conversation_id)
    .bind(seq)
    .bind(caller.turn_id)
    .bind(format!("{verb} rule: {}", update.section))
    .bind(Json(&update))
    .execute(&mut *tx)
    .await?;
    events::item_upsert(&mut tx, caller.bot_id, item_id).await?;
    events::bot_updated(&mut tx, caller.bot_id).await?;
    tx.commit().await?;
    Ok(Saved::Saved(update))
}

/// The owner restores an earlier version as a new one; version 0 is the
/// empty file before the first rule. Restoring the current text saves
/// nothing.
/// `expected` is the version the owner was looking at; a newer version
/// refuses the restore instead of discarding changes they have not seen.
pub async fn restore(
    pool: &SqlitePool,
    bot_id: Uuid,
    version: i64,
    expected: Option<i64>,
) -> Result<i64, RulesError> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
    match status.as_deref() {
        None => return Err(RulesError::NotFound),
        Some("active") => {}
        Some(_) => return Err(RulesError::BotDeleted),
    }
    let target: Option<String> = if version == 0 {
        Some(String::new())
    } else {
        sqlx::query_scalar("SELECT markdown FROM bot_rules WHERE bot_id = ?1 AND version = ?2")
            .bind(bot_id)
            .bind(version)
            .fetch_optional(&mut *tx)
            .await?
    };
    let Some(target) = target else {
        return Err(RulesError::NoSuchVersion);
    };
    let (current_version, markdown) = current(&mut tx, bot_id).await?.unwrap_or_default();
    if expected.is_some_and(|expected| expected != current_version) {
        return Err(RulesError::Stale);
    }
    if markdown == target {
        return Ok(current_version);
    }
    let diff = crate::app::rules::document_diff(&markdown, &target);
    let saved = append(
        &mut tx,
        NewVersion {
            bot_id,
            markdown: &target,
            section: "",
            change: ChangeKind::Restored,
            owner_words: "",
            author: "owner",
            restored_from: Some(version),
            diff: &diff,
            turn_id: None,
        },
    )
    .await?;
    events::bot_updated(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(saved)
}
