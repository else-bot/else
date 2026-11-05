use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EmailStatus {
    Pending,
    Approved,
    Sending,
    Sent,
    Failed,
    Uncertain,
    Discarded,
    Superseded,
}

impl EmailStatus {
    pub fn can_follow_up(self) -> bool {
        matches!(self, Self::Pending | Self::Failed | Self::Uncertain)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "Needs approval",
            Self::Approved => "Approved, waiting to send",
            Self::Sending => "Sending email…",
            Self::Sent => "Sent email",
            Self::Failed => "Email not sent",
            Self::Uncertain => "May have been sent",
            Self::Discarded => "Draft discarded",
            Self::Superseded => "Draft replaced",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct EmailDraft {
    pub id: Uuid,
    pub email_id: Uuid,
    pub revision: i64,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub version: i64,
    pub status: EmailStatus,
    pub can_approve: bool,
    pub result_detail: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EmailEdit {
    pub version: i64,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EmailAttachment {
    pub id: Uuid,
    #[serde(flatten)]
    pub edit: EmailEdit,
}

impl EmailDraft {
    pub fn edit(&self) -> EmailEdit {
        EmailEdit {
            version: self.version,
            subject: self.subject.clone(),
            body: self.body.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_attachment_carries_the_exact_version_and_edits() {
        let id = Uuid::new_v4();
        let attachment = EmailAttachment {
            id,
            edit: EmailEdit {
                version: 3,
                subject: "New subject".into(),
                body: "Hello\n\nChanged body".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(attachment).unwrap(),
            serde_json::json!({"id":id,"version":3,"subject":"New subject","body":"Hello\n\nChanged body"})
        );
    }
}
