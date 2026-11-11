use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DraftStatus {
    Pending,
    Approved,
    Sending,
    Sent,
    Failed,
    Uncertain,
    Discarded,
    Superseded,
}

impl DraftStatus {
    pub fn can_follow_up(self) -> bool {
        matches!(self, Self::Pending | Self::Failed | Self::Uncertain)
    }

    pub fn label(self, content: &DraftContent) -> &'static str {
        match self {
            Self::Pending => "Needs approval",
            Self::Approved if content.is_post() => "Approved, waiting to post",
            Self::Approved => "Approved, waiting to send",
            Self::Sending if content.is_post() => "Posting…",
            Self::Sending if content.is_email() => "Sending email…",
            Self::Sending => "Sending message…",
            Self::Sent if content.is_post() => "Posted",
            Self::Sent if content.is_email() => "Sent email",
            Self::Sent => "Sent message",
            Self::Failed if content.is_post() => "Post not published",
            Self::Failed if content.is_email() => "Email not sent",
            Self::Failed => "Message not sent",
            Self::Uncertain if content.is_post() => "May have been posted",
            Self::Uncertain => "May have been sent",
            Self::Discarded => "Draft discarded",
            Self::Superseded => "Draft replaced",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "channel", rename_all = "lowercase")]
pub enum DraftContent {
    Email {
        from: String,
        to: Vec<String>,
        cc: Vec<String>,
        bcc: Vec<String>,
        subject: String,
        body: String,
    },
    Message {
        platform: String,
        account: Option<String>,
        destination: Option<String>,
        body: String,
    },
}

impl DraftContent {
    pub fn is_email(&self) -> bool {
        matches!(self, Self::Email { .. })
    }
    pub fn is_post(&self) -> bool {
        matches!(self, Self::Message { platform, .. } if platform == "x")
    }

    pub fn label(&self) -> &str {
        match self {
            Self::Email { .. } => "Mail",
            Self::Message { platform, .. } => match platform.as_str() {
                "x" => "X",
                "telegram" => "Telegram",
                "discord" => "Discord",
                _ => platform,
            },
        }
    }

    pub fn body(&self) -> &str {
        match self {
            Self::Email { body, .. } | Self::Message { body, .. } => body,
        }
    }

    pub fn subject(&self) -> Option<&str> {
        match self {
            Self::Email { subject, .. } => Some(subject),
            Self::Message { .. } => None,
        }
    }

    pub fn audience(&self) -> String {
        match self {
            Self::Email { to, .. } => to.join(", "),
            Self::Message {
                destination: Some(destination),
                ..
            } => destination.clone(),
            Self::Message {
                account: Some(account),
                ..
            } => account.clone(),
            Self::Message { .. } if self.is_post() => "Public post".into(),
            Self::Message { .. } => String::new(),
        }
    }

    pub fn metadata(&self) -> Vec<(&'static str, String)> {
        match self {
            Self::Email {
                from, to, cc, bcc, ..
            } => {
                let mut rows = vec![("To", to.join(", ")), ("From", from.clone())];
                if !cc.is_empty() {
                    rows.push(("Cc", cc.join(", ")));
                }
                if !bcc.is_empty() {
                    rows.push(("Bcc", bcc.join(", ")));
                }
                rows
            }
            Self::Message {
                platform,
                account,
                destination,
                ..
            } => {
                let mut rows = Vec::new();
                if let Some(account) = account {
                    rows.push(("Account", account.clone()));
                }
                if let Some(destination) = destination {
                    rows.push((
                        match platform.as_str() {
                            "telegram" => "Chat",
                            "x" => "Reply to",
                            _ => "Destination",
                        },
                        destination.clone(),
                    ));
                }
                rows
            }
        }
    }

    /// Who and where "Approve always" covers, as in "X posts from @acme".
    pub fn kind_label(&self) -> String {
        let Self::Message {
            account,
            destination,
            ..
        } = self
        else {
            return "emails".into();
        };
        let mut label = format!(
            "{} {}",
            self.label(),
            if self.is_post() { "posts" } else { "messages" }
        );
        if let Some(account) = account {
            label.push_str(&format!(" from {account}"));
        }
        if let Some(destination) = destination {
            label.push_str(&format!(" to {destination}"));
        }
        label
    }

    pub fn approve_label(&self) -> &'static str {
        if self.is_post() {
            "Approve & post"
        } else {
            "Approve & send"
        }
    }
    pub fn body_label(&self) -> &'static str {
        if self.is_email() {
            "Email body"
        } else if self.is_post() {
            "Post text"
        } else {
            "Message text"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalSource {
    Button,
    Message,
    /// Approved as proposed, because the owner approves its kind always.
    Standing,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OutgoingDraft {
    pub id: Uuid,
    pub lineage_id: Uuid,
    pub revision: i64,
    pub content: DraftContent,
    pub version: i64,
    pub status: DraftStatus,
    pub can_approve: bool,
    /// Handed to the live request of the owner reply it was sent with.
    #[serde(default)]
    pub in_reply: bool,
    pub result_detail: Option<String>,
    #[serde(default)]
    pub approval_source: Option<ApprovalSource>,
    /// What "Approve always" would cover; none for an email or an X reply.
    #[serde(default)]
    pub kind: Option<serde_json::Value>,
    pub updated_at: DateTime<Utc>,
    pub settled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DraftEdit {
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DraftAttachment {
    pub id: Uuid,
    #[serde(flatten)]
    pub edit: DraftEdit,
}

impl OutgoingDraft {
    /// Open on its card for the owner to edit, approve, or discard.
    pub fn is_open(&self) -> bool {
        self.status == DraftStatus::Pending && !self.in_reply
    }

    pub fn label(&self) -> &'static str {
        if self.status == DraftStatus::Pending && self.in_reply {
            "Continued in your reply"
        } else {
            self.status.label(&self.content)
        }
    }

    pub fn edit(&self) -> DraftEdit {
        DraftEdit {
            version: self.version,
            subject: self.content.subject().map(str::to_owned),
            body: self.content.body().to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn connected_email_cards_keep_edits_and_read_mail_receipts() {
        let card: OutgoingDraft = serde_json::from_value(json!({
            "id": Uuid::nil(), "lineage_id": Uuid::nil(), "revision": 1,
            "content": {"channel":"email", "from":"owner@example.com",
                "to":["ada@example.com"], "cc":[], "bcc":[],
                "subject":"Edited reply", "body":"Friday works.",
                "account_id":Uuid::nil(),
                "reply_to":{"folder":"INBOX","uid_validity":7,"uid":42}},
            "version":2,"status":"sent","can_approve":false,
            "delivery":{"via":"mail","account_id":Uuid::nil(),"action_id":Uuid::nil()},
            "result_detail":"The mail server accepted it from the connected account.",
            "updated_at":"2026-11-05T12:00:00Z","settled_at":"2026-11-05T12:00:00Z"
        }))
        .unwrap();
        assert_eq!(card.edit().subject.as_deref(), Some("Edited reply"));
        assert_eq!(card.edit().body, "Friday works.");
        assert_eq!(card.edit().version, 2);
        assert_eq!(card.status, DraftStatus::Sent);
        assert_eq!(
            card.result_detail.as_deref(),
            Some("The mail server accepted it from the connected account.")
        );
        assert_eq!(card.status.label(&card.content), "Sent email");
        assert!(
            card.content
                .metadata()
                .contains(&("From", "owner@example.com".into()))
        );
    }

    #[test]
    fn message_edits_and_metadata_have_no_email_fields() {
        for platform in ["x", "telegram", "future-platform"] {
            let content: DraftContent = serde_json::from_value(json!({"channel":"message", "platform":platform, "account":"@team", "destination":"Project chat", "body":"Edited message"})).unwrap();
            assert_eq!(content.subject(), None);
            assert!(
                content
                    .metadata()
                    .iter()
                    .all(|(label, _)| !["From", "To", "Cc", "Bcc", "Subject"].contains(label))
            );
            let edit = DraftAttachment {
                id: Uuid::nil(),
                edit: DraftEdit {
                    version: 3,
                    subject: content.subject().map(str::to_owned),
                    body: content.body().to_owned(),
                },
            };
            let payload = serde_json::to_value(edit).unwrap();
            assert!(payload.get("subject").is_none());
            assert_eq!(payload["body"], "Edited message");
            assert!(!DraftStatus::Sent.label(&content).contains("email"));
        }
    }

    #[test]
    fn a_draft_handed_to_a_reply_is_no_longer_open_on_its_card() {
        let card = |status: &str, in_reply: bool| -> OutgoingDraft {
            serde_json::from_value(json!({
                "id": Uuid::nil(), "lineage_id": Uuid::nil(), "revision": 1,
                "content": {"channel":"message", "platform":"telegram", "body":"Hi"},
                "version":1, "status":status, "can_approve":false, "in_reply":in_reply,
                "result_detail":null, "updated_at":"2026-11-09T09:36:00Z", "settled_at":null
            }))
            .unwrap()
        };
        assert!(card("pending", false).is_open());
        assert!(!card("pending", true).is_open());
        assert_eq!(card("pending", true).label(), "Continued in your reply");
        assert_eq!(card("discarded", false).label(), "Draft discarded");
    }

    #[test]
    fn an_always_approved_post_names_what_approve_always_covers() {
        let card: OutgoingDraft = serde_json::from_value(json!({
            "id": Uuid::nil(), "lineage_id": Uuid::nil(), "revision": 1,
            "content": {"channel":"message", "platform":"x", "account":"@proliquid_xyz",
                "destination":null, "body":"A whale flipped long."},
            "version":1, "status":"approved", "can_approve":false,
            "approval_source":"standing",
            "kind":{"platform":"x","account_id":null,"account":"@proliquid_xyz","destination":null},
            "result_detail":null,
            "updated_at":"2026-11-11T06:30:00Z", "settled_at":null
        }))
        .unwrap();
        assert_eq!(card.approval_source, Some(ApprovalSource::Standing));
        assert!(card.kind.is_some());
        assert_eq!(card.content.kind_label(), "X posts from @proliquid_xyz");
        let chat: DraftContent = serde_json::from_value(json!({"channel":"message",
            "platform":"telegram", "account":"@main_bot", "destination":"Family", "body":"Hi"}))
        .unwrap();
        assert_eq!(
            chat.kind_label(),
            "Telegram messages from @main_bot to Family"
        );
    }

    #[test]
    fn email_attachment_keeps_its_subject_and_exact_version() {
        let attachment = DraftAttachment {
            id: Uuid::nil(),
            edit: DraftEdit {
                version: 3,
                subject: Some("New subject".into()),
                body: "Hello\n\nChanged body".into(),
            },
        };
        assert_eq!(
            serde_json::to_value(attachment).unwrap(),
            json!({"id":Uuid::nil(),"version":3,"subject":"New subject","body":"Hello\n\nChanged body"})
        );
    }
}
