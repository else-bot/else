//! Public JSON projections shared by HTTP responses and the event journal,
//! so snapshots and streamed events carry identical shapes. Internal
//! checkpoints, native session IDs, profiles, and credentials never appear.

use else_wire as wire;

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
            waiting_on: bot.waiting_on.as_deref().map(Into::into),
            running_job_count: 0,
            error: None,
        }
    }
}

impl From<&TurnRecord> for wire::turns::Turn {
    fn from(turn: &TurnRecord) -> Self {
        Self {
            id: turn.id,
            bot_id: Some(turn.bot_id),
            request_id: turn.request_id,
            turn_no: turn.turn_no,
            attempt: Some(i64::from(turn.attempt)),
            limit_continued: (turn.limit_continuations > 0).then(|| wire::turns::LimitContinued {
                count: turn.limit_continuations,
                max: crate::db::limit_cuts::PER_REQUEST,
                minutes: (crate::providers::cli::attempt::TURN_DEADLINE.as_secs() / 60) as i64,
            }),
            provider_id: turn.provider_id.clone(),
            profile_revision: turn.profile_revision,
            status: turn.status.into(),
            error: turn.error_code.as_ref().map(|code| wire::turns::TurnError {
                code: code.clone(),
                message: turn.error_message.clone(),
            }),
            effect_uncertain: turn.effect_uncertain,
            created_at: Some(turn.created_at),
            started_at: turn.started_at,
            finished_at: turn.finished_at,
            reaction: turn.reaction.as_ref().map(|reaction| reaction.0.into()),
            steered_into: turn.steered_into,
            steer_status: turn.steer_status.map(Into::into),
            secret_requests: turn.secret_requests.iter().map(Into::into).collect(),
            social_setups: turn.social_setups.iter().map(Into::into).collect(),
            skill_installs: turn.skill_installs.iter().map(Into::into).collect(),
            questions: turn.questions.iter().map(Into::into).collect(),
            routine: turn.routine.as_deref().map(Into::into),
            mail: turn.mail.iter().map(Into::into).collect(),
            outgoing_drafts: turn.outgoing_drafts.iter().map(Into::into).collect(),
            delegations: turn.delegations.iter().map(Into::into).collect(),
            mcp_waits: turn.mcp_waits.iter().map(Into::into).collect(),
            computer_job_waits: turn.computer_job_waits.iter().map(Into::into).collect(),
            computer_handoffs: turn.computer_handoffs.iter().map(Into::into).collect(),
            owner_notices: turn.owner_notices.iter().map(Into::into).collect(),
            retryable: turn.retryable(),
        }
    }
}

impl From<&ItemRecord> for wire::items::Item {
    fn from(item: &ItemRecord) -> Self {
        Self {
            id: item.id,
            seq: item.seq,
            turn_id: item.turn_id,
            request_id: item.request_id,
            kind: item.kind.into(),
            text: item.text.clone(),
            provider_id: item.provider_id.clone(),
            created_at: Some(item.created_at),
            reaction: item.reaction.as_ref().map(|reaction| reaction.0.into()),
            bot_task: item.bot_task.as_ref().map(|task| task.0.clone()),
            bot_reply: item.bot_reply.as_ref().map(|reply| reply.0.clone()),
            routine_update: item.routine_update.as_deref().map(Into::into),
            bot_created: item.bot_created.as_deref().map(Into::into),
            rules_update: item.rules_update.as_deref().map(Into::into),
            parts: item
                .parts
                .iter()
                .flat_map(|parts| &parts.0)
                .map(Into::into)
                .collect(),
            attempt: item.attempt,
        }
    }
}
