use serde::Serialize;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SidebarRow {
    question_kind: Option<String>,
    question_title: Option<String>,
    credential_reason: Option<String>,
    skill_install_title: Option<String>,
    latest_turn_status: Option<String>,
    /// The owner read the latest turn's last reply in full, such as its
    /// failure notice.
    latest_turn_read: bool,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SidebarStatus {
    WaitingForAnswer { preview: String },
    NeedsApproval { preview: String },
    NeedsCredentials { preview: String },
    Failed,
    Stopped,
    Uncertain,
}

impl SidebarRow {
    pub fn status(&self, busy: bool) -> Option<SidebarStatus> {
        if let Some(title) = &self.question_title {
            let preview = super::super::reads::preview(title);
            return Some(if self.question_kind.as_deref() == Some("choice") {
                SidebarStatus::WaitingForAnswer { preview }
            } else {
                SidebarStatus::NeedsApproval { preview }
            });
        }
        if let Some(title) = &self.skill_install_title {
            return Some(SidebarStatus::NeedsApproval {
                preview: super::super::reads::preview(title),
            });
        }
        if let Some(reason) = &self.credential_reason {
            return Some(SidebarStatus::NeedsCredentials {
                preview: super::super::reads::preview(reason),
            });
        }
        if busy || self.latest_turn_read {
            return None;
        }
        match self.latest_turn_status.as_deref() {
            Some("failed") => Some(SidebarStatus::Failed),
            Some("interrupted") => Some(SidebarStatus::Stopped),
            Some("uncertain") => Some(SidebarStatus::Uncertain),
            _ => None,
        }
    }
}
