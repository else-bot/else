//! Public JSON projections shared by HTTP responses and the event journal,
//! so snapshots and streamed events carry identical shapes. Internal
//! checkpoints, native session IDs, profiles, and credentials never appear.

use else_wire as wire;
use serde_json::{Value, json};

use crate::app::routines::ModelSetting;
use crate::db::bots::{BotRecord, BotStatus};
use crate::db::routines::{Routine, RunState};
use crate::db::turns::{ItemRecord, TurnRecord, TurnStatus};

impl From<&BotRecord> for wire::bots::Bot {
    /// An active bot, or a deletion receipt with only its identity and
    /// cleanup.
    fn from(bot: &BotRecord) -> Self {
        let receipt = Self {
            id: bot.id,
            name: bot.name.clone(),
            avatar: bot.avatar().into(),
            conversation_id: None,
            created_at: None,
            last_message_at: None,
            metadata_revision: None,
            deleted_at: bot.deleted_at,
            selected_provider_id: None,
            category_id: None,
            muted: false,
            activity: None,
            sidebar_status: None,
            read_state: None,
            watching: 0,
            cleanup: None,
        };
        if bot.status != BotStatus::Active {
            return Self {
                cleanup: Some(wire::bots::Cleanup {
                    state: if bot.status == BotStatus::Deleted {
                        wire::bots::CleanupState::Done
                    } else {
                        wire::bots::CleanupState::Pending
                    },
                    error: None,
                }),
                ..receipt
            };
        }
        Self {
            conversation_id: Some(bot.conversation_id),
            created_at: Some(bot.created_at),
            last_message_at: bot.last_message_at,
            metadata_revision: Some(bot.metadata_revision),
            deleted_at: None,
            selected_provider_id: bot.selected_provider_id.clone(),
            category_id: bot.category_id,
            muted: bot.muted,
            activity: Some(bot.into()),
            sidebar_status: bot
                .sidebar
                .status(bot.active_turn_id.is_some() || bot.queued_count > 0),
            read_state: Some(bot.read.state()),
            watching: usize::try_from(bot.watching).unwrap_or(0),
            ..receipt
        }
    }
}

impl From<&BotRecord> for wire::bots::BotEnvelope {
    fn from(bot: &BotRecord) -> Self {
        Self { bot: bot.into() }
    }
}

impl From<&crate::db::bots::Listing> for wire::bots::BotList {
    fn from(listing: &crate::db::bots::Listing) -> Self {
        Self {
            default_bot_id: listing.default_bot_id,
            categories: listing.categories.iter().cloned().map(Into::into).collect(),
            bots: listing.bots.iter().map(Into::into).collect(),
        }
    }
}

impl From<&BotRecord> for wire::bots::Activity {
    /// Projected from turn records. No separate state.
    fn from(bot: &BotRecord) -> Self {
        let phase = match (bot.cancel_pending, bot.active_turn_status) {
            (true, _) | (_, Some(TurnStatus::Cancelling)) => wire::bots::Phase::Cancelling,
            (_, Some(_)) => wire::bots::Phase::Working,
            (_, None) if bot.queued_count > 0 => wire::bots::Phase::Queued,
            _ => wire::bots::Phase::Idle,
        };
        Self {
            phase,
            active_turn_id: bot.active_turn_id,
            queued_count: u32::try_from(bot.queued_count).unwrap_or(u32::MAX),
            waiting_on: bot.waiting_on.as_ref().map(|waiting| waiting.0.clone()),
            running_job_count: 0,
            error: None,
        }
    }
}

pub fn turn_json(turn: &TurnRecord) -> Value {
    let error = turn
        .error_code
        .as_ref()
        .map(|code| json!({ "code": code, "message": turn.error_message }));
    json!({
        "id": turn.id,
        "bot_id": turn.bot_id,
        "request_id": turn.request_id,
        "turn_no": turn.turn_no,
        "attempt": turn.attempt,
        "limit_continued": (turn.limit_continuations > 0).then(|| json!({
            "count": turn.limit_continuations,
            "max": crate::db::limit_cuts::PER_REQUEST,
            "minutes": crate::providers::cli::attempt::TURN_DEADLINE.as_secs() / 60,
        })),
        "provider_id": turn.provider_id,
        "profile_revision": turn.profile_revision,
        "status": turn.status,
        "error": error,
        "effect_uncertain": turn.effect_uncertain,
        "created_at": turn.created_at,
        "started_at": turn.started_at,
        "finished_at": turn.finished_at,
        "reaction": turn.reaction.as_ref().map(|reaction| reaction.0),
        "steered_into": turn.steered_into,
        "steer_status": turn.steer_status,
        "secret_requests": turn.secret_requests,
        "social_setups": turn.social_setups,
        "skill_installs": turn.skill_installs,
        "questions": turn.questions,
        "routine": turn.routine,
        "mail": turn.mail,
        "outgoing_drafts": turn.outgoing_drafts,
        "delegations": turn.delegations,
        "mcp_waits": turn.mcp_waits,
        "computer_job_waits": turn.computer_job_waits,
        "computer_handoffs": turn.computer_handoffs,
        "owner_notices": turn.owner_notices,
    })
}

pub fn item_json(item: &ItemRecord) -> Value {
    json!({
        "id": item.id,
        "seq": item.seq,
        "turn_id": item.turn_id,
        "request_id": item.request_id,
        "kind": item.kind,
        "text": item.text,
        "provider_id": item.provider_id,
        "created_at": item.created_at,
        "reaction": item.reaction.as_ref().map(|reaction| reaction.0),
        "bot_task": item.bot_task,
        "bot_reply": item.bot_reply,
        "routine_update": item.routine_update,
        "bot_created": item.bot_created,
        "rules_update": item.rules_update,
        "parts": item.parts.as_ref().map_or(&[][..], |parts| &parts.0[..]),
        "attempt": item.attempt,
    })
}

/// A watcher with its schedule and what its last check saw.
pub fn watcher_json(watcher: &crate::db::watchers::Watcher) -> Value {
    json!({
        "id": watcher.id,
        "purpose": watcher.purpose,
        "check": watcher.check_command,
        "secrets": watcher.secrets.0,
        "every_minutes": watcher.every_minutes,
        "until": watcher.watch_until,
        "status": watcher.status,
        "outcome": watcher.outcome,
        "last_status": watcher.last_status,
        "last_checked_at": watcher.last_checked_at,
        "next_check_at": watcher.next_check_at,
        "last_output": watcher.last_output,
        "checks": watcher.checks,
        "failures": watcher.failures,
        "created_at": watcher.created_at,
        "created_turn_id": watcher.created_turn_id,
    })
}

/// A webhook with its pace, waiting wake and last wake. Never its token.
pub fn webhook_json(webhook: &crate::db::webhooks::Webhook) -> Value {
    json!({
        "id": webhook.id,
        "bot_id": webhook.bot_id,
        "name": webhook.name,
        "prompt": webhook.prompt,
        "min_interval_minutes": webhook.pace.min_interval_minutes(),
        "daily_limit": webhook.pace.daily_limit(),
        "enabled": webhook.enabled,
        "requested_at": webhook.requested_at,
        "wake_not_before": webhook.not_before,
        "created_at": webhook.created_at,
        "updated_at": webhook.updated_at,
        "last_wake": webhook.last_wake.as_ref().map(|wake| json!({
            "woke_at": wake.woke_at,
            "status": wake.status,
            "turn_id": wake.turn_id,
        })),
    })
}

/// A routine with its derived last-run state. The prompt is included: the
/// owner may read what each run will do.
pub fn routine_json(routine: &Routine) -> Value {
    let last_run = routine.last_run.as_ref().map(|run| {
        json!({
            "due_at": run.due_at,
            "status": run.state.label(),
            "skip_reason": match run.state {
                RunState::Skipped(skip) => Some(skip),
                _ => None,
            },
            "turn_id": run.turn_id,
            "error": run.error_code.as_ref().map(|code| json!({ "code": code, "message": run.error_message })),
            "finished_at": run.finished_at,
        })
    });
    json!({
        "id": routine.id,
        "bot_id": routine.bot_id,
        "name": routine.name,
        "prompt": routine.prompt,
        "prompt_revision": routine.prompt_revision,
        "check": routine.check,
        "cron": routine.cron,
        "run_at": routine.run_at,
        "timezone": routine.timezone,
        "active_hours": routine.active_hours.map(|hours| json!({
            "days": hours.days(),
            "start": hours.start(),
            "end": hours.end(),
        })),
        "enabled": routine.enabled,
        "model": match &routine.model {
            ModelSetting::Own(model) => json!(model),
            ModelSetting::Inherit { .. } => Value::Null,
        },
        "requested_model": match &routine.model {
            ModelSetting::Inherit { requested } => json!(requested),
            ModelSetting::Own(_) => Value::Null,
        },
        "next_run_at": routine.next_run_at,
        "created_at": routine.created_at,
        "updated_at": routine.updated_at,
        "last_run": last_run,
        "failed_runs": routine.failed_runs,
        "paused_reason": routine.paused_reason,
        "retrying": routine.retrying,
        "quiet": routine.quiet,
    })
}
