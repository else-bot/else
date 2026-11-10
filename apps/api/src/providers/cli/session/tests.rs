use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::app::providers::{HistoryEntry, MemoryContext};

pub(crate) fn input() -> TurnInput {
    TurnInput {
        bot_id: Uuid::nil(),
        conversation_id: Uuid::nil(),
        turn_id: Uuid::new_v4(),
        turn_no: 3,
        bot_name: "Work".into(),
        text: "latest".into(),
        parts: Vec::new(),
        files: Vec::new(),
        profile: None,
        history: [
            (1, "user", "first"),
            (1, "assistant", "one"),
            (2, "user", "second"),
        ]
        .into_iter()
        .map(|(turn_no, kind, text)| HistoryEntry {
            turn_no,
            kind: kind.into(),
            text: text.into(),
            status: "completed".into(),
            error: None,
            parts: Vec::new(),
        })
        .collect(),
        session: None,
        tool_grant: None,
        memory: Some(MemoryContext {
            revision: 5,
            text: "[saved memory] likes tea".into(),
            lines: Vec::new(),
        }),
        attempt: 1,
        delegation: Default::default(),
        secrets: Vec::new(),
        questions: Vec::new(),
        standing_permissions: Vec::new(),
        outgoing_drafts: Vec::new(),
        social_setups: Vec::new(),
        mcp_waits: Vec::new(),
        skill_installs: Vec::new(),
        skills: None,
        mcps: None,
        handoff: None,
        jobs: Vec::new(),
        run: None,
        rules: None,
    }
}

/// A sink that logs its calls in order and can refuse them.
#[derive(Default)]
pub(crate) struct Log {
    pub calls: Mutex<Vec<String>>,
    pub sessions: Mutex<Vec<SessionState>>,
    pub fenced: std::sync::atomic::AtomicBool,
}

impl Log {
    fn push(&self, call: impl Into<String>) -> Result<(), Fenced> {
        self.calls.lock().unwrap().push(call.into());
        if self.fenced.load(std::sync::atomic::Ordering::SeqCst) {
            Err(Fenced)
        } else {
            Ok(())
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl TurnSink for Log {
    async fn packet(&self, _: &Selection, _: &str) -> Result<(), Fenced> {
        self.push("packet")
    }
    async fn retire_session(&self) {
        let _ = self.push("retire");
    }
    async fn sending(&self) -> Result<(), Fenced> {
        self.push("sending")
    }
    async fn running(&self) -> Result<(), Fenced> {
        self.push("running")
    }
    async fn text(&self, chunk: &str) -> Result<(), Fenced> {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        self.push(format!("text:{chunk}"))
    }
    async fn end_message(&self) -> Result<(), Fenced> {
        self.push("end")
    }
    async fn session(&self, state: &SessionState) -> Result<(), Fenced> {
        self.sessions.lock().unwrap().push(state.clone());
        self.push("session")
    }
    async fn reported(&self, settings: &Value, native_turn: &str) -> Result<(), Fenced> {
        self.push(format!("reported:{}:{native_turn}", settings["model"]))
    }
}

#[tokio::test]
async fn a_fresh_session_gets_all_history_and_memory_and_advances_once_on_acceptance() {
    let input = input();
    let sink = Log::default();
    let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n1".into()));
    assert!(!turn.resumed() && !turn.accepted());
    assert!(turn.packet.contains("likes tea") && turn.packet.contains("Owner: first"));
    assert!(turn.packet.contains("[else.bot default skills]"));
    turn.record(&sink).await.unwrap();
    let recorded = sink.sessions.lock().unwrap()[0].clone();
    assert_eq!(
        (recorded.considered_through_turn, recorded.memory_revision),
        (0, None),
        "nothing is considered given before acceptance"
    );
    let reported = json!({ "model": "m" });
    turn.accept(&sink, &reported, "t1").await.unwrap();
    turn.accept(&sink, &reported, "t1").await.unwrap();
    assert_eq!(
        sink.calls(),
        [
            "session",
            "packet",
            "running",
            "session",
            "reported:\"m\":t1"
        ]
    );
    let accepted = sink.sessions.lock().unwrap()[1].clone();
    assert_eq!(accepted.native_id, "n1");
    assert_eq!(accepted.considered_through_turn, 3);
    assert_eq!(accepted.memory_revision, Some(5));
    assert!(!accepted.default_skills.is_empty());
}

#[tokio::test]
async fn compaction_makes_the_next_turn_resend_what_the_session_was_given() {
    let mut input = input();
    input.delegation.origin = crate::app::providers::Origin::OwnerRequest;
    let sink = Log::default();

    // Compacted before acceptance: recorded at once, in case acceptance never
    // comes, and forgotten again when accept() records what was sent.
    let mut early = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n1".into()));
    early.compacted(&sink).await.unwrap();
    let unaccepted = sink.sessions.lock().unwrap().last().cloned().unwrap();
    assert_eq!(unaccepted.considered_through_turn, 0, "no history advance");
    assert!(unaccepted.context.is_empty() && unaccepted.memory_revision.is_none());
    early.accept(&sink, &json!({}), "t").await.unwrap();
    let session = sink.sessions.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        session.considered_through_turn, 3,
        "history stays considered"
    );
    assert_eq!(session.memory_revision, None);
    assert!(session.default_skills.is_empty() && session.context.is_empty());

    // Compacted after acceptance: the session is recorded again at once.
    let mut late = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n2".into()));
    late.accept(&sink, &json!({}), "t").await.unwrap();
    assert!(
        !sink
            .sessions
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .context
            .is_empty()
    );
    late.compacted(&sink).await.unwrap();
    let forgotten = sink.sessions.lock().unwrap().last().cloned().unwrap();
    assert_eq!(forgotten.memory_revision, None);
    assert!(forgotten.context.is_empty());

    input.turn_no = 4;
    let next = PreparedTurn::new(&input, "fp".into(), Opened::Resumed(&forgotten));
    assert!(next.packet.contains("likes tea"));
    assert!(next.packet.contains("[else.bot default skills]"));
    assert!(next.packet.contains("[else.bot saved permissions]"));
}

#[tokio::test]
async fn a_warm_session_gets_only_what_it_lacks() {
    let mut input = input();
    let fresh = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n1".into()));
    let sink = Log::default();
    let mut warm = fresh;
    warm.accept(&sink, &json!({}), "t").await.unwrap();
    let session = sink.sessions.lock().unwrap()[0].clone();
    input.turn_no = 4;
    input.history.push(HistoryEntry {
        turn_no: 3,
        kind: "user".into(),
        text: "third".into(),
        status: "completed".into(),
        error: None,
        parts: Vec::new(),
    });
    let resumed = PreparedTurn::new(&input, "fp".into(), Opened::Resumed(&session));
    assert!(resumed.resumed());
    assert!(
        !resumed.packet.contains("likes tea"),
        "same memory revision"
    );
    assert!(!resumed.packet.contains("default skills"), "same skills");
    assert!(
        !resumed.packet.contains("Owner: first"),
        "considered history"
    );
    input.memory.as_mut().unwrap().revision = 6;
    let changed = PreparedTurn::new(&input, "fp".into(), Opened::Resumed(&session));
    assert!(
        changed.packet.contains("likes tea"),
        "a new memory revision"
    );
}

#[tokio::test]
async fn a_warm_session_gets_only_memory_lines_past_its_receipt() {
    use crate::app::providers::MemoryLine;
    let view = |revision, lines: &[(i64, &str)]| MemoryContext {
        revision,
        text: format!("[full view {revision}]"),
        lines: lines
            .iter()
            .map(|(end, text)| MemoryLine {
                end: *end,
                text: format!("{text}\n"),
            })
            .collect(),
    };
    let mut input = input();
    input.memory = Some(view(5, &[(4, "0+4|likes tea"), (5, "4+1|moved to Lyon")]));
    let sink = Log::default();
    let mut fresh = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n1".into()));
    assert!(fresh.packet.contains("[full view 5]"));
    fresh.accept(&sink, &json!({}), "t").await.unwrap();
    let session = sink.sessions.lock().unwrap().last().cloned().unwrap();
    assert_eq!(session.context.get(prompt::MEMORY_RECEIPT).unwrap(), "5");

    input.turn_no = 4;
    input.memory = Some(view(
        6,
        &[
            (4, "0+4|likes tea"),
            (5, "4+1|moved to Lyon"),
            (6, "5+1|prefers trains"),
        ],
    ));
    let mut warm = PreparedTurn::new(&input, "fp".into(), Opened::Resumed(&session));
    assert!(!warm.packet.contains("[full view"), "{}", warm.packet);
    assert!(warm.packet.contains("5+1|prefers trains"));
    assert!(!warm.packet.contains("likes tea") && !warm.packet.contains("moved to Lyon"));
    warm.accept(&sink, &json!({}), "t").await.unwrap();
    let session = sink.sessions.lock().unwrap().last().cloned().unwrap();
    assert_eq!(session.memory_revision, Some(6));
    assert_eq!(session.context.get(prompt::MEMORY_RECEIPT).unwrap(), "6");

    input.memory = Some(view(
        7,
        &[(6, "0+6|likes tea, lives in Lyon, prefers trains")],
    ));
    let merged = PreparedTurn::new(&input, "fp".into(), Opened::Resumed(&session));
    assert!(
        !merged.packet.contains("[else.bot memory]") && !merged.packet.contains("[full view"),
        "a summary over held entries adds nothing: {}",
        merged.packet
    );

    input.memory = Some(view(8, &[(2, "0+2|likes tea")]));
    let rebuilt = PreparedTurn::new(&input, "fp".into(), Opened::Resumed(&session));
    assert!(
        rebuilt.packet.contains("[full view 8]"),
        "a shorter view is sent whole"
    );
}

#[tokio::test]
async fn a_fenced_acceptance_does_not_advance_the_session() {
    let input = input();
    let sink = Log::default();
    sink.fenced.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut turn = PreparedTurn::new(&input, "fp".into(), Opened::Fresh("n1".into()));
    assert!(turn.accept(&sink, &json!({}), "t").await.is_err());
    assert_eq!(sink.calls(), ["running"]);
    assert!(sink.sessions.lock().unwrap().is_empty());
}

#[test]
fn fingerprints_cover_identity_policy_profile_and_bot() {
    let input = input();
    let profile = Profile {
        model: "m".into(),
        reasoning_effort: "high".into(),
        service_tier: "default".into(),
    };
    let identity = |policy: &'static [&'static str]| Identity {
        prompt_version: "v1",
        version: "1.0",
        runtime: "runtime:host:/bots/x",
        tools: "tools:host",
        policy,
    };
    let base = fingerprint(&input, &profile, &identity(&["agents:disabled", "acct"]));
    assert_eq!(base.len(), 32);
    assert_eq!(
        base,
        fingerprint(&input, &profile, &identity(&["agents:disabled", "acct"]))
    );
    assert_ne!(
        base,
        fingerprint(&input, &profile, &identity(&["agents:disabled", "other"]))
    );
    let mut renamed = input.clone();
    renamed.bot_name = "Other".into();
    assert_ne!(
        base,
        fingerprint(&renamed, &profile, &identity(&["agents:disabled", "acct"]))
    );
    let fast = Profile {
        model: "fast".into(),
        ..profile.clone()
    };
    assert_ne!(
        base,
        fingerprint(&input, &fast, &identity(&["agents:disabled", "acct"]))
    );
}
