//! A turn's native session: its identity, the packet it needs, and the
//! bookkeeping that advances once the provider accepts the turn.

use std::collections::BTreeMap;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::app::providers::{Fenced, Profile, Selection, SessionState, TurnInput, TurnSink};
use crate::providers::{context, default_skills, prompt};

/// The native session a turn uses.
pub enum Opened<'s> {
    /// A new session under this native ID.
    Fresh(String),
    /// The bot's recorded session, which the provider still holds.
    Resumed(&'s SessionState),
}

/// What a session has been given, applied once the turn is accepted.
struct Advance {
    through: i64,
    memory_revision: Option<i64>,
    default_skills: BTreeMap<String, String>,
    context: BTreeMap<String, String>,
}

/// A turn ready to send: the session to record, and the packet with only
/// what that session lacks (history since it last considered the
/// conversation, changed memory, changed default skills).
pub struct PreparedTurn<'a> {
    session: SessionState,
    resumed: bool,
    pub segments: Vec<prompt::Segment<'a>>,
    pub packet: String,
    selection: Selection,
    advance: Option<Advance>,
    /// The provider compacted the session during this turn.
    compacted: bool,
}

impl<'a> PreparedTurn<'a> {
    pub fn new(input: &'a TurnInput, fingerprint: String, opened: Opened<'_>) -> Self {
        let resumed = match opened {
            Opened::Resumed(session) => Some(session),
            Opened::Fresh(_) => None,
        };
        let session = match opened {
            Opened::Resumed(session) => SessionState {
                fingerprint,
                ..session.clone()
            },
            Opened::Fresh(native_id) => SessionState {
                native_id,
                fingerprint,
                considered_through_turn: 0,
                memory_revision: None,
                default_skills: BTreeMap::new(),
                context: BTreeMap::new(),
            },
        };
        let memory = prompt::memory_for(input.memory.as_ref(), resumed);
        let skills = default_skills::for_session(resumed);
        let mut context = context::for_session(input, resumed);
        let (segments, selection) = prompt::turn_content(
            input,
            session.considered_through_turn,
            memory.as_ref(),
            &skills,
            &context,
        );
        let packet = prompt::segments_text(&segments);
        let given = memory
            .is_some()
            .then(|| input.memory.as_ref()?.lines.last())
            .flatten();
        if let Some(line) = given {
            context
                .applied
                .insert(prompt::MEMORY_RECEIPT.to_owned(), line.end.to_string());
        }
        let advance = Advance {
            through: input.turn_no,
            memory_revision: memory.map(|m| m.revision).or(session.memory_revision),
            default_skills: skills.applied,
            context: context.applied,
        };
        Self {
            session,
            resumed: resumed.is_some(),
            segments,
            packet,
            selection,
            advance: Some(advance),
            compacted: false,
        }
    }

    pub fn native_id(&self) -> &str {
        &self.session.native_id
    }

    pub fn resumed(&self) -> bool {
        self.resumed
    }

    pub fn accepted(&self) -> bool {
        self.advance.is_none()
    }

    /// Record the session and the packet about to be sent.
    pub async fn record(&self, sink: &dyn TurnSink) -> Result<(), Fenced> {
        sink.session(&self.session).await?;
        sink.packet(&self.selection, &self.packet).await
    }

    /// The provider acknowledged the turn: mark it running and record that
    /// the session now holds this turn's history, memory, and skills, with
    /// the settings the provider reported. Only the first call has effect.
    pub async fn accept(
        &mut self,
        sink: &dyn TurnSink,
        reported: &Value,
        native_turn: &str,
    ) -> Result<(), Fenced> {
        let Some(advance) = self.advance.take() else {
            return Ok(());
        };
        sink.running().await?;
        self.session.considered_through_turn = advance.through;
        self.session.memory_revision = advance.memory_revision;
        self.session.default_skills = advance.default_skills;
        self.session.context = advance.context;
        if self.compacted {
            self.forget_given();
        }
        sink.session(&self.session).await?;
        sink.reported(reported, native_turn).await
    }
}

impl PreparedTurn<'_> {
    /// The provider compacted the session: its summary may have dropped the
    /// memory, default skills, and context blocks, so the next turn sends
    /// them again. History stays considered; compaction summarized it.
    /// Recorded at once, so a turn the provider never accepts cannot leave a
    /// resumable session with stale receipts; accept() forgets them again
    /// after recording what this turn sent.
    pub async fn compacted(&mut self, sink: &dyn TurnSink) -> Result<(), Fenced> {
        self.compacted = true;
        self.forget_given();
        sink.session(&self.session).await
    }

    fn forget_given(&mut self) {
        self.session.memory_revision = None;
        self.session.default_skills.clear();
        self.session.context.clear();
    }
}

/// A provider's non-secret session identity beyond the profile and the
/// bot: anything whose change makes a native session unusable.
pub struct Identity<'a> {
    pub prompt_version: &'a str,
    pub version: &'a str,
    pub runtime: &'a str,
    pub tools: &'a str,
    /// Provider policies and the host account, in a fixed order.
    pub policy: &'a [&'a str],
}

/// The session compatibility fingerprint: a native session is resumed only
/// under the same else.bot tools, delegation mode, identity, and profile.
pub fn fingerprint(input: &TurnInput, profile: &Profile, identity: &Identity<'_>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(crate::app::tools::TOOL_NAMES.join("\0").as_bytes());
    // The registry's mode line only: the bot list rides in the packet.
    let registry = prompt::registry_text(input);
    let mode = registry.lines().next().unwrap_or_default();
    let head = [
        mode,
        identity.prompt_version,
        identity.version,
        identity.runtime,
        identity.tools,
    ];
    let tail = [
        profile.model.as_str(),
        &profile.reasoning_effort,
        &profile.service_tier,
        &input.bot_name,
    ];
    for part in head.iter().chain(identity.policy).chain(&tail) {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    hex::encode(&hasher.finalize()[..16])
}

#[cfg(test)]
pub(crate) mod tests;
