//! Where a view stands with the server's newest page, and the only code
//! that moves it between those states.

use super::ConversationView;
use crate::model::{Applied, Conversation, Frame, Reset};
use else_wire::bots::Activity;
use else_wire::conversation::Sent;
use gpui_kit::*;

pub(super) enum Load {
    /// Its conversation, if any, is the server's and follows the stream.
    Settled,
    Running {
        _task: Task<()>,
    },
    /// The last load or stream event failed; a shown view retries after
    /// a pause, a warm one when shown again.
    Failed {
        _retry: Option<Task<()>>,
    },
}

/// What a view needs to catch up once shown.
#[derive(Debug, PartialEq)]
pub(in crate::app) enum Resume {
    /// Follow the live stream from this cursor.
    Follow(String),
    /// Its load is still running and subscribes when it lands.
    Wait,
    /// Load its newest page.
    Load,
}

impl ConversationView {
    pub(in crate::app) fn resume(&self) -> Resume {
        resume(&self.load, self.conversation.as_ref())
    }

    pub(in crate::app) fn stream_problem(&self) -> Option<&str> {
        self.stream_problem.as_deref()
    }

    /// Starts loading the newest page; `task` lands it.
    pub(in crate::app) fn begin_load(&mut self, task: Task<()>) {
        self.load = Load::Running { _task: task };
    }

    /// Shows a page saved on disk while the server's loads, unless the view
    /// already has a conversation. Returns whether it showed it.
    pub(in crate::app) fn show_saved(
        &mut self,
        conversation: Conversation,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.conversation.is_some() {
            return false;
        }
        self.conversation = Some(conversation);
        cx.notify();
        true
    }

    /// Takes the server's newest page, which then follows the stream.
    pub(in crate::app) fn settle(&mut self, conversation: Conversation, cx: &mut Context<Self>) {
        self.conversation = Some(conversation);
        self.load = Load::Settled;
        self.loading_gap = None;
        self.stream_problem = None;
        cx.notify();
    }

    /// The load failed with `problem`; `retry` loads again after a pause.
    pub(in crate::app) fn fail(
        &mut self,
        problem: String,
        retry: Option<Task<()>>,
        cx: &mut Context<Self>,
    ) {
        self.stream_problem = Some(problem);
        self.load = Load::Failed { _retry: retry };
        cx.notify();
    }

    /// The server no longer serves the page asked for; the next resume
    /// loads it again.
    pub(in crate::app) fn expire(&mut self) {
        self.load = Load::Failed { _retry: None };
    }

    /// A warm view's stream broke; the next resume loads its page again
    /// unless a load is already running.
    pub(in crate::app) fn break_stream(&mut self) {
        if matches!(self.load, Load::Settled) {
            self.load = Load::Failed { _retry: None };
        }
    }

    /// Shows why the stream is down, or clears it once it is back.
    pub(in crate::app) fn set_stream_problem(
        &mut self,
        problem: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.stream_problem = problem;
        cx.notify();
    }

    /// Applies a stream frame, and returns the bot's activity after it.
    /// Returns `None` while nothing is loaded to apply it to.
    pub(in crate::app) fn apply_frame(
        &mut self,
        frame: &Frame,
        cx: &mut Context<Self>,
    ) -> Option<Result<(Applied, Option<Activity>), Reset>> {
        let conversation = self.conversation.as_mut()?;
        let applied = conversation.apply(frame);
        if matches!(applied, Ok(Applied::Changed)) {
            cx.notify();
        }
        Some(applied.map(|applied| (applied, conversation.activity.clone())))
    }

    /// Adds a send the server accepted. Returns whether a conversation took
    /// it.
    pub(in crate::app) fn insert_sent(&mut self, sent: Sent, cx: &mut Context<Self>) -> bool {
        let Some(conversation) = self.conversation.as_mut() else {
            return false;
        };
        conversation.insert_item(sent.item);
        conversation.insert_turn(sent.turn);
        cx.notify();
        true
    }
}

fn resume(load: &Load, conversation: Option<&Conversation>) -> Resume {
    match (load, conversation) {
        (Load::Running { .. }, _) => Resume::Wait,
        (Load::Settled, Some(conversation)) if !conversation.is_saved() => {
            Resume::Follow(conversation.cursor())
        }
        _ => Resume::Load,
    }
}

#[cfg(test)]
mod tests {
    use super::{Load, Resume, resume};
    use crate::model::Conversation;
    use gpui_kit::Task;

    fn page(applied: i64) -> else_wire::conversation::Snapshot {
        serde_json::from_value(serde_json::json!({
            "event_cursor": format!("server:bot:{applied}"),
            "items": [], "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        }))
        .unwrap()
    }

    #[test]
    fn a_view_shown_again_follows_from_its_cursor_unless_it_needs_a_load() {
        assert_eq!(
            resume(&Load::Settled, None),
            Resume::Load,
            "nothing loaded yet"
        );
        let running = Load::Running {
            _task: Task::ready(()),
        };
        assert_eq!(
            resume(&running, None),
            Resume::Wait,
            "its load subscribes on landing"
        );
        let live = Conversation::from_snapshot(page(42)).unwrap();
        assert_eq!(
            resume(&Load::Settled, Some(&live)),
            Resume::Follow("server:bot:42".into())
        );
        assert_eq!(
            resume(&Load::Failed { _retry: None }, Some(&live)),
            Resume::Load,
            "a failed load or a reset reloads"
        );
        let saved = Conversation::from_saved(page(42)).unwrap();
        assert_eq!(
            resume(&Load::Settled, Some(&saved)),
            Resume::Load,
            "a saved page is replaced"
        );
    }
}
