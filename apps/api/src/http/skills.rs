//! Skill tools for bots and owner routes for install cards and installed
//! skills. Every `/v1` route needs the owner token; task grants never
//! reach them.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiPath, SecretJson};
use super::mcp::{Outcome, tool_error, tool_result};
use crate::db::skills::{self, RequestError, Scope, SettleError, Settled, Status};
use crate::grants::Grant;
use crate::skills::{FetchError, valid_id};

pub const INSTALL: &str = "skill_install";
pub const READ: &str = "skill_read";
const MAX_READ: usize = 64 * 1024;

pub(super) fn schemas() -> Vec<Value> {
    vec![
        json!({
            "name": INSTALL,
            "description": "Install an Agent Skill from a github.com link to its directory (one holding SKILL.md) when the owner asks. \
                The owner reviews its files, picks which bots get it, and can enter its keys in a card. Returns parked: end the attempt.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "minLength": 1, "maxLength": 2048,
                             "description": "github.com link to the skill directory or its SKILL.md." },
                    "reason": { "type": "string", "minLength": 1, "maxLength": skills::MAX_REASON,
                                "description": "Shown to the owner: why install it." },
                    "env": { "type": "array", "maxItems": 16, "uniqueItems": true,
                             "items": { "type": "string", "pattern": "^[A-Z_][A-Z0-9_]{0,63}$" },
                             "description": "Key variables the skill's docs say it needs, e.g. TREG_TOKEN. Names only." },
                },
                "required": ["url", "reason"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false,
                             "idempotentHint": true, "openWorldHint": true },
        }),
        json!({
            "name": READ,
            "description": "Read an installed skill: without path, its SKILL.md and file list; with path, one of its files.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Skill id from the installed skills list." },
                    "path": { "type": "string", "description": "File path inside the skill, e.g. scripts/run.py." },
                },
                "required": ["id"],
                "additionalProperties": false,
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false,
                             "idempotentHint": true, "openWorldHint": false },
        }),
    ]
}

pub(super) async fn call(state: &AppState, grant: &Grant, name: &str, args: &Value) -> Outcome {
    match name {
        INSTALL => install(state, grant, args).await,
        _ => read(state, grant, args).await,
    }
}

fn env_names(args: &Value) -> Result<Vec<String>, &'static str> {
    let Some(list) = args.get("env") else {
        return Ok(Vec::new());
    };
    let list = list
        .as_array()
        .filter(|l| l.len() <= crate::db::task_secrets::MAX_VARS)
        .ok_or("env lists up to 16 variable names")?;
    let mut names = std::collections::BTreeSet::new();
    for var in list {
        let var = var
            .as_str()
            .filter(|v| crate::db::task_secrets::valid_var(v))
            .ok_or("each env entry is an allowed variable name (A-Z, 0-9, _); never send values")?;
        if !names.insert(var.to_owned()) {
            return Err("env names must be distinct");
        }
    }
    Ok(names.into_iter().collect())
}

async fn install(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let refuse = |code: &str, message: &str| {
        (
            tool_error(&format!("{code}: {message}")),
            Some(format!("rejected {code}")),
        )
    };
    let Some(url) = args["url"].as_str().filter(|u| u.len() <= 2048) else {
        return refuse("INVALID_REQUEST", "url is a github.com link");
    };
    let Some(reason) = args["reason"]
        .as_str()
        .map(str::trim)
        .filter(|r| !r.is_empty() && r.len() <= skills::MAX_REASON && !r.contains('\0'))
    else {
        return refuse("INVALID_REQUEST", "reason is 1 to 500 bytes of text");
    };
    let env = match env_names(args) {
        Ok(env) => env,
        Err(message) => return refuse("INVALID_REQUEST", message),
    };
    let package = match crate::skills::fetch(&state.skill_sources, url).await {
        Ok(package) => package,
        Err(err) => {
            let code = match err {
                FetchError::Invalid(_) => "INVALID_SKILL",
                FetchError::NotFound(_) => "NOT_FOUND",
                FetchError::Unavailable(_) => "UNAVAILABLE",
            };
            return refuse(code, &err.to_string());
        }
    };
    match skills::create(&state.db, grant.turn_id, reason, &env, &package).await {
        Ok(card) if card.status != Status::Pending => {
            let result = json!({
                "status": card.status,
                "request_id": card.id,
                "skill": card.skill_id,
                "next": if card.status == Status::Installed {
                    "The owner installed this skill; use it with skill_read."
                } else {
                    "The owner declined this skill; continue without it and do not ask again unless they do."
                },
            });
            (
                tool_result(result),
                Some(format!("already {:?} {}", card.status, card.id).to_lowercase()),
            )
        }
        Ok(card) => {
            state.scheduler.grants().park(grant.turn_id).await;
            let result = json!({
                "status": "pending",
                "parked": true,
                "request_id": card.id,
                "skill": card.skill_id,
                "commit": card.source.commit,
                "files": card.files.len(),
                "next": "Tell the owner briefly what the skill does and end this attempt now. Do not \
                         wait, poll, or call more tools; the answer arrives in a continuation.",
            });
            (
                tool_result(result),
                Some(format!("pending {} for {}", card.id, card.skill_id)),
            )
        }
        Err(err) => {
            let code = match err {
                RequestError::NotAllowed => "NOT_ALLOWED",
                RequestError::AttemptParked => "ATTEMPT_PARKED",
                RequestError::BudgetExhausted => "BUDGET_EXHAUSTED",
                RequestError::AlreadyInstalled(..) => "ALREADY_INSTALLED",
                RequestError::NotRunning => "NOT_RUNNING",
                RequestError::Db => "UNAVAILABLE",
            };
            refuse(code, &err.to_string())
        }
    }
}

async fn read(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Some(id) = args["id"].as_str().filter(|id| valid_id(id)) else {
        return (
            tool_error("INVALID_REQUEST: id is an installed skill id"),
            None,
        );
    };
    let found = match skills::get(&state.db, id).await {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(error = %err, "reading a skill failed");
            return (tool_error("UNAVAILABLE: the skill could not be read"), None);
        }
    };
    let Some((skill, files)) =
        found.filter(|(s, _)| s.bot_id.is_none_or(|owner| owner == grant.bot_id))
    else {
        return (
            tool_error("NOT_FOUND: no installed skill with that id for this bot"),
            None,
        );
    };
    let path = args["path"].as_str().unwrap_or("SKILL.md");
    let Some(content) = files.get(path) else {
        return (
            tool_error(&format!(
                "NOT_FOUND: {path} is not in this skill; files: {}",
                files.keys().cloned().collect::<Vec<_>>().join(", ")
            )),
            None,
        );
    };
    let (truncated, content) = if content.len() > MAX_READ {
        (true, super::mcp::slice(content, 0, MAX_READ).1)
    } else {
        (false, content.as_str())
    };
    let mut result = json!({
        "id": skill.id,
        "path": path,
        "content": content,
        "truncated": truncated,
    });
    if args.get("path").is_none() {
        result["files"] = json!(skill.files);
        result["commit"] = json!(skill.source.commit);
        if !skill.env.is_empty() {
            result["keys"] = json!({
                "secret": skill.secret,
                "env": skill.env,
                "stored": skill.key_stored,
            });
        }
    }
    (
        tool_result(result),
        Some(format!("read {} {path}", skill.id)),
    )
}

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/settings/skills", get(list))
        .route("/settings/skills/{id}", get(show).delete(remove))
        .route("/bots/{bot_id}/skill-installs/{id}", get(review))
        .route("/bots/{bot_id}/skill-installs/{id}/approve", post(approve))
        .route("/bots/{bot_id}/skill-installs/{id}/decline", post(decline))
}

async fn list(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(json!({ "skills": skills::list(&state.db).await? })))
}

async fn show(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<String>,
) -> Result<Json<Value>, ApiError> {
    let (skill, contents) = skills::get(&state.db, &id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({ "skill": skill, "contents": contents })))
}

async fn remove(
    State(state): State<Arc<AppState>>,
    ApiPath(id): ApiPath<String>,
) -> Result<StatusCode, ApiError> {
    if skills::delete(&state.db, &id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

fn settle_error(err: SettleError) -> ApiError {
    match err {
        SettleError::NotFound => ApiError::not_found(),
        SettleError::Settled(status) => ApiError::new(
            StatusCode::CONFLICT,
            "already_answered",
            format!(
                "This request was already {}.",
                serde_json::to_value(status)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or_default()
            ),
        ),
        err @ SettleError::EnvMismatch(_) => ApiError::bad_request("env_mismatch", err.to_string()),
        err @ SettleError::Exists(_) => {
            ApiError::new(StatusCode::CONFLICT, "skill_exists", err.to_string())
        }
        SettleError::Secret(err) => super::task_secrets::error(err),
    }
}

fn settled(state: &AppState, settled: Settled) -> Json<Value> {
    if settled.requeued {
        state.scheduler.wake();
    }
    Json(json!({ "install": settled.install, "requeued": settled.requeued }))
}

async fn review(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let (install, contents) = skills::review(&state.db, bot_id, id)
        .await
        .map_err(settle_error)?;
    Ok(Json(json!({ "install": install, "contents": contents })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    scope: Scope,
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
}

/// Install a card's skill for the chosen bots, store its keys when given,
/// and continue the waiting turn. Key values are never returned.
async fn approve(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, id)): ApiPath<(Uuid, Uuid)>,
    SecretJson(body): SecretJson<Approval>,
) -> Result<Json<Value>, ApiError> {
    let done = skills::approve(
        &state.db,
        state.secret_key.as_deref(),
        bot_id,
        id,
        body.scope,
        body.env.as_ref(),
    )
    .await
    .map_err(settle_error)?;
    Ok(settled(&state, done))
}

async fn decline(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let done = skills::decline(&state.db, bot_id, id)
        .await
        .map_err(settle_error)?;
    Ok(settled(&state, done))
}
