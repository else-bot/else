//! Routines over MCP (the bot manages its own, for an owner request) and
//! the owner's routes (list, switch one on or off, choose its model, and
//! delete one). A model the bot names is resolved best effort: one it
//! cannot resolve never blocks saving, the routine inherits the bot's model
//! and keeps the request for the owner to see.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, patch};
use axum::{Json, Router};
use chrono::Utc;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::error::{ApiError, ApiJson, ApiPath};
use super::mcp::{Outcome, tool_error, tool_result};
use crate::app::providers::{Availability, Profile, default_profiles};
use crate::app::routines::{
    DEFAULT_TIMEZONE, MAX_NAME_CHARS, MAX_PER_BOT, MAX_PROMPT_BYTES, ModelSetting, RoutineModel,
    RoutineName, RoutinePrompt, Schedule, match_model,
};
use crate::db::routines::{self, By, Caller, NewRoutine, Patch, RoutineError};
use crate::grants::Grant;
use crate::views::routine_json;

pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/bots/{bot_id}/routines", get(owner_list))
        .route(
            "/bots/{bot_id}/routines/{routine_id}",
            patch(owner_update).delete(owner_delete),
        )
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

/// `Some(None)` for an explicit `null`; a missing field stays `None`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerPatch {
    enabled: Option<bool>,
    /// A complete choice, or `null` to inherit the bot's model again.
    #[serde(default, deserialize_with = "present")]
    model: Option<Option<super::settings::ProfileChoice>>,
}

async fn owner_update(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, routine_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<OwnerPatch>,
) -> Result<Json<Value>, ApiError> {
    let model = match body.model {
        None => None,
        Some(None) => Some(ModelSetting::Inherit { requested: None }),
        Some(Some(choice)) => {
            let provider_id = super::settings::validate_choice(&state, &choice)?;
            Some(ModelSetting::Own(RoutineModel {
                provider_id: provider_id.to_owned(),
                profile: choice.profile(),
            }))
        }
    };
    let patch = Patch {
        enabled: body.enabled,
        model,
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

async fn owner_delete(
    State(state): State<Arc<AppState>>,
    ApiPath((bot_id, routine_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let now_deleted = routines::delete(&state.db, By::Owner { bot_id }, routine_id).await?;
    Ok(Json(json!({ "routine_id": routine_id, "deleted": true,
                    "already_deleted": !now_deleted })))
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
        "model": { "type": "string", "maxLength": MAX_MODEL_CHARS,
                   "description": "Only when the owner asks for this routine to run on a \
                       specific model: its ID or name, such as gpt-6.1-sol, GPT 6.1 Sol, or \
                       Claude Opus 5.5. Omit it to run on your own model at each run. Runs on \
                       it never change your own model. A name that matches no single model \
                       never blocks saving: the routine uses your model and the result says \
                       so." },
        "provider_id": { "type": "string",
                         "description": format!(
                             "With model, only when the owner named the provider ({}).",
                             crate::providers::definitions::id_list()
                         ) },
        "reasoning_effort": { "type": "string",
                              "description": "With model, only when the owner asked for one \
                                  (low, medium, high, xhigh, max). Defaults to your usual \
                                  effort for that provider." },
        "service_tier": { "type": "string",
                          "description": "With model, only when the owner asked: priority is \
                              the Fast tier, default the standard one." },
    })
}

const MAX_MODEL_CHARS: usize = 100;

/// A model the bot asked a routine to run on, as sent.
struct ModelRequest<'a> {
    model: &'a str,
    provider_id: Option<&'a str>,
    reasoning_effort: Option<&'a str>,
    service_tier: Option<&'a str>,
}

impl ModelRequest<'_> {
    fn key(&self) -> String {
        [
            "model",
            self.model,
            self.provider_id.unwrap_or(""),
            self.reasoning_effort.unwrap_or(""),
            self.service_tier.unwrap_or(""),
        ]
        .join("\0")
    }

    fn requested(&self) -> String {
        self.model
            .trim()
            .replace('\0', "")
            .chars()
            .take(MAX_MODEL_CHARS)
            .collect()
    }
}

/// The routine's own model for `request`, or why it cannot be used.
/// Reasoning effort and service tier the bot did not name come from the
/// bot's effective profile for that provider, else the provider's seeded
/// default, else the provider's own catalog; the adapter validates them.
async fn resolve(
    state: &AppState,
    bot_id: Uuid,
    request: &ModelRequest<'_>,
) -> Result<RoutineModel, String> {
    let registry = state.scheduler.registry();
    let providers: Vec<_> = registry
        .all()
        .iter()
        .filter(|p| p.uses_profile())
        .filter(|p| request.provider_id.is_none_or(|id| p.id() == id))
        .collect();
    let catalogs: Vec<_> = providers
        .iter()
        .map(|p| (p.id(), p.catalog().models))
        .collect();
    let Some((provider_id, model)) = match_model(request.model, &catalogs) else {
        let known: Vec<&str> = catalogs
            .iter()
            .flat_map(|(_, m)| m.iter().copied())
            .collect();
        return Err(if known.is_empty() {
            "no provider here offers a model choice".to_owned()
        } else {
            format!(
                "it does not name exactly one available model ({})",
                known.join(", ")
            )
        });
    };
    let Some(provider) = providers.into_iter().find(|p| p.id() == provider_id) else {
        return Err("no provider here offers a model choice".to_owned());
    };
    if let Availability::Unavailable(why) = provider.availability() {
        return Err(format!("{} is unavailable: {why}", provider.label()));
    }
    let saved = match state.db.acquire().await {
        Ok(mut conn) => crate::db::bot_profiles::effective(&mut conn, bot_id, provider_id)
            .await
            .ok()
            .flatten()
            .map(|(_, profile)| profile.0),
        Err(_) => None,
    }
    .or_else(|| {
        default_profiles()
            .into_iter()
            .find(|(id, _)| *id == provider_id)
            .map(|(_, profile)| profile)
    });
    let catalog = provider.catalog();
    let pick = |asked: Option<&str>, saved: Option<&str>, values: &[&str], usual: &str| {
        asked
            .or(saved.filter(|v| values.contains(v)))
            .or(values.contains(&usual).then_some(usual))
            .or(values.first().copied())
            .map(str::to_owned)
    };
    let (Some(reasoning_effort), Some(service_tier)) = (
        pick(
            request.reasoning_effort,
            saved.as_ref().map(|p| p.reasoning_effort.as_str()),
            catalog.efforts,
            "medium",
        ),
        pick(
            request.service_tier,
            saved.as_ref().map(|p| p.service_tier.as_str()),
            catalog.tiers,
            "default",
        ),
    ) else {
        return Err(format!(
            "{} lists no reasoning efforts or service tiers",
            provider.label()
        ));
    };
    let profile = Profile {
        model: model.to_owned(),
        reasoning_effort,
        service_tier,
    };
    provider.validate_profile(&profile)?;
    Ok(RoutineModel {
        provider_id: provider_id.to_owned(),
        profile,
    })
}

fn describe_model(setting: &ModelSetting) -> String {
    match setting {
        ModelSetting::Own(own) => format!(
            "Each run uses {} on {} ({} reasoning, {} tier); your own model is unchanged.",
            own.profile.model,
            own.provider_id,
            own.profile.reasoning_effort,
            own.profile.service_tier,
        ),
        ModelSetting::Inherit { requested: None } => "Each run uses your current model.".to_owned(),
        ModelSetting::Inherit {
            requested: Some(requested),
        } => format!(
            "The model \"{requested}\" was not applied. The routine runs on your current model. \
             Tell the owner they can choose its model in Bot details > Routines.",
        ),
    }
}

async fn model_setting(
    state: &AppState,
    bot_id: Uuid,
    request: &ModelRequest<'_>,
) -> (ModelSetting, String) {
    match resolve(state, bot_id, request).await {
        Ok(own) => {
            let setting = ModelSetting::Own(own);
            let note = describe_model(&setting);
            (setting, note)
        }
        Err(reason) => {
            let requested = request.requested();
            let note = format!(
                "The model \"{requested}\" was not applied: {reason}. The routine is saved and \
                 runs on your current model. Tell the owner, and that they can choose its model \
                 in Bot details > Routines."
            );
            (
                ModelSetting::Inherit {
                    requested: Some(requested),
                },
                note,
            )
        }
    }
}

const EXTRAS_NEED_MODEL: &str = "provider_id, reasoning_effort, and service_tier apply only together with model; they were \
     ignored.";

pub fn create_schema() -> Value {
    json!({
        "name": "routine_create",
        "description": "Save one of your own recurring tasks; it runs on a schedule in your \
            conversation with the owner. Each run gives you the prompt as a scheduled routine \
            task; you do it and report the results to the owner. Returns the routine with its \
            next run time. Repeating the same call in this request returns the same routine; \
            other settings for the same name conflict.",
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
    properties["model"]["type"] = json!(["string", "null"]);
    properties["model"]["description"] = json!(
        "Only when the owner asks to change which model this routine runs on: its ID or name, \
         such as gpt-6.1-sol, GPT 6.1 Sol, or Claude Opus 5.5, or null to run on your own model \
         again. Omit it to keep the routine's model. A name that matches no single model is \
         not guessed: the routine uses your model and the result says so."
    );
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
    model: Option<String>,
    provider_id: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
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
        return invalid(
            "pass name, prompt, and cron as strings, and optionally timezone and model with \
             provider_id, reasoning_effort, and service_tier.",
        );
    };
    let now = Utc::now();
    let (name, prompt, schedule) = match parse_new(&args, now) {
        Ok(parsed) => parsed,
        Err(err) => return reject(err),
    };
    let request = args.model.as_deref().map(|model| ModelRequest {
        model,
        provider_id: args.provider_id.as_deref(),
        reasoning_effort: args.reasoning_effort.as_deref(),
        service_tier: args.service_tier.as_deref(),
    });
    let (model, model_note) = match &request {
        Some(request) => {
            let (setting, note) = model_setting(state, grant.bot_id, request).await;
            (setting, Some(note))
        }
        None => {
            let extras = args.provider_id.is_some()
                || args.reasoning_effort.is_some()
                || args.service_tier.is_some();
            (
                ModelSetting::Inherit { requested: None },
                extras.then(|| EXTRAS_NEED_MODEL.to_owned()),
            )
        }
    };
    let model_request = request.as_ref().map(ModelRequest::key);
    let new = NewRoutine {
        name: &name,
        prompt: &prompt,
        schedule: &schedule,
        model: &model,
        model_request: model_request.as_deref(),
    };
    match routines::create(&state.db, caller(grant), new, now).await {
        Ok((routine, created)) => {
            let model_note = if routine.model == model {
                model_note
            } else {
                Some(describe_model(&routine.model))
            };
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
                "model_note": model_note,
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
    #[serde(default, deserialize_with = "present")]
    model: Option<Option<String>>,
    provider_id: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
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
        model: None,
    })
}

pub async fn update(state: &AppState, grant: &Grant, args: &Value) -> Outcome {
    let Ok(args) = serde_json::from_value::<UpdateArgs>(args.clone()) else {
        return invalid("pass routine_id and only the fields to change, with their types.");
    };
    let mut patch = match parse_patch(&args) {
        Ok(patch) => patch,
        Err(err) => return reject(err),
    };
    let extras = args.provider_id.is_some()
        || args.reasoning_effort.is_some()
        || args.service_tier.is_some();
    let model_note = match &args.model {
        Some(Some(model)) => {
            let request = ModelRequest {
                model,
                provider_id: args.provider_id.as_deref(),
                reasoning_effort: args.reasoning_effort.as_deref(),
                service_tier: args.service_tier.as_deref(),
            };
            let (setting, note) = model_setting(state, grant.bot_id, &request).await;
            patch.model = Some(setting);
            Some(note)
        }
        Some(None) => {
            patch.model = Some(ModelSetting::Inherit { requested: None });
            Some(if extras {
                EXTRAS_NEED_MODEL.to_owned()
            } else {
                "Each run now uses your current model.".to_owned()
            })
        }
        None => extras.then(|| EXTRAS_NEED_MODEL.to_owned()),
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
                tool_result(json!({ "routine": routine_json(&routine), "model_note": model_note })),
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
    match routines::delete(&state.db, By::Turn(caller(grant)), args.routine_id).await {
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
