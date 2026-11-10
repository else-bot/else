//! Webhooks over MCP (the bot manages its own, for an owner request) and
//! the trigger route, authenticated by the webhook's own token. A pace
//! faster than the default needs the owner's approval of the exact call.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use super::mcp::{Outcome, tool_error, tool_result};
use crate::app::routines::{MAX_NAME_CHARS, MAX_PROMPT_BYTES, RoutineName, RoutinePrompt};
use crate::app::webhooks::{
    FASTEST_INTERVAL_MINUTES, MAX_DAILY_WAKES, MAX_PER_BOT, Pace, SLOWEST_INTERVAL_MINUTES,
    approval_context,
};
use crate::db::routines::{By, Caller};
use crate::db::webhooks::{self, FireError, Fired, NewWebhook, Patch, WebhookError};
use crate::grants::Grant;
use crate::views::webhook_json;

/// The owner's routes to pause, resume, and delete a bot's webhooks.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/bots/{bot_id}/webhooks/{webhook_id}",
        axum::routing::patch(owner_update).delete(owner_delete),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerPatch {
    enabled: bool,
}

fn owner_error(err: WebhookError) -> ApiError {
    match err {
        WebhookError::NotFound => ApiError::not_found(),
        WebhookError::BotDeleted => super::turns::bot_deleted(),
        WebhookError::Db(err) => err.into(),
        other => ApiError::new(StatusCode::CONFLICT, "conflict", other.to_string()),
    }
}

async fn owner_update(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, webhook_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<OwnerPatch>,
) -> Result<Json<Value>, ApiError> {
    let patch = Patch {
        enabled: Some(body.enabled),
        ..Patch::default()
    };
    let (webhook, _) = webhooks::update(&state.db, By::Owner { bot_id }, webhook_id, patch)
        .await
        .map_err(owner_error)?;
    Ok(Json(json!({ "webhook": webhook_json(&webhook) })))
}

async fn owner_delete(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, webhook_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let now_deleted = webhooks::delete(&state.db, By::Owner { bot_id }, webhook_id)
        .await
        .map_err(owner_error)?;
    Ok(Json(json!({ "webhook_id": webhook_id, "deleted": true,
                    "already_deleted": !now_deleted })))
}

/// The trigger route, nested at `/webhooks/v1`.
pub fn trigger_routes() -> Router<Arc<AppState>> {
    Router::new().route("/{webhook_id}", post(trigger))
}

async fn trigger(
    State(state): State<Arc<AppState>>,
    ApiPath(webhook_id): ApiPath<Uuid>,
    headers: HeaderMap,
) -> Response {
    let unauthorized = || {
        if let Some(retry_after) = state.auth_failures.blocked_for() {
            return ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "too_many_failures",
                "Too many failed authentication attempts.",
            )
            .retry_after(retry_after)
            .into_response();
        }
        state.auth_failures.record_failure();
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "invalid_webhook_token",
            "Unknown webhook or token.",
        )
        .into_response()
    };
    let Some(token) = super::auth::bearer(&headers) else {
        return unauthorized();
    };
    let fired = match webhooks::fire(&state.db, webhook_id, token, Utc::now()).await {
        Ok(fired) => fired,
        Err(FireError::Unauthorized) => return unauthorized(),
        Err(FireError::Db(err)) => return ApiError::from(err).into_response(),
    };
    let (status, wakes_at) = match fired {
        Fired::Requested { wakes_at } => ("requested", wakes_at),
        Fired::Coalesced { wakes_at } => ("coalesced", wakes_at),
        Fired::Disabled => {
            return ApiError::new(
                StatusCode::CONFLICT,
                "webhook_disabled",
                "This webhook is disabled.",
            )
            .into_response();
        }
    };
    state.scheduler.wake();
    (
        StatusCode::ACCEPTED,
        Json(json!({ "status": status, "wakes_at": wakes_at })),
    )
        .into_response()
}

fn caller(grant: &Grant) -> Caller {
    Caller {
        bot_id: grant.bot_id,
        conversation_id: grant.conversation_id,
        turn_id: grant.turn_id,
    }
}

fn code(err: &WebhookError) -> &'static str {
    match err {
        WebhookError::NotAllowed => "NOT_ALLOWED",
        WebhookError::NotRunning => "NOT_RUNNING",
        WebhookError::NotFound => "NOT_FOUND",
        WebhookError::BotDeleted => "BOT_DELETED",
        WebhookError::NameTaken => "NAME_TAKEN",
        WebhookError::Conflict => "CREATE_CONFLICT",
        WebhookError::Limit => "LIMIT_REACHED",
        WebhookError::Changed => "CHANGED",
        WebhookError::Invalid(_) => "INVALID_ARGUMENTS",
        WebhookError::Db(_) => "UNAVAILABLE",
    }
}

fn reject(err: WebhookError) -> Outcome {
    if let WebhookError::Db(err) = &err {
        tracing::warn!(error = %err, "a webhook tool failed");
    }
    let code = code(&err);
    let message = match err {
        WebhookError::Db(_) => "webhooks are unavailable right now; try again shortly.".to_owned(),
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

const HOW_TO_FIRE: &str = "Fire it from a background job in your computer: pass webhook_id to \
    computer_job_start, and make the script exit 0 when the event happens (any other exit \
    code does not fire). Or, from the else.bot API host or the owner's tailnet (not from your \
    computer), POST /webhooks/v1/<webhook_id> with Authorization: Bearer <token>.";

fn fields() -> Value {
    json!({
        "name": { "type": "string", "minLength": 1, "maxLength": MAX_NAME_CHARS,
                  "description": "Short display name, unique among your webhooks." },
        "prompt": { "type": "string", "minLength": 1, "maxLength": MAX_PROMPT_BYTES,
                    "description": "The complete task to do when it fires, self-contained: you \
                        will read it later without this conversation. Say how to check the \
                        event (for example: read the output of the watcher job)." },
        "min_interval_minutes": { "type": "integer", "minimum": FASTEST_INTERVAL_MINUTES,
            "maximum": SLOWEST_INTERVAL_MINUTES,
            "description": format!("Least time between two wakes. Default {}; less needs the \
                owner's approval.", Pace::DEFAULT.min_interval_minutes()) },
        "daily_limit": { "type": "integer", "minimum": 1, "maximum": MAX_DAILY_WAKES,
            "description": format!("Most wakes in any 24 hours. Default {}; more needs the \
                owner's approval.", Pace::DEFAULT.daily_limit()) },
    })
}

pub fn create_schema() -> Value {
    json!({
        "name": "webhook_create",
        "description": "Save a webhook: a task you are woken for in your owner conversation when an event \
            fires it. Events during a pending wake combine, and wakes stay min_interval_minutes \
            apart. Fire it from a job by passing webhook_id to computer_job_start and exiting 0 \
            on the event, or POST /webhooks/v1/<id> with its Bearer token from the API host or \
            tailnet. The token is shown once.",
        "inputSchema": {
            "type": "object",
            "properties": fields(),
            "required": ["name", "prompt"],
            "additionalProperties": false,
        },
        "annotations": annotations(false, false),
    })
}

pub fn list_schema() -> Value {
    json!({
        "name": "webhook_list",
        "description": "List your webhooks with their pace, whether each is enabled, a wake \
            waiting and when it may run, and the last wake's outcome.",
        "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
        "annotations": annotations(true, false),
    })
}

pub fn update_schema() -> Value {
    let mut properties = fields();
    properties["webhook_id"] = json!({ "type": "string", "format": "uuid" });
    properties["enabled"] = json!({ "type": "boolean",
        "description": "False ignores events (a waiting wake is dropped); true accepts them again." });
    properties["rotate_token"] = json!({ "type": "boolean",
        "description": "True replaces the token; the old one stops working. Returns the new one once." });
    json!({
        "name": "webhook_update",
        "description": "Change one of your webhooks: pass only the fields to set. A faster pace \
            needs the owner's approval.",
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": ["webhook_id"],
            "additionalProperties": false,
        },
        "annotations": annotations(false, false),
    })
}

pub fn delete_schema() -> Value {
    json!({
        "name": "webhook_delete",
        "description": "Delete one of your webhooks. Its token stops working and a waiting wake \
            is dropped; a wake already queued still runs. Stop any job watching for it too.",
        "inputSchema": {
            "type": "object",
            "properties": { "webhook_id": { "type": "string", "format": "uuid" } },
            "required": ["webhook_id"],
            "additionalProperties": false,
        },
        "annotations": annotations(false, true),
    })
}

/// Ask the owner before saving a pace faster than `current` and the
/// default. Returns a note for the activity log when approved.
async fn approve_pace(
    state: &AppState,
    grant: &Grant,
    tool: &str,
    args: &Value,
    name: &str,
    pace: Pace,
    current: Pace,
) -> Result<Option<&'static str>, Outcome> {
    if !pace.needs_approval_over(current) {
        return Ok(None);
    }
    super::guest_tools::approval::approve_card(
        state,
        grant,
        tool,
        args,
        crate::app::questions::Kind::External,
        format!(
            "Let the webhook \"{name}\" wake this bot every {} min, up to {} a day",
            pace.min_interval_minutes(),
            pace.daily_limit()
        ),
        approval_context(name, pace),
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateArgs {
    name: String,
    prompt: String,
    min_interval_minutes: Option<i64>,
    daily_limit: Option<i64>,
}

pub async fn create(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Ok(parsed) = serde_json::from_value::<CreateArgs>(args.clone()) else {
        return invalid(
            "pass name and prompt as strings, and optionally min_interval_minutes and \
             daily_limit as integers.",
        );
    };
    let name = match RoutineName::parse(&parsed.name) {
        Ok(name) => name,
        Err(err) => return invalid(&err.to_string()),
    };
    let prompt = match RoutinePrompt::parse(&parsed.prompt) {
        Ok(prompt) => prompt,
        Err(err) => return invalid(&err.to_string()),
    };
    let pace = match Pace::with(
        Pace::DEFAULT,
        parsed.min_interval_minutes,
        parsed.daily_limit,
    ) {
        Ok(pace) => pace,
        Err(err) => return invalid(&err.to_string()),
    };
    let retry = match webhooks::created_by(&state.db, grant.turn_id, &name).await {
        Ok(retry) => retry,
        Err(err) => return reject(err.into()),
    };
    let current = if retry { pace } else { Pace::DEFAULT };
    let note = match approve_pace(
        state,
        grant,
        "webhook_create",
        args,
        name.as_str(),
        pace,
        current,
    )
    .await
    {
        Ok(note) => note,
        Err(outcome) => return outcome,
    };
    let new = NewWebhook {
        name: &name,
        prompt: &prompt,
        pace,
    };
    match webhooks::create(&state.db, caller(grant), new).await {
        Ok((webhook, token)) => {
            let next = if webhook.deleted {
                "This webhook was deleted; do not create it again unless the owner asks.".to_owned()
            } else if token.is_none() {
                "This request already created it; its token was shown then. Use \
                 webhook_update with rotate_token if you need a new one."
                    .to_owned()
            } else {
                format!(
                    "Saved. {HOW_TO_FIRE} Keep the token out of commands and files you \
                     share. Tell the owner what will wake you and how often at most."
                )
            };
            let log = format!(
                "{} {} ({}) every {}m, {}/day{}",
                if token.is_some() {
                    "created"
                } else {
                    "existing"
                },
                webhook.name,
                webhook.id,
                webhook.pace.min_interval_minutes(),
                webhook.pace.daily_limit(),
                note.map(|n| format!(", {n}")).unwrap_or_default(),
            );
            let result = json!({
                "webhook": webhook_json(&webhook),
                "token": token,
                "trigger_path": format!("/webhooks/v1/{}", webhook.id),
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
        webhooks::list(&mut conn, grant.bot_id).await
    }
    .await;
    match listed {
        Ok(list) => {
            let log = format!("{} webhooks", list.len());
            let result = json!({
                "webhooks": list.iter().map(webhook_json).collect::<Vec<_>>(),
                "limit": MAX_PER_BOT,
                "default_pace": {
                    "min_interval_minutes": Pace::DEFAULT.min_interval_minutes(),
                    "daily_limit": Pace::DEFAULT.daily_limit(),
                },
            });
            (tool_result(result), Some(log))
        }
        Err(err) => reject(err.into()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    webhook_id: Uuid,
    name: Option<String>,
    prompt: Option<String>,
    min_interval_minutes: Option<i64>,
    daily_limit: Option<i64>,
    enabled: Option<bool>,
    #[serde(default)]
    rotate_token: bool,
}

pub async fn update(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Ok(parsed) = serde_json::from_value::<UpdateArgs>(args.clone()) else {
        return invalid("pass webhook_id and only the fields to change, with their types.");
    };
    let current = async {
        let mut conn = state.db.acquire().await?;
        webhooks::get(&mut conn, grant.bot_id, parsed.webhook_id).await
    }
    .await;
    let current = match current {
        Ok(Some(current)) => current,
        Ok(None) => return reject(WebhookError::NotFound),
        Err(err) => return reject(err.into()),
    };
    let mut patch = Patch {
        enabled: parsed.enabled,
        rotate_token: parsed.rotate_token,
        ..Patch::default()
    };
    if let Some(name) = &parsed.name {
        match RoutineName::parse(name) {
            Ok(name) => patch.name = Some(name),
            Err(err) => return invalid(&err.to_string()),
        }
    }
    if let Some(prompt) = &parsed.prompt {
        match RoutinePrompt::parse(prompt) {
            Ok(prompt) => patch.prompt = Some(prompt),
            Err(err) => return invalid(&err.to_string()),
        }
    }
    let pace = match Pace::with(
        current.pace,
        parsed.min_interval_minutes,
        parsed.daily_limit,
    ) {
        Ok(pace) => pace,
        Err(err) => return invalid(&err.to_string()),
    };
    patch.min_interval_minutes = parsed.min_interval_minutes;
    patch.daily_limit = parsed.daily_limit;
    let display = patch
        .name
        .as_ref()
        .map_or(current.name.as_str(), RoutineName::as_str)
        .to_owned();
    let note = match approve_pace(
        state,
        grant,
        "webhook_update",
        args,
        &display,
        pace,
        current.pace,
    )
    .await
    {
        Ok(note) => note,
        Err(outcome) => return outcome,
    };
    patch.approved = note.is_some().then_some(pace);
    match webhooks::update(&state.db, By::Turn(caller(grant)), parsed.webhook_id, patch).await {
        Ok((webhook, token)) => {
            let log = format!(
                "updated {} ({}) every {}m, {}/day enabled={}{}{}",
                webhook.name,
                webhook.id,
                webhook.pace.min_interval_minutes(),
                webhook.pace.daily_limit(),
                webhook.enabled,
                if token.is_some() {
                    " token rotated"
                } else {
                    ""
                },
                note.map(|n| format!(", {n}")).unwrap_or_default(),
            );
            (
                tool_result(json!({ "webhook": webhook_json(&webhook), "token": token })),
                Some(log),
            )
        }
        Err(err) => reject(err),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteArgs {
    webhook_id: Uuid,
}

pub async fn delete(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Ok(args) = serde_json::from_value::<DeleteArgs>(args.clone()) else {
        return invalid("pass webhook_id.");
    };
    match webhooks::delete(&state.db, By::Turn(caller(grant)), args.webhook_id).await {
        Ok(now_deleted) => {
            let log = format!(
                "{} {}",
                if now_deleted {
                    "deleted"
                } else {
                    "already deleted"
                },
                args.webhook_id
            );
            let result = json!({ "webhook_id": args.webhook_id, "deleted": true,
                                 "already_deleted": !now_deleted });
            (tool_result(result), Some(log))
        }
        Err(err) => reject(err),
    }
}
