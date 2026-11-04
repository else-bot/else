//! Provider switching (POC-020). When an owner turn is bound to a different
//! provider than the turn before it, and the receiving native session has
//! not seen all settled history, the API freezes that history at the
//! previous owner turn, asks the outgoing provider for a bounded checkpoint
//! with tools disabled and verified (`Provider::checkpoint`), and records a
//! handoff before anything is sent. When the outgoing provider is missing,
//! unavailable, cannot prove a no-tool run, fails, times out, or returns too
//! much, the handoff uses the deterministic fallback: the recorded history
//! alone. Either way the receiving adapter gets the checkpoint (if any) with
//! the history its session missed and the owner's message exactly once;
//! the follow-up itself is never given to the outgoing provider.

use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use crate::app::providers::{Availability, CheckpointRequest, HandoffContext, TurnInput};
use crate::db::handoffs::{self, NewHandoff};
use crate::providers::{CHECKPOINT_BYTES, Registry};

pub async fn prepare(
    pool: &SqlitePool,
    registry: &Registry,
    input: &TurnInput,
    target: &str,
    cancel: &CancellationToken,
) -> Result<Option<HandoffContext>, sqlx::Error> {
    if input.history.is_empty() {
        return Ok(None);
    }
    // Already prepared for this attempt (for example before a restart).
    if let Some(existing) = handoffs::existing(pool, input.turn_id, input.attempt).await? {
        return Ok(Some(existing));
    }
    // A continuation of a turn whose handoff was accepted needs none.
    if input.attempt > 1 && handoffs::accepted_for_turn(pool, input.turn_id).await? {
        return Ok(None);
    }
    let Some(source) =
        handoffs::previous_provider(pool, input.conversation_id, input.turn_no).await?
    else {
        return Ok(None);
    };
    if source == target {
        return Ok(None);
    }
    let boundary = input.history.last().map_or(0, |e| e.turn_no);
    // Nothing missed: the receiving session already considered it all.
    if input
        .session
        .as_ref()
        .is_some_and(|s| s.considered_through_turn >= boundary)
    {
        return Ok(None);
    }
    let (checkpoint, reason) = match registry.get(&source) {
        None => (
            None,
            Some(format!("the {source} provider is not configured")),
        ),
        Some(provider) => match provider.availability() {
            Availability::Unavailable(why) => {
                (None, Some(format!("{source} is unavailable: {why}")))
            }
            Availability::Available => {
                let profile = if provider.uses_profile() {
                    crate::db::profiles::get(pool, &source)
                        .await?
                        .map(|record| record.profile())
                } else {
                    None
                };
                let request = CheckpointRequest {
                    bot_id: input.bot_id,
                    profile,
                    prompt: crate::providers::prompt::checkpoint_prompt(input, CHECKPOINT_BYTES),
                    max_bytes: CHECKPOINT_BYTES,
                };
                match provider.checkpoint(request, cancel).await {
                    Ok(text) => (Some(text), None),
                    Err(why) => (None, Some(why)),
                }
            }
        },
    };
    if let Some(reason) = &reason {
        tracing::info!(turn = %input.turn_id, %source, %target, %reason, "handoff uses the recorded history");
    }
    handoffs::insert(
        pool,
        NewHandoff {
            bot_id: input.bot_id,
            conversation_id: input.conversation_id,
            turn_id: input.turn_id,
            attempt: input.attempt,
            source_provider: &source,
            target_provider: target,
            boundary_turn_no: boundary,
            checkpoint,
            reason,
        },
    )
    .await
}
