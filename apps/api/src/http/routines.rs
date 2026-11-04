//! Routines over MCP (the bot manages its own, from an owner message) and
//! the owner's routes (list, and switch one on or off).

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use super::mcp::{Outcome, tool_error, tool_result};
use crate::app::routines::{
    DEFAULT_TIMEZONE, MAX_NAME_CHARS, MAX_PER_BOT, MAX_PROMPT_BYTES, RoutineName, RoutinePrompt,
    Schedule,
};
use crate::db::routines::{self, By, Caller, NewRoutine, Patch, RoutineError};
use crate::grants::Grant;
use crate::views::routine_json;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/routines", get(owner_list))
        .route("/bots/{bot_id}/routines/{routine_id}", patch(owner_update))
}

impl From<RoutineError> for ApiError {
    fn from(err: RoutineError) -> Self {
        match err {
            RoutineError::NotFound => ApiError::not_found(),
            RoutineError::BotDeleted => super::turns::bot_deleted(),
            RoutineError::Invalid(err) => ApiError::bad_request("invalid_input", err.to_string()),
            RoutineError::Db(err) => err.into(),
            other => ApiError::new(StatusCode::CONFLICT, "conflict", other.to_string()),
        }
    }
}

fn code(err: &RoutineError) -> &'static str {
    match err {
        RoutineError::NotAllowed => "NOT_ALLOWED",
        RoutineError::NotRunning => "NOT_RUNNING",
        RoutineError::NotFound => "NOT_FOUND",
        RoutineError::BotDeleted => "BOT_DELETED",
        RoutineError::NameTaken => "NAME_TAKEN",
        RoutineError::Conflict => "CREATE_CONFLICT",
        RoutineError::Limit => "LIMIT_REACHED",
        RoutineError::Invalid(_) => "INVALID_ROUTINE",
        RoutineError::Db(_) => "UNAVAILABLE",
    }
}

async fn owner_list(
    State(state): State<Arc<AppState>>,
    ApiPath(bot_id): ApiPath<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let mut tx = state.db.begin().await?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot_id)
        .fetch_optional(&mut *tx)
        .await?;
    match status.as_deref() {
        None => return Err(ApiError::not_found()),
        Some("active") => {}
        Some(_) => return Err(super::turns::bot_deleted()),
    }
    let list = routines::list(&mut tx, bot_id).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "routines": list.iter().map(routine_json).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerPatch {
    enabled: bool,
}

async fn owner_update(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, routine_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<OwnerPatch>,
) -> Result<Json<Value>, ApiError> {
    let patch = Patch {
        enabled: Some(body.enabled),
        ..Patch::default()
    };
    let routine = routines::update(
        &state.db,
        By::Owner { bot_id },
        routine_id,
        patch,
        Utc::now(),
    )
    .await?;
    Ok(Json(json!({ "routine": routine_json(&routine) })))
}

fn caller(grant: &Grant) -> Caller {
    Caller {
        bot_id: grant.bot_id,
        conversation_id: grant.conversation_id,
        turn_id: grant.turn_id,
    }
}

fn reject(err: RoutineError) -> Outcome {
    if let RoutineError::Db(err) = &err {
        tracing::warn!(error = %err, "a routine tool failed");
    }
    let code = code(&err);
    let message = match err {
        RoutineError::Db(_) => "routines are unavailable right now; try again shortly.".to_owned(),
        other => other.to_string(),
    };
    (
        tool_error(&format!("{code}: {message}")),
        Some(format!("rejected {code}")),
    )
}

fn invalid(message: &str) -> Outcome {
    (
        tool_error(&format!("INVALID_ARGUMENTS: {message}")),
        Some("rejected INVALID_ARGUMENTS".into()),
    )
}

fn annotations(read_only: bool, destructive: bool) -> Value {
    json!({ "readOnlyHint": read_only, "destructiveHint": destructive,
            "idempotentHint": true, "openWorldHint": false })
}

fn fields() -> Value {
    json!({
        "name": { "type": "string", "minLength": 1, "maxLength": MAX_NAME_CHARS,
                  "description": "Short display name, unique among your routines." },
        "prompt": { "type": "string", "minLength": 1, "maxLength": MAX_PROMPT_BYTES,
                    "description": "The complete task to run each time, self-contained: you \
                        will read it later without this conversation." },
        "cron": { "type": "string",
                  "description": "Standard five-field cron: minute hour day-of-month month \
                      day-of-week, digits with * , - / only (for example 0 9 * * 1-5 for \
                      weekdays at 09:00). Day-of-week 0 or 7 is Sunday." },
        "timezone": { "type": "string",
                      "description": "IANA time zone the cron is read in, such as \
                          Europe/Paris. Defaults to UTC." },
    })
}

pub fn create_schema() -> Value {
    json!({
        "name": "routine_create",
        "description": "Save a recurring task that runs on a schedule in this conversation. \
            Each run gives you the prompt as a scheduled routine task; you do it and report the \
            results to the owner. Returns the routine with its next run time. Repeating the same \
            call in this owner request returns the same routine; other settings for the same \
            name conflict.",
        "inputSchema": {
            "type": "object",
            "properties": fields(),
            "required": ["name", "prompt", "cron"],
            "additionalProperties": false,
        },
        "annotations": annotations(false, false),
    })
}

pub fn list_schema() -> Value {
    json!({
        "name": "routine_list",
        "description": "List your routines with their schedule, time zone, whether each is \
            enabled, the next run time, and the last run's outcome.",
        "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        "annotations": annotations(true, false),
    })
}

pub fn update_schema() -> Value {
    let mut properties = fields();
    properties["routine_id"] = json!({ "type": "string", "format": "uuid" });
    properties["enabled"] = json!({ "type": "boolean",
        "description": "False pauses future runs; true resumes counting from now." });
    json!({
        "name": "routine_update",
        "description": "Change one of your routines: pass only the fields to set. Setting the \
            values it already has changes nothing.",
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": ["routine_id"],
            "additionalProperties": false,
        },
        "annotations": annotations(false, false),
    })
}

pub fn delete_schema() -> Value {
    json!({
        "name": "routine_delete",
        "description": "Delete one of your routines. It never runs again; a run already in \
            progress finishes.",
        "inputSchema": {
            "type": "object",
            "properties": { "routine_id": { "type": "string", "format": "uuid" } },
            "required": ["routine_id"],
            "additionalProperties": false,
        },
        "annotations": annotations(false, true),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    name: String,
    prompt: String,
    cron: String,
    timezone: Option<String>,
}

fn parse_new(
    args: &CreateArgs,
    now: chrono::DateTime<Utc>,
) -> Result<(RoutineName, RoutinePrompt, Schedule), RoutineError> {
    Ok((
        RoutineName::parse(&args.name)?,
        RoutinePrompt::parse(&args.prompt)?,
        Schedule::parse_due(&args.cron, args.timezone.as_deref(), now)?,
    ))
}

pub async fn create(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Ok(args) = serde_json::from_value::<CreateArgs>(args.clone()) else {
        return invalid("pass name, prompt, and cron as strings, and optionally timezone.");
    };
    let now = Utc::now();
    let (name, prompt, schedule) = match parse_new(&args, now) {
        Ok(parsed) => parsed,
        Err(err) => return reject(err),
    };
    let new = NewRoutine {
        name: &name,
        prompt: &prompt,
        schedule: &schedule,
    };
    match routines::create(&state.db, caller(grant), new, now).await {
        Ok((routine, created)) => {
            let next = if routine.deleted {
                "This routine was deleted; do not create it again unless the owner asks."
            } else if args.timezone.is_none() {
                "Saved. It runs in UTC because no timezone was given; tell the owner the \
                 schedule and next run time, and offer to set their time zone."
            } else {
                "Saved. Tell the owner the schedule and the next run time."
            };
            let log = format!(
                "{} {} ({}) {} {}",
                if created { "created" } else { "existing" },
                routine.name,
                routine.id,
                routine.cron,
                routine.timezone
            );
            let result = json!({
                "routine": routine_json(&routine),
                "created": created,
                "deleted": routine.deleted,
                "next": next,
            });
            (tool_result(result), Some(log))
        }
        Err(err) => reject(err),
    }
}

pub async fn list(state: &AppState, grant: &Grant) -> Outcome {
    let listed = async {
        let mut conn = state.db.acquire().await?;
        routines::list(&mut conn, grant.bot_id).await
    }
    .await;
    match listed {
        Ok(list) => {
            let log = format!("{} routines", list.len());
            let result = json!({
                "routines": list.iter().map(routine_json).collect::<Vec<_>>(),
                "limit": MAX_PER_BOT,
                "default_timezone": DEFAULT_TIMEZONE,
            });
            (tool_result(result), Some(log))
        }
        Err(err) => reject(err.into()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    routine_id: Uuid,
    name: Option<String>,
    prompt: Option<String>,
    cron: Option<String>,
    timezone: Option<String>,
    enabled: Option<bool>,
}

fn parse_patch(args: &UpdateArgs) -> Result<Patch, RoutineError> {
    Ok(Patch {
        name: args.name.as_deref().map(RoutineName::parse).transpose()?,
        prompt: args
            .prompt
            .as_deref()
            .map(RoutinePrompt::parse)
            .transpose()?,
        cron: args.cron.clone(),
        timezone: args.timezone.clone(),
        enabled: args.enabled,
    })
}

pub async fn update(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Ok(args) = serde_json::from_value::<UpdateArgs>(args.clone()) else {
        return invalid("pass routine_id and only the fields to change, with their types.");
    };
    let patch = match parse_patch(&args) {
        Ok(patch) => patch,
        Err(err) => return reject(err),
    };
    match routines::update(
        &state.db,
        By::Turn(caller(grant)),
        args.routine_id,
        patch,
        Utc::now(),
    )
    .await
    {
        Ok(routine) => {
            let log = format!(
                "updated {} ({}) {} {} enabled={}",
                routine.name, routine.id, routine.cron, routine.timezone, routine.enabled
            );
            (
                tool_result(json!({ "routine": routine_json(&routine) })),
                Some(log),
            )
        }
        Err(err) => reject(err),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    routine_id: Uuid,
}

pub async fn delete(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Ok(args) = serde_json::from_value::<DeleteArgs>(args.clone()) else {
        return invalid("pass routine_id.");
    };
    match routines::delete(&state.db, caller(grant), args.routine_id).await {
        Ok(now_deleted) => {
            let log = format!(
                "{} {}",
                if now_deleted {
                    "deleted"
                } else {
                    "already deleted"
                },
                args.routine_id
            );
            let result = json!({ "routine_id": args.routine_id, "deleted": true,
                                 "already_deleted": !now_deleted });
            (tool_result(result), Some(log))
        }
        Err(err) => reject(err),
    }
}
