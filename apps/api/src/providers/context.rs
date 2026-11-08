//! Context a native session keeps until it changes: the bot registry and
//! the saved-permission snapshot. Each block rides in the turn packet only
//! when its content hash differs from the one the session acknowledged, so
//! the system prompt stays byte-identical across turns and the provider's
//! cached prefix survives a new bot, a delegation, or a permission change.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use crate::app::providers::{SessionState, TurnInput};
use crate::providers::prompt;

pub struct Update {
    pub text: Option<String>,
    /// Content hashes the session holds once this turn is accepted.
    pub applied: BTreeMap<String, String>,
}

/// The blocks for this turn, by receipt key. An empty block does not apply
/// to this turn (a routine run has no saved-permission snapshot): it is not
/// sent and the session keeps the receipt of the copy it holds.
fn blocks(input: &TurnInput) -> [(&'static str, String); 2] {
    [
        ("registry", prompt::registry_block(input)),
        ("permissions", prompt::permissions_block(input)),
    ]
}

pub fn for_session(input: &TurnInput, session: Option<&SessionState>) -> Update {
    let mut applied = session.map(|s| s.context.clone()).unwrap_or_default();
    let mut text = String::new();
    for (key, block) in blocks(input) {
        if block.is_empty() {
            continue;
        }
        let hash = hex::encode(&Sha256::digest(block.as_bytes())[..16]);
        if applied.get(key) != Some(&hash) {
            text.push_str(&block);
            applied.insert(key.to_owned(), hash);
        }
    }
    Update {
        text: (!text.is_empty()).then_some(text),
        applied,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::providers::Origin;
    use uuid::Uuid;

    fn input() -> TurnInput {
        let mut input = crate::providers::cli::session::tests::input();
        input.delegation.origin = Origin::OwnerRequest;
        input.delegation.main_bot = true;
        input.delegation.registry = vec![(Uuid::nil(), "Researcher".into())];
        input
    }

    fn session(context: BTreeMap<String, String>) -> SessionState {
        SessionState {
            native_id: "n".into(),
            fingerprint: "f".into(),
            considered_through_turn: 1,
            memory_revision: None,
            default_skills: BTreeMap::new(),
            context,
        }
    }

    #[test]
    fn a_block_is_sent_once_and_again_only_after_it_changes() {
        let mut turn = input();
        let fresh = for_session(&turn, None);
        let text = fresh.text.unwrap();
        assert!(text.contains("Researcher (bot_id 00000000-0000-0000-0000-000000000000)"));
        assert!(text.contains("[else.bot saved permissions]"));

        let held = session(fresh.applied.clone());
        let unchanged = for_session(&turn, Some(&held));
        assert_eq!(unchanged.text, None);
        assert_eq!(unchanged.applied, fresh.applied);

        turn.delegation
            .registry
            .push((Uuid::max(), "Writer".into()));
        let grown = for_session(&turn, Some(&held)).text.unwrap();
        assert!(grown.contains("Writer"));
        assert!(!grown.contains("[else.bot saved permissions]"));
    }

    #[test]
    fn a_block_that_does_not_apply_is_skipped_and_its_receipt_kept() {
        let mut turn = input();
        let held = for_session(&turn, None).applied;
        turn.delegation.origin = Origin::RoutineRun;
        let routine = for_session(&turn, Some(&session(held.clone())));
        assert_eq!(routine.text, None);
        assert_eq!(routine.applied, held);
    }

    #[test]
    fn the_system_prompt_ignores_the_registry_and_the_delegation_budget() {
        let mut turn = input();
        let before = prompt::instructions(&turn, "/p");
        turn.delegation
            .registry
            .push((Uuid::max(), "Writer".into()));
        turn.delegation.remaining = 0;
        assert_eq!(prompt::instructions(&turn, "/p"), before);
        assert!(!before.contains("Researcher"));
    }
}
