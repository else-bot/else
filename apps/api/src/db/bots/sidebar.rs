use chrono::{DateTime, Utc};
use sqlx::types::Json;

use crate::app::outgoing::Content;

use crate::db::turns::TurnStatus;
use crate::socials::types::Provider;
use else_wire::bots::SidebarStatus;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SidebarRow {
    question_kind: Option<crate::app::questions::Kind>,
    question_title: Option<String>,
    question_at: Option<DateTime<Utc>>,
    handoff_reason: Option<String>,
    handoff_at: Option<DateTime<Utc>>,
    credential_reason: Option<String>,
    credential_at: Option<DateTime<Utc>>,
    setup_provider: Option<Provider>,
    setup_at: Option<DateTime<Utc>>,
    /// A turn still open waits for the owner to hand back the computer.
    /// A taken handoff outlives its turn, so ended turns do not count.
    computer_taken_at: Option<DateTime<Utc>>,
    skill_install_title: Option<String>,
    skill_install_at: Option<DateTime<Utc>>,
    draft_content: Option<Json<Content>>,
    draft_at: Option<DateTime<Utc>>,
    notice_id: Option<uuid::Uuid>,
    notice_summary: Option<String>,
    notice_at: Option<DateTime<Utc>>,
    latest_turn_status: Option<TurnStatus>,
    /// The owner read the latest turn's last reply in full, such as its
    /// failure notice.
    latest_turn_read: bool,
}

impl SidebarRow {
    pub fn status(&self, busy: bool) -> Option<SidebarStatus> {
        let preview = |text: &str| super::super::reads::preview(text);
        if let (Some(title), Some(since)) = (&self.question_title, self.question_at) {
            let preview = preview(title);
            return Some(
                if self.question_kind == Some(crate::app::questions::Kind::Choice) {
                    SidebarStatus::WaitingForAnswer {
                        preview,
                        since: Some(since),
                    }
                } else {
                    SidebarStatus::NeedsApproval {
                        preview,
                        since: Some(since),
                    }
                },
            );
        }
        if let (Some(reason), Some(since)) = (&self.handoff_reason, self.handoff_at) {
            return Some(SidebarStatus::NeedsTakeover {
                preview: preview(reason),
                since: Some(since),
            });
        }
        if let (Some(title), Some(since)) = (&self.skill_install_title, self.skill_install_at) {
            return Some(SidebarStatus::NeedsApproval {
                preview: preview(title),
                since: Some(since),
            });
        }
        if let (Some(Json(content)), Some(since)) = (&self.draft_content, self.draft_at) {
            return Some(SidebarStatus::NeedsApproval {
                preview: preview(&format!("{} to {}", content.noun(), content.audience())),
                since: Some(since),
            });
        }
        if let (Some(reason), Some(since)) = (&self.credential_reason, self.credential_at) {
            return Some(SidebarStatus::NeedsCredentials {
                preview: preview(reason),
                since: Some(since),
            });
        }
        if let (Some(provider), Some(since)) = (self.setup_provider, self.setup_at) {
            return Some(SidebarStatus::NeedsCredentials {
                preview: format!("Connect {}", provider.label()),
                since: Some(since),
            });
        }
        if let (Some(notice_id), Some(summary), Some(since)) =
            (self.notice_id, &self.notice_summary, self.notice_at)
        {
            return Some(SidebarStatus::NeedsYou {
                preview: preview(summary),
                since: Some(since),
                notice_id: Some(notice_id),
            });
        }
        if let Some(since) = self.computer_taken_at {
            return Some(SidebarStatus::HasComputer { since: Some(since) });
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
