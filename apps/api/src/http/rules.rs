//! A bot's rules file: the `rules_update` tool the bot uses while answering
//! the owner, and the owner's routes to read the file and its history and
//! to restore an earlier version.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use super::mcp::{Outcome, tool_error, tool_result};
use crate::app::rules::{
    Edit, MAX_BODY_BYTES, MAX_OWNER_WORDS_CHARS, MAX_TITLE_CHARS, Rules, parse_owner_words,
};
use crate::db::rules::{self, Caller, RulesError, Saved};
use crate::grants::Grant;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/rules", get(show))
        .route("/bots/{bot_id}/rules/restore", post(restore))
}

impl From<RulesError> for ApiError {
    fn from(err: RulesError) -> Self {
        match err {
            RulesError::NotFound => ApiError::not_found(),
            RulesError::BotDeleted => super::turns::bot_deleted(),
            RulesError::NoSuchVersion => ApiError::bad_request("invalid_input", err.to_string()),
            RulesError::Invalid(err) => ApiError::bad_request("invalid_input", err.to_string()),
            RulesError::Db(err) => err.into(),
            other => ApiError::new(StatusCode::CONFLICT, "conflict", other.to_string()),
        }
    }
}

pub(super) fn schema() -> Value {
    json!({
        "name": "rules_update",
        "description": "Set one section of your rules file, which reaches you at the start of \
            every turn, routine, webhook, and event turns included. Pass the section's whole new \
            text; other sections stay. Keep text the owner wants sent or typed exactly as they \
            wrote it, in backticks, with placeholders such as {ISSUE_ID}. Empty content removes \
            the section. The owner sees each change as a card.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "section": { "type": "string", "minLength": 1, "maxLength": MAX_TITLE_CHARS,
                             "description": "Short rule title, such as \"New Linear issue\". \
                                 Reuse an existing title to change that rule." },
                "content": { "type": "string", "maxLength": MAX_BODY_BYTES,
                             "description": "The complete rule in markdown, for example \
                                 \"When: ...\\nDo: ...\\nNever: ...\". No '## ' headings." },
                "owner_words": { "type": "string", "minLength": 1,
                                 "maxLength": MAX_OWNER_WORDS_CHARS,
                                 "description": "The owner's own words this change follows, \
                                     quoted exactly." },
            },
            "required": ["section", "content", "owner_words"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    section: String,
    content: String,
    owner_words: String,
}

pub(super) async fn update(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let reject = |code: &str, message: &str| {
        (
            tool_error(&format!("{code}: {message}")),
            Some(format!("rejected {code}")),
        )
    };
    let Ok(parsed) = serde_json::from_value::<UpdateArgs>(args.clone()) else {
        return reject(
            "INVALID_ARGUMENTS",
            "pass section, content, and owner_words strings.",
        );
    };
    let edit = match Edit::parse(&parsed.section, &parsed.content) {
        Ok(edit) => edit,
        Err(err) => return reject("INVALID_RULE", &err.to_string()),
    };
    let owner_words = match parse_owner_words(&parsed.owner_words) {
        Ok(words) => words,
        Err(err) => return reject("INVALID_RULE", &err.to_string()),
    };
    let caller = Caller {
        bot_id: grant.bot_id,
        conversation_id: grant.conversation_id,
        turn_id: grant.turn_id,
    };
    match rules::update_for_turn(&state.db, caller, &edit, &owner_words).await {
        Ok(Saved::Unchanged { version }) => (
            tool_result(json!({ "version": version, "changed": false })),
            Some(format!("rules unchanged at version {version}")),
        ),
        Ok(Saved::Saved(update)) => (
            tool_result(json!({
                "version": update.version,
                "changed": true,
                "section": update.section,
                "change": update.change,
            })),
            Some(format!(
                "rules version {}: {} {}",
                update.version,
                update.change.as_str(),
                update.section
            )),
        ),
        Err(RulesError::Invalid(err)) => reject("INVALID_RULE", &err.to_string()),
        Err(err @ RulesError::NotAllowed) => reject("NOT_ALLOWED", &err.to_string()),
        Err(err @ RulesError::NotRunning) => reject("NOT_RUNNING", &err.to_string()),
        Err(err) => {
            tracing::warn!(error = %err, "saving a bot's rules failed");
            reject(
                "UNAVAILABLE",
                "rules cannot be saved right now; try again shortly.",
            )
        }
    }
}

async fn active_bot(state: &AppState, bot_id: Uuid) -> Result<(), ApiError> {
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_optional(&state.db)
        .await?;
    match status.as_deref() {
        None => Err(ApiError::not_found()),
        Some("active") => Ok(()),
        Some(_) => Err(super::turns::bot_deleted()),
    }
}

async fn show(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    active_bot(&state, bot_id).await?;
    let mut conn = state.db.acquire().await?;
    let versions = rules::history(&mut conn, bot_id).await?;
    let (version, markdown) = versions
        .first()
        .map_or((0, ""), |v| (v.version, v.markdown.as_str()));
    let sections: Vec<Value> = Rules::parse(markdown)
        .sections
        .iter()
        .map(|s| json!({ "title": s.title, "body": s.body }))
        .collect();
    let history: Vec<Value> = versions
        .iter()
        .map(|v| {
            json!({
                "version": v.version,
                "section": v.section,
                "change": v.change,
                "owner_words": v.owner_words,
                "author": v.author,
                "restored_from": v.restored_from,
                "diff": v.diff,
                "created_at": v.created_at,
            })
        })
        .collect();
    Ok(Json(json!({
        "version": version,
        "markdown": markdown,
        "sections": sections,
        "history": history,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreBody {
    version: i64,
    /// The current version the owner saw; omit to restore unconditionally.
    expected_version: Option<i64>,
}

async fn restore(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<RestoreBody>,
) -> Result<Json<Value>, ApiError> {
    if body.version < 0 {
        return Err(ApiError::bad_request(
            "invalid_input",
            "version must not be negative",
        ));
    }
    let version = rules::restore(&state.db, bot_id, body.version, body.expected_version).await?;
    Ok(Json(json!({ "version": version })))
}
