//! Public JSON projections shared by HTTP responses and the event journal,
//! so snapshots and streamed events carry identical shapes. Internal
//! checkpoints, native session IDs, profiles, and credentials never appear.

use else_wire as wire;
use else_wire::delegations::Delegation;
use else_wire::notices::OwnerNotice;
use else_wire::outgoing::OutgoingDraft;
use else_wire::waits::{ComputerJobWait, McpWait};
use serde_json::{Value, json};

use crate::db::bots::{BotRecord, BotStatus};
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
        "secret_requests": turn.secret_requests.iter().map(else_wire::secrets::SecretRequest::from).collect::<Vec<_>>(),
        "social_setups": turn.social_setups.iter().map(else_wire::socials::SocialSetup::from).collect::<Vec<_>>(),
        "skill_installs": turn.skill_installs.iter().map(else_wire::skills::SkillInstall::from).collect::<Vec<_>>(),
        "questions": turn.questions.iter().map(else_wire::questions::Question::from).collect::<Vec<_>>(),
        "routine": turn.routine,
        "mail": turn.mail,
        "outgoing_drafts": turn.outgoing_drafts.iter().map(OutgoingDraft::from).collect::<Vec<_>>(),
        "delegations": turn.delegations.iter().map(Delegation::from).collect::<Vec<_>>(),
        "mcp_waits": turn.mcp_waits.iter().map(McpWait::from).collect::<Vec<_>>(),
        "computer_job_waits": turn.computer_job_waits.iter().map(ComputerJobWait::from).collect::<Vec<_>>(),
        "computer_handoffs": turn.computer_handoffs,
        "owner_notices": turn.owner_notices.iter().map(OwnerNotice::from).collect::<Vec<_>>(),
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
