//! Runtime task grants: short-lived credentials for one provider task.
//!
//! A grant is an opaque random token whose digest maps to a server-owned
//! record of the bot, conversation, turn, and tool allowlist. Identity comes
//! only from that record: request payloads, native session IDs, or object IDs
//! never select a bot. Grants live in process memory, so an API restart
//! revokes all of them; they also expire, and are revoked when their turn
//! ends or their bot is stopped or deleted. They never authenticate owner
//! routes, and the owner token never authenticates tool routes.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::actor::Actor;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

/// Tools a grant may call. Only implemented operations exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    /// Read this conversation's own history.
    ReadHistory,
    /// Read the bot's memory and memory the owner shared with it.
    ReadMemory,
    /// Hand a task to another registered bot (main bot only).
    Delegate,
    /// Register a new bot for the owner (main bot only).
    CreateBot,
    /// Use the bot's own computer (commands, jobs, files, browser).
    Computer,
    /// Ask the owner for credentials (owner conversations only).
    RequestSecret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub turn_id: Uuid,
    pub tools: Vec<Tool>,
    /// When set, memory may be read only from these bots (a delegated task
    /// does not inherit its bot's unrelated standing permissions).
    pub memory_scope: Option<Vec<Uuid>>,
    expires_at: Instant,
}

impl Grant {
    pub fn allows(&self, tool: Tool) -> bool {
        self.tools.contains(&tool)
    }
}

#[derive(Default)]
pub struct Grants {
    state: Actor<State>,
}

#[derive(Default)]
struct State {
    by_digest: HashMap<[u8; 32], Grant>,
    /// Turns whose current attempt delegated or asked for a secret: every further tool call of
    /// that attempt is refused (it must end). A new attempt's grant clears it.
    parked: std::collections::HashSet<Uuid>,
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Grants {
    /// Issue a grant and return its token. Only the digest is kept.
    pub async fn issue(
        &self,
        bot_id: Uuid,
        conversation_id: Uuid,
        turn_id: Uuid,
        tools: Vec<Tool>,
        ttl: Duration,
    ) -> String {
        self.issue_scoped(bot_id, conversation_id, turn_id, tools, None, ttl)
            .await
    }

    /// Issue a grant whose memory reads are limited to `memory_scope`.
    pub async fn issue_scoped(
        &self,
        bot_id: Uuid,
        conversation_id: Uuid,
        turn_id: Uuid,
        tools: Vec<Tool>,
        memory_scope: Option<Vec<Uuid>>,
        ttl: Duration,
    ) -> String {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("system random source");
        // A distinct prefix keeps grants and owner tokens visibly different.
        let token = format!("tg_{}", URL_SAFE_NO_PAD.encode(bytes));
        let key = digest(&token);
        self.state
            .call(move |state| {
                state.parked.remove(&turn_id);
                state.by_digest.insert(
                    key,
                    Grant {
                        bot_id,
                        conversation_id,
                        turn_id,
                        tools,
                        memory_scope,
                        expires_at: Instant::now() + ttl,
                    },
                );
            })
            .await;
        token
    }

    /// The live grant for a presented token. Expired grants are removed.
    pub async fn resolve(&self, token: &str) -> Option<Grant> {
        let key = digest(token);
        self.state
            .call(move |state| {
                let map = &mut state.by_digest;
                match map.get(&key) {
                    Some(grant) if grant.expires_at > Instant::now() => Some(grant.clone()),
                    Some(_) => {
                        map.remove(&key);
                        None
                    }
                    None => None,
                }
            })
            .await
    }

    /// The attempt of `turn_id` delegated or asked for a secret and must end.
    pub async fn park(&self, turn_id: Uuid) {
        self.state
            .call(move |state| {
                state.parked.insert(turn_id);
            })
            .await;
    }

    pub async fn is_parked(&self, turn_id: Uuid) -> bool {
        self.state
            .call(move |state| state.parked.contains(&turn_id))
            .await
    }

    pub async fn revoke_turn(&self, turn_id: Uuid) {
        self.state
            .call(move |state| {
                state.by_digest.retain(|_, grant| grant.turn_id != turn_id);
                state.parked.remove(&turn_id);
            })
            .await;
    }

    pub async fn revoke_bot(&self, bot_id: Uuid) {
        self.state
            .call(move |state| state.by_digest.retain(|_, grant| grant.bot_id != bot_id))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn issue_resolve_expire_and_revoke() {
        let grants = Grants::default();
        let (bot, conv, turn) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let token = grants
            .issue(bot, conv, turn, vec![Tool::ReadHistory], DEFAULT_TTL)
            .await;
        assert!(token.starts_with("tg_"));
        let grant = grants.resolve(&token).await.unwrap();
        assert_eq!((grant.bot_id, grant.turn_id), (bot, turn));
        assert!(grant.allows(Tool::ReadHistory));
        assert!(grants.resolve("tg_forged").await.is_none());
        grants.revoke_turn(turn).await;
        assert!(grants.resolve(&token).await.is_none());

        let token = grants.issue(bot, conv, turn, vec![], DEFAULT_TTL).await;
        grants.revoke_bot(bot).await;
        assert!(grants.resolve(&token).await.is_none());

        let token = grants.issue(bot, conv, turn, vec![], Duration::ZERO).await;
        assert!(grants.resolve(&token).await.is_none(), "expired");
    }
}
