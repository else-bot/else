//! The tools that list, create, rename, re-model and delete bots.

use else_wire::settings::ProfileChoice;
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::owner_tools::{Card, Declined, ask};
use super::tool_defs::read_only;
use super::tool_result::{self, Done, Refusal, ToolResult};
use crate::grants::Grant;

#[derive(serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ListBotsArgs {
    category_id: Option<Uuid>,
}

pub(super) fn list_schema() -> Value {
    json!({
        "description": "Discover all other active bots and categories, including the main bot ID. Filter by category_id to find council members. This does not grant access to their private memory.",
        "inputSchema": {"type": "object", "properties": {"category_id": {"type": "string", "format": "uuid"}}, "additionalProperties": false},
        "annotations": read_only(),
    })
}

pub(super) fn create_schema() -> Value {
    json!({
        "description": "Register a new else.bot specialist for an owner request, with no shared memory or \
            access. Pass its purpose, which is saved to its memory, and model only when the \
            owner named one. Returns its bot_id at once: delegate its first task to it in this \
            attempt. Repeating the same name in this request returns the same bot. category_id \
            comes from list_bots.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "name": { "type": "string", "minLength": 1,
                          "maxLength": crate::app::bots::MAX_NAME_CHARS,
                          "description": "Short display name, for example GitHub." },
                "purpose": { "type": "string", "minLength": 1, "maxLength": crate::app::bots::MAX_PURPOSE_CHARS },
                "category_id": { "type": "string", "format": "uuid" },
                "model": { "type": "string", "maxLength": super::routines::MAX_MODEL_CHARS,
                           "description": "Only when the owner named the model: its ID or name, such as grok-4.7 or Claude Opus \
                               5.5; it selects the provider. Omit it for the default. An unmatched name creates \
                               nothing; the error lists models." },
                "provider_id": { "type": "string",
                                 "description": format!(
                                     "With model, only when the owner named the provider ({}).",
                                     crate::providers::definitions::id_list()
                                 ) },
                "reasoning_effort": { "type": "string",
                                      "description": "With model, only when the owner asked for \
                                          one (low, medium, high, xhigh, max)." },
                "service_tier": { "type": "string",
                                  "description": "With model, only when the owner asked: \
                                      priority is the Fast tier, default the standard one." },
            },
            "required": ["name"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) fn rename_schema() -> Value {
    json!({
        "description": "Rename an existing else.bot when asked, including yourself or another bot. \
            Available to any bot for work rooted in an owner request. Use the exact target_bot_id \
            from your registry, task, or a create_bot result. Changes only the display name; \
            repeating the same name changes nothing. Confirm the saved name from the result.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target_bot_id": { "type": "string", "format": "uuid" },
                "name": { "type": "string", "minLength": 1,
                          "maxLength": crate::app::bots::MAX_NAME_CHARS },
            },
            "required": ["target_bot_id", "name"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) fn model_schema() -> Value {
    json!({
        "description": "Set an else.bot's own model, reasoning effort, and service tier when the owner asks, \
            including yours or another bot's, by exact target_bot_id. Its next turn uses it; a \
            running turn and routines with their own model keep theirs. An unmatched model \
            changes nothing and the error lists models. Confirm the saved model from the result.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target_bot_id": { "type": "string", "format": "uuid" },
                "model": { "type": "string", "minLength": 1,
                           "maxLength": super::routines::MAX_MODEL_CHARS,
                           "description": "Its ID or name, such as gpt-6.1-sol or Claude Opus 5.5; it selects the provider. Omit to keep the bot's model." },
                "provider_id": { "type": "string",
                                 "description": format!(
                                     "With model, only when the owner named the provider ({}).",
                                     crate::providers::definitions::id_list()
                                 ) },
                "reasoning_effort": { "type": "string",
                                      "description": "low, medium, high, xhigh, or max. Omit to keep the bot's current effort for that provider." },
                "service_tier": { "type": "string",
                                  "description": "priority is the Fast tier, default the standard one. Omit to keep the bot's current tier." },
            },
            "required": ["target_bot_id"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": false,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) fn delete_schema() -> Value {
    json!({
        "description": "Ask the owner to delete another else.bot by exact target_bot_id, for an owner request. \
            The owner always approves on a card listing its routines, triggers and tasks that \
            stop; this parks. Once approved, call it again with the same target_bot_id. Never \
            yourself or the main bot.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "target_bot_id": { "type": "string", "format": "uuid" },
                "reason": { "type": "string", "maxLength": crate::app::questions::MAX_TITLE,
                            "description": "Why, in the owner's words. Shown on the card." },
            },
            "required": ["target_bot_id"],
            "additionalProperties": false,
        },
        "annotations": { "readOnlyHint": false, "destructiveHint": true,
                         "idempotentHint": true, "openWorldHint": false },
    })
}

pub(super) async fn list_bots(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    let parsed = if args.is_null() {
        Ok(ListBotsArgs::default())
    } else {
        serde_json::from_value(args.clone())
    };
    let Ok(parsed) = parsed else {
        return Err(Refusal::text(
            "Pass an optional category_id from list_bots.",
        ));
    };
    match crate::db::bots::list(&state.db, false).await {
        Ok(listing) => {
            if parsed
                .category_id
                .is_some_and(|id| !listing.categories.iter().any(|c| c.id == id))
            {
                return Err(Refusal::text(
                    "CATEGORY_NOT_FOUND: Use a category_id from list_bots.",
                ));
            }
            let bots: Vec<_> = listing
                .bots
                .iter()
                .filter(|b| {
                    b.id != grant.bot_id
                        && parsed
                            .category_id
                            .is_none_or(|id| b.category_id == Some(id))
                })
                .map(|b| json!({"bot_id": b.id, "name": b.name, "category_id": b.category_id}))
                .collect();
            Ok(Done::json(
                json!({"main_bot_id": listing.default_bot_id, "categories": listing.categories,
                "bots": bots}),
            ))
        }
        Err(_) => Err(Refusal::text("UNAVAILABLE: Bot discovery is unavailable.")),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBotArgs {
    name: String,
    purpose: Option<String>,
    category_id: Option<Uuid>,
    model: Option<String>,
    provider_id: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

pub(super) async fn create_bot(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    use crate::app::bots::{BotName, BotPurpose};
    use crate::db::bots::{self, BotError, Created, Refusal as BotRefusal};
    let parsed: CreateBotArgs = tool_result::args(args, || {
        Refusal::coded(
            "INVALID_ARGUMENTS",
            "pass a name, and optionally purpose, category_id, and model with provider_id, \
             reasoning_effort, and service_tier.",
        )
    })?;
    let name = match BotName::parse(&parsed.name) {
        Ok(name) => name,
        Err(err) => return Err(Refusal::coded("INVALID_BOT", err.to_string())),
    };
    let purpose = match parsed.purpose.as_deref().map(BotPurpose::parse).transpose() {
        Ok(purpose) => purpose,
        Err(err) => return Err(Refusal::coded("INVALID_BOT", err)),
    };
    let model = match parsed.model.as_deref() {
        Some(model) => {
            let request = super::routines::ModelRequest {
                model,
                provider_id: parsed.provider_id.as_deref(),
                reasoning_effort: parsed.reasoning_effort.as_deref(),
                service_tier: parsed.service_tier.as_deref(),
            };
            match super::routines::resolve(state, None, &request).await {
                Ok(model) => Some((request.key(), model)),
                Err(reason) => {
                    return Err(Refusal::coded(
                        "MODEL_NOT_AVAILABLE",
                        format!(
                            "the model \"{}\" was not applied: {reason}. No bot was created; \
                             retry with an available model or tell the owner.",
                            request.requested()
                        ),
                    ));
                }
            }
        }
        None if parsed.provider_id.is_some()
            || parsed.reasoning_effort.is_some()
            || parsed.service_tier.is_some() =>
        {
            return Err(Refusal::coded(
                "INVALID_ARGUMENTS",
                "provider_id, reasoning_effort, and service_tier apply only together with model.",
            ));
        }
        None => None,
    };
    let model = model.as_ref().map(|(request, model)| bots::TurnModel {
        request,
        provider_id: &model.provider_id,
        profile: &model.profile,
    });
    match bots::create_for_turn(
        &state.db,
        grant.turn_id,
        &name,
        purpose.as_ref(),
        model,
        parsed.category_id,
    )
    .await
    {
        Ok(created) => {
            let (bot, new) = match created {
                Created::New(bot) => (bot, true),
                Created::Existing(bot) => (bot, false),
            };
            let next = match bot.status {
                bots::BotStatus::Active => {
                    "It is registered. Delegate to this bot_id when it should work; it can use \
                     the task secrets stored on this server but has no other access until \
                     the owner grants it."
                }
                _ => "The owner deleted this bot; do not delegate to it or create it again.",
            };
            let log = format!(
                "{} {} ({}) {:?}",
                if new { "created" } else { "existing" },
                bot.name,
                bot.id,
                bot.status
            );
            let mut result = json!({
                "bot_id": bot.id,
                "name": bot.name,
                "status": bot.status,
                "created": new,
                "next": next,
            });
            if let Ok(Some(own)) = crate::db::bot_profiles::get(&state.db, bot.id).await {
                result["model"] = json!(ProfileChoice::from(&own));
            }
            Ok(Done::json(result).logged(log))
        }
        Err(BotError::Refused(refusal)) => {
            let code = match refusal {
                BotRefusal::NotOwnerConversation | BotRefusal::NotOwnerRequest => "NOT_ALLOWED",
                BotRefusal::PurposeRequired => "PURPOSE_REQUIRED",
                BotRefusal::NotRunning => "NOT_RUNNING",
                BotRefusal::AttemptDelegated => "ATTEMPT_PARKED",
                BotRefusal::OwnBot => "OWN_BOT",
                BotRefusal::Upstream => "UPSTREAM_BOT",
            };
            Err(Refusal::coded(code, refusal.to_string()))
        }
        Err(BotError::RequestConflict) => Err(Refusal::coded(
            "REQUEST_CONFLICT",
            "This name was already used with another purpose, category, or model in this request.",
        )),
        Err(BotError::CategoryNotFound) => Err(Refusal::coded(
            "CATEGORY_NOT_FOUND",
            "Use a category_id from list_bots.",
        )),
        Err(err) => {
            tracing::warn!(error = %err, "creating a bot from a turn failed");
            Err(Refusal::coded(
                "UNAVAILABLE",
                "bots cannot be created right now; try again shortly.",
            ))
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBotArgs {
    target_bot_id: Uuid,
    name: String,
}

pub(super) async fn rename_bot(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    use crate::app::bots::BotName;
    use crate::db::bots;
    let parsed: RenameBotArgs = tool_result::args(args, || {
        Refusal::coded(
            "INVALID_ARGUMENTS",
            "pass a target_bot_id UUID and a name string.",
        )
    })?;
    let name = match BotName::parse(&parsed.name) {
        Ok(name) => name,
        Err(err) => return Err(Refusal::coded("INVALID_BOT", err.to_string())),
    };
    match bots::rename_for_turn(&state.db, grant.turn_id, parsed.target_bot_id, &name).await {
        Ok(bot) => {
            let log = format!("renamed {} ({})", bot.name, bot.id);
            Ok(
                Done::json(json!({ "bot_id": bot.id, "name": bot.name, "status": bot.status }))
                    .logged(log),
            )
        }
        Err(err) => {
            let Some(code) = edit_refusal(&err) else {
                tracing::warn!(error = %err, "renaming a bot from a turn failed");
                return Err(Refusal::coded(
                    "UNAVAILABLE",
                    "bots cannot be renamed right now; try again shortly.",
                ));
            };
            Err(Refusal::coded(code, err.to_string()))
        }
    }
}

/// The provider and model a bot's next turn would use, if it has one.
async fn current_model(
    state: &AppState,
    bot_id: Uuid,
) -> Result<Option<(String, String)>, sqlx::Error> {
    let Some(provider) = crate::db::bot_profiles::provider_for(&state.db, bot_id).await? else {
        return Ok(None);
    };
    let mut conn = state.db.acquire().await?;
    let profile = crate::db::bot_profiles::effective(&mut conn, bot_id, &provider).await?;
    Ok(profile.map(|(_, profile)| (provider, profile.0.model)))
}

/// The tool error code for a refused bot edit, or `None` for a failure to log.
fn edit_refusal(err: &crate::db::bots::BotError) -> Option<&'static str> {
    use crate::db::bots::{BotError, Refusal};
    Some(match err {
        BotError::NotFound => "BOT_NOT_FOUND",
        BotError::Deleted => "BOT_UNAVAILABLE",
        BotError::Refused(
            Refusal::NotOwnerConversation | Refusal::NotOwnerRequest | Refusal::PurposeRequired,
        ) => "NOT_ALLOWED",
        BotError::Refused(Refusal::NotRunning) => "NOT_RUNNING",
        BotError::Refused(Refusal::AttemptDelegated) => "ATTEMPT_PARKED",
        BotError::Refused(Refusal::OwnBot) => "OWN_BOT",
        BotError::Refused(Refusal::Upstream) => "UPSTREAM_BOT",
        BotError::MainBot => "MAIN_BOT",
        _ => return None,
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SetBotModelArgs {
    target_bot_id: Uuid,
    model: Option<String>,
    provider_id: Option<String>,
    reasoning_effort: Option<String>,
    service_tier: Option<String>,
}

pub(super) async fn set_bot_model(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    use crate::db::bots;
    let parsed: SetBotModelArgs = tool_result::args(args, || {
        Refusal::coded(
            "INVALID_ARGUMENTS",
            "pass a target_bot_id UUID, and model, provider_id, reasoning_effort, or \
             service_tier.",
        )
    })?;
    let current = match parsed.model {
        Some(_) => None,
        None => match current_model(state, parsed.target_bot_id).await {
            Ok(Some(current)) => Some(current),
            Ok(None) => {
                return Err(Refusal::coded(
                    "MODEL_REQUIRED",
                    "this bot has no model yet; pass the model the owner wants.",
                ));
            }
            Err(err) => {
                tracing::warn!(error = %err, "reading a bot's model failed");
                return Err(Refusal::coded(
                    "UNAVAILABLE",
                    "bot models cannot be changed right now; try again shortly.",
                ));
            }
        },
    };
    let request = super::routines::ModelRequest {
        model: parsed
            .model
            .as_deref()
            .or(current.as_ref().map(|(_, model)| model.as_str()))
            .unwrap_or_default(),
        provider_id: parsed
            .provider_id
            .as_deref()
            .or(current.as_ref().map(|(provider, _)| provider.as_str())),
        reasoning_effort: parsed.reasoning_effort.as_deref(),
        service_tier: parsed.service_tier.as_deref(),
    };
    let model = match super::routines::resolve(state, Some(parsed.target_bot_id), &request).await {
        Ok(model) => model,
        Err(reason) => {
            return Err(Refusal::coded(
                "MODEL_NOT_AVAILABLE",
                format!(
                    "the model \"{}\" was not applied: {reason}. Nothing changed; retry with an \
                     available model or tell the owner.",
                    request.requested()
                ),
            ));
        }
    };
    match bots::set_model_for_turn(
        &state.db,
        grant.turn_id,
        parsed.target_bot_id,
        &model.provider_id,
        &model.profile,
    )
    .await
    {
        Ok(saved) => {
            let log = format!(
                "set {} to {} on {} ({} reasoning, {} tier)",
                parsed.target_bot_id,
                saved.profile.model,
                saved.provider_id,
                saved.profile.reasoning_effort,
                saved.profile.service_tier,
            );
            Ok(Done::json(json!({
                "bot_id": parsed.target_bot_id,
                "model": ProfileChoice::from(&saved),
                "next": "Its next turn uses this model. Routines with their own model keep \
                    it; to change one, delegate to that bot asking it to update the routine.",
            }))
            .logged(log))
        }
        Err(err) => {
            let Some(code) = edit_refusal(&err) else {
                tracing::warn!(error = %err, "setting a bot's model from a turn failed");
                return Err(Refusal::coded(
                    "UNAVAILABLE",
                    "bot models cannot be changed right now; try again shortly.",
                ));
            };
            Err(Refusal::coded(code, err.to_string()))
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteBotArgs {
    target_bot_id: Uuid,
    reason: Option<String>,
}

pub(super) async fn delete_bot(state: &AppState, grant: &Grant, args: &Value) -> ToolResult {
    use crate::app::questions::{BotDeletion, MAX_TITLE, Spec};
    use crate::db::bots::{self, Deletion};
    let parsed = serde_json::from_value::<DeleteBotArgs>(args.clone())
        .ok()
        .filter(|parsed| parsed.reason.as_ref().is_none_or(|r| r.len() <= MAX_TITLE));
    let Some(parsed) = parsed else {
        return Err(Refusal::coded(
            "INVALID_ARGUMENTS",
            "pass a target_bot_id UUID and optionally a reason of at most 500 bytes.",
        ));
    };
    let target = parsed.target_bot_id;
    let (bot, impact) = match bots::deletion_for_turn(&state.db, grant.turn_id, target).await {
        Ok(Deletion::Active(bot, impact)) => (bot, impact),
        Ok(Deletion::Ended(status)) => return already_deleted(target, status),
        Err(err) => {
            let Some(code) = edit_refusal(&err) else {
                tracing::warn!(error = %err, "reading a bot to delete failed");
                return Err(Refusal::coded(
                    "UNAVAILABLE",
                    "bots cannot be deleted right now; try again shortly.",
                ));
            };
            return Err(Refusal::coded(code, err.to_string()));
        }
    };
    let model = match current_model(state, target).await {
        Ok(model) => model.map(|(_, model)| model),
        Err(err) => {
            tracing::warn!(error = %err, "reading a bot's model for its deletion card failed");
            None
        }
    };
    let reason = parsed
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty());
    let spec = Spec::bot_deletion(
        BotDeletion {
            bot_id: target,
            name: bot.name.clone(),
            avatar: bot.avatar(),
            model,
            category: impact.category.clone(),
        },
        deletion_context(reason, &impact),
    );
    let card = Card::approval(
        "Tell the owner briefly which bot this deletes and why, then end this attempt now. Do \
         not wait, poll, or call more tools. If they approve, call delete_bot again with the \
         same target_bot_id.",
    );
    let declined = Declined::Status(
        "The owner denied deleting this bot. Do not delete it or ask again in this request.",
    );
    ask(state, grant, &spec, card, declined).await?;
    match super::bots::delete_bot(state, target).await {
        Ok(deleted) => Ok(Done::json(json!({
            "bot_id": target,
            "name": deleted.name,
            "status": deleted.status,
            "removed": {
                "routines": impact.routines,
                "webhooks": impact.webhooks,
                "mail_triggers": impact.mail_triggers,
                "social_triggers": impact.social_triggers,
                "cancelled_tasks": impact.tasks.len(),
                "memory_readers": impact.memory_readers,
            },
            "next": deleted_next(deleted.status),
        }))
        .logged(format!("deleted {} ({target})", deleted.name))),
        Err(err) => {
            tracing::warn!(error = %err, "deleting a bot from a turn failed");
            Err(Refusal::coded(
                "UNAVAILABLE",
                "the bot was not deleted; tell the owner they can delete it in the app.",
            ))
        }
    }
}

fn deleted_next(status: crate::db::bots::BotStatus) -> &'static str {
    match status {
        crate::db::bots::BotStatus::Deleted => {
            "Tell the owner the bot is deleted and what stopped. Its history and memory stay \
             on this machine."
        }
        _ => {
            "Tell the owner the bot is being deleted and what stopped: its running work is \
             stopping and it finishes deleting on its own. Its history and memory stay on \
             this machine."
        }
    }
}

fn already_deleted(target: Uuid, status: crate::db::bots::BotStatus) -> ToolResult {
    Ok(Done::json(json!({
            "bot_id": target,
            "status": status,
            "next": match status {
                crate::db::bots::BotStatus::Deleted => "This bot is already deleted. Nothing changed.",
                _ => "This bot is already being deleted; its running work is stopping. Nothing changed.",
            },
        })).logged(format!("{target} already {status:?}")))
}

fn deletion_context(reason: Option<&str>, impact: &crate::db::bots::Impact) -> String {
    const TASK_CHARS: usize = 120;
    const TAIL: &str = "Its chat history and memory stay on this machine. Its computer stops. \
                        It cannot be restored from the app.";
    let mut text = String::new();
    if let Some(reason) = reason {
        text.push_str(reason);
        text.push_str("\n\n");
    }
    let mut stops = Vec::new();
    for (label, names) in [
        ("Routine", &impact.routines),
        ("Webhook", &impact.webhooks),
        ("Mail trigger", &impact.mail_triggers),
        ("Social trigger", &impact.social_triggers),
    ] {
        stops.extend(names.iter().map(|name| format!("- {label}: {name}")));
    }
    for task in &impact.tasks {
        let direction = if task.incoming { "from" } else { "to" };
        let line = task.task.split_whitespace().collect::<Vec<_>>().join(" ");
        let short = match line.char_indices().nth(TASK_CHARS) {
            Some((end, _)) => format!("{}…", &line[..end]),
            None => line,
        };
        stops.push(format!("- Task {direction} {}: {short}", task.with));
    }
    if stops.is_empty() {
        text.push_str("Nothing scheduled or in progress stops.\n");
    } else {
        text.push_str("Stops:\n");
        text.push_str(&stops.join("\n"));
        text.push('\n');
    }
    if !impact.memory_readers.is_empty() {
        text.push_str(&format!(
            "These bots lose access to its memory: {}\n",
            impact.memory_readers.join(", ")
        ));
    }
    let room = crate::app::questions::MAX_CONTEXT - TAIL.len() - "…\n".len();
    if text.len() > room {
        text.truncate(text.floor_char_boundary(room));
        text.push_str("…\n");
    }
    text.push_str(TAIL);
    text
}
