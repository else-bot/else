use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::db::turns::TurnStatus;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SidebarRow {
    question_kind: Option<String>,
    question_title: Option<String>,
    question_at: Option<DateTime<Utc>>,
    handoff_reason: Option<String>,
    handoff_at: Option<DateTime<Utc>>,
    credential_reason: Option<String>,
    credential_at: Option<DateTime<Utc>>,
    skill_install_title: Option<String>,
    skill_install_at: Option<DateTime<Utc>>,
    latest_turn_status: Option<TurnStatus>,
    /// The owner read the latest turn's last reply in full, such as its
    /// failure notice.
    latest_turn_read: bool,
}

/// `since` is when the open request it shows was made.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SidebarStatus {
    WaitingForAnswer {
        preview: String,
        since: DateTime<Utc>,
    },
    NeedsApproval {
        preview: String,
        since: DateTime<Utc>,
    },
    NeedsTakeover {
        preview: String,
        since: DateTime<Utc>,
    },
    NeedsCredentials {
        preview: String,
        since: DateTime<Utc>,
    },
    Failed,
    Stopped,
    Uncertain,
}

impl SidebarRow {
    pub fn status(&self, busy: bool) -> Option<SidebarStatus> {
        let preview = |text: &str| super::super::reads::preview(text);
        if let (Some(title), Some(since)) = (&self.question_title, self.question_at) {
            let preview = preview(title);
            return Some(if self.question_kind.as_deref() == Some("choice") {
                SidebarStatus::WaitingForAnswer { preview, since }
            } else {
                SidebarStatus::NeedsApproval { preview, since }
            });
        }
        if let (Some(reason), Some(since)) = (&self.handoff_reason, self.handoff_at) {
            return Some(SidebarStatus::NeedsTakeover {
                preview: preview(reason),
                since,
            });
        }
        if let (Some(title), Some(since)) = (&self.skill_install_title, self.skill_install_at) {
            return Some(SidebarStatus::NeedsApproval {
                preview: preview(title),
                since,
            });
        }
        if let (Some(reason), Some(since)) = (&self.credential_reason, self.credential_at) {
            return Some(SidebarStatus::NeedsCredentials {
                preview: preview(reason),
                since,
            });
        }
        if busy || self.latest_turn_read {
            return None;
        }
        match self.latest_turn_status? {
            TurnStatus::Failed => Some(SidebarStatus::Failed),
            TurnStatus::Interrupted => Some(SidebarStatus::Stopped),
            TurnStatus::Uncertain => Some(SidebarStatus::Uncertain),
            TurnStatus::Queued
            | TurnStatus::Preparing
            | TurnStatus::Sending
            | TurnStatus::Running
            | TurnStatus::Cancelling
            | TurnStatus::Waiting
            | TurnStatus::Completed => None,
        }
    }
}
