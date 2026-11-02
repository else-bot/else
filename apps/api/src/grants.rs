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
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub bot_id: Uuid,
    pub conversation_id: Uuid,
    pub turn_id: Uuid,
    pub tools: Vec<Tool>,
    expires_at: Instant,
}

impl Grant {
    pub fn allows(&self, tool: Tool) -> bool {
        self.tools.contains(&tool)
    }
}

#[derive(Default)]
pub struct Grants {
    by_digest: Mutex<HashMap<[u8; 32], Grant>>,
    /// Turns whose current attempt delegated: every further tool call of
    /// that attempt is refused (it must end). A new attempt's grant clears it.
    parked: Mutex<std::collections::HashSet<Uuid>>,
}

fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Grants {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<[u8; 32], Grant>> {
        self.by_digest.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Issue a grant and return its token. Only the digest is kept.
    pub fn issue(
        &self,
        bot_id: Uuid,
        conversation_id: Uuid,
        turn_id: Uuid,
        tools: Vec<Tool>,
        ttl: Duration,
    ) -> String {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("system random source");
        // A distinct prefix keeps grants and owner tokens visibly different.
        let token = format!("tg_{}", URL_SAFE_NO_PAD.encode(bytes));
        self.parked().remove(&turn_id);
        self.map().insert(
            digest(&token),
            Grant {
                bot_id,
                conversation_id,
                turn_id,
                tools,
                expires_at: Instant::now() + ttl,
            },
        );
        token
    }

    /// The live grant for a presented token. Expired grants are removed.
    pub fn resolve(&self, token: &str) -> Option<Grant> {
        let key = digest(token);
        let mut map = self.map();
        match map.get(&key) {
            Some(grant) if grant.expires_at > Instant::now() => Some(grant.clone()),
            Some(_) => {
                map.remove(&key);
                None
            }
            None => None,
        }
    }

    fn parked(&self) -> std::sync::MutexGuard<'_, std::collections::HashSet<Uuid>> {
        self.parked.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The attempt of `turn_id` delegated and must end.
    pub fn park(&self, turn_id: Uuid) {
        self.parked().insert(turn_id);
    }

    pub fn is_parked(&self, turn_id: Uuid) -> bool {
        self.parked().contains(&turn_id)
    }

    pub fn revoke_turn(&self, turn_id: Uuid) {
        self.map().retain(|_, grant| grant.turn_id != turn_id);
    }

    pub fn revoke_bot(&self, bot_id: Uuid) {
        self.map().retain(|_, grant| grant.bot_id != bot_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_resolve_expire_and_revoke() {
        let grants = Grants::default();
        let (bot, conv, turn) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let token = grants.issue(bot, conv, turn, vec![Tool::ReadHistory], DEFAULT_TTL);
        assert!(token.starts_with("tg_"));
        let grant = grants.resolve(&token).unwrap();
        assert_eq!((grant.bot_id, grant.turn_id), (bot, turn));
        assert!(grant.allows(Tool::ReadHistory));
        assert!(grants.resolve("tg_forged").is_none());
        grants.revoke_turn(turn);
        assert!(grants.resolve(&token).is_none());

        let token = grants.issue(bot, conv, turn, vec![], DEFAULT_TTL);
        grants.revoke_bot(bot);
        assert!(grants.resolve(&token).is_none());

        let token = grants.issue(bot, conv, turn, vec![], Duration::ZERO);
        assert!(grants.resolve(&token).is_none(), "expired");
    }
}
