//! Public JSON projections shared by HTTP responses and the event journal,
//! so snapshots and streamed events carry identical shapes. Internal
//! checkpoints, native session IDs, profiles, and credentials never appear.

use serde_json::{Value, json};

use crate::db::bots::{BotRecord, BotStatus};
use crate::db::routines::Routine;
use crate::db::turns::{ItemRecord, TurnRecord};

/// Public projection of an active bot or a deletion receipt.
pub fn bot_json(bot: &BotRecord) -> Value {
    if bot.status != BotStatus::Active {
        return json!({
            "id": bot.id,
            "name": bot.name,
            "avatar": bot.avatar(),
            "deleted_at": bot.deleted_at,
            "cleanup": {
                "state": if bot.status == BotStatus::Deleted { "done" } else { "pending" },
                "error": null,
            },
        });
    }
    json!({
        "id": bot.id,
        "name": bot.name,
        "avatar": bot.avatar(),
        "conversation_id": bot.conversation_id,
        "created_at": bot.created_at,
        "last_message_at": bot.last_message_at,
        "deleted_at": null,
        "selected_provider_id": bot.selected_provider_id,
        "category_id": bot.category_id,
        "activity": activity_json(bot),
        "cleanup": null,
    })
}

/// Project turn records into the sidebar activity. No separate state.
pub fn activity_json(bot: &BotRecord) -> Value {
    let phase = match (bot.cancel_pending, bot.active_turn_status.as_deref()) {
        (true, _) | (_, Some("cancelling")) => "cancelling",
        (_, Some(_)) => "working",
        (_, None) if bot.queued_count > 0 => "queued",
        _ => "idle",
    };
    json!({
        "phase": phase,
        "active_turn_id": bot.active_turn_id,
        "queued_count": bot.queued_count,
        "running_job_count": 0,
        "error": null,
    })
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
        "provider_id": turn.provider_id,
        "profile_revision": turn.profile_revision,
        "status": turn.status,
        "error": error,
        "effect_uncertain": turn.effect_uncertain,
        "created_at": turn.created_at,
        "finished_at": turn.finished_at,
        "secret_requests": turn.secret_requests,
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
    })
}

/// A routine with its derived last-run state. The prompt is included: the
/// owner may read what each run will do.
pub fn routine_json(routine: &Routine) -> Value {
    let last_run = routine.last_run.as_ref().map(|run| {
        json!({
            "due_at": run.due_at,
            "status": run.status,
            "skip_reason": run.skip_reason,
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
        "cron": routine.cron,
        "timezone": routine.timezone,
        "enabled": routine.enabled,
        "next_run_at": routine.next_run_at,
        "created_at": routine.created_at,
        "updated_at": routine.updated_at,
        "last_run": last_run,
    })
}
