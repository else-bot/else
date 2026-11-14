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

    /// What became of the draft, without who approved it.
    pub fn label(self, content: &DraftContent) -> &'static str {
        match self {
            Self::Pending => "Needs approval",
            Self::Approved if content.is_post() => "Waiting to post",
            Self::Approved => "Waiting to send",
            Self::Sending if content.is_post() => "Posting…",
            Self::Sending if content.is_email() => "Sending email…",
            Self::Sending => "Sending message…",
            Self::Sent if content.is_post() => "Posted",
            Self::Sent if content.is_email() => "Email sent",
            Self::Sent => "Message sent",
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
        /// The recurring task the bot filed it under.
        task: Option<String>,
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

    /// What a receipt inside a task leads with: the email's subject, else
    /// what the draft is.
    pub fn title(&self) -> String {
        match self {
            Self::Email { subject, .. } if !subject.is_empty() => subject.clone(),
            Self::Email { .. } => "Email".into(),
            Self::Message { .. } if self.is_post() => "X post".into(),
            Self::Message { .. } => format!("{} message", self.label()),
        }
    }

    /// Who it goes to, when it names someone.
    fn recipient(&self) -> Option<String> {
        match self {
            Self::Email { to, .. } => Some(to.join(", ")),
            Self::Message { destination, .. } => destination.clone(),
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
                task,
                ..
            } => {
                let mut rows = Vec::new();
                if let Some(task) = task {
                    rows.push(("Task", task.clone()));
                }
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

    /// Who and where "Approve always" covers, as in "X posts from @acme",
    /// or for a task, "Initial lead DMs: X messages from @acme to anyone".
    pub fn kind_label(&self) -> String {
        let Self::Message {
            account,
            destination,
            task,
            ..
        } = self
        else {
            return "emails".into();
        };
        let mut label = format!(
            "{}{} {}",
            task.as_ref().map(|t| format!("{t}: ")).unwrap_or_default(),
            self.label(),
            if self.is_post() && destination.is_none() && task.is_none() {
                "posts"
            } else {
                "messages"
            }
        );
        if let Some(account) = account {
            label.push_str(&format!(" from {account}"));
        }
        match (task, destination) {
            (Some(_), _) => label.push_str(" to anyone"),
            (None, Some(destination)) => label.push_str(&format!(" to {destination}")),
            (None, None) => {}
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

/// The detail the API writes when it closes a draft because the owner's reply
/// moved on (migration `0037_reply_closes_drafts`), not by the Discard button.
const REPLY_MOVED_ON: &str = "Your reply moved on without sending it.";

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

    /// The receipt line: who acted, then what became of the draft.
    pub fn label(&self) -> String {
        match self.status {
            DraftStatus::Pending if self.in_reply => "Continued in your reply".into(),
            DraftStatus::Discarded if self.result_detail.as_deref() == Some(REPLY_MOVED_ON) => {
                "Closed by your reply".into()
            }
            // Otherwise only the owner's Discard button discards a draft.
            DraftStatus::Discarded => "You discarded this draft".into(),
            _ => {
                let outcome = self.status.label(&self.content);
                match self.approver() {
                    Some(who) => format!("{who} · {outcome}"),
                    None => outcome.into(),
                }
            }
        }
    }

    /// Who approved the draft, once it has left review.
    fn approver(&self) -> Option<&'static str> {
        match (self.status, self.approval_source) {
            (DraftStatus::Pending | DraftStatus::Superseded, _) => None,
            (_, Some(ApprovalSource::Button)) => Some("You approved"),
            (_, Some(ApprovalSource::Message)) => Some("You approved in chat"),
            (_, Some(ApprovalSource::Standing)) => Some("Approved by your standing rule"),
            (DraftStatus::Approved, None) => Some("Approved"),
            (_, None) => None,
        }
    }

    /// The receipt's title and outcome lines inside a task, whose meta row
    /// already carries the time.
    pub fn task_receipt(&self) -> (String, String) {
        let outcome = match self.content.recipient() {
            Some(to) => format!("{} · to {to}", self.label()),
            None => self.label(),
        };
        (self.content.title(), outcome)
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
        assert_eq!(card.status.label(&card.content), "Email sent");
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
        assert_eq!(card("discarded", false).label(), "You discarded this draft");
    }

    #[test]
    fn the_reply_closed_detail_matches_the_migration_that_writes_it() {
        assert!(
            include_str!("../../../api/migrations/0037_reply_closes_drafts.sql")
                .contains(REPLY_MOVED_ON),
            "rewording the trigger's detail would credit the owner with its discards"
        );
    }

    #[test]
    fn a_receipt_names_who_approved_or_discarded_it() {
        let card = |status: &str, source: Option<&str>, detail: Option<&str>| -> String {
            serde_json::from_value::<OutgoingDraft>(json!({
                "id": Uuid::nil(), "lineage_id": Uuid::nil(), "revision": 1,
                "content": {"channel":"email", "from":"me@example.com", "to":["ada@example.com"],
                    "cc":[], "bcc":[], "subject":"Hi", "body":"Hi"},
                "version":1, "status":status, "can_approve":false,
                "approval_source":source, "result_detail":detail,
                "updated_at":"2026-11-14T11:00:00Z", "settled_at":"2026-11-14T11:00:00Z"
            }))
            .unwrap()
            .label()
        };
        assert_eq!(
            card("sent", Some("button"), None),
            "You approved · Email sent"
        );
        assert_eq!(
            card("sent", Some("message"), None),
            "You approved in chat · Email sent"
        );
        assert_eq!(
            card("sent", Some("standing"), None),
            "Approved by your standing rule · Email sent"
        );
        assert_eq!(
            card("failed", Some("button"), Some("Refused")),
            "You approved · Email not sent"
        );
        assert_eq!(
            card("approved", Some("standing"), None),
            "Approved by your standing rule · Waiting to send"
        );
        assert_eq!(card("approved", None, None), "Approved · Waiting to send");
        assert_eq!(card("sent", None, None), "Email sent");
        assert_eq!(card("discarded", None, None), "You discarded this draft");
        assert_eq!(
            card("discarded", Some("button"), Some("Email not sent: refused")),
            "You discarded this draft",
            "a failed draft the owner discarded keeps its failure detail"
        );
        assert_eq!(
            card("discarded", None, Some(REPLY_MOVED_ON)),
            "Closed by your reply"
        );
        assert_eq!(card("superseded", Some("button"), None), "Draft replaced");
        assert_eq!(card("pending", None, None), "Needs approval");
    }

    #[test]
    fn a_receipt_inside_a_task_leads_with_the_draft_then_the_decision() {
        let card = |content: serde_json::Value, status: &str, source: Option<&str>| {
            serde_json::from_value::<OutgoingDraft>(json!({
                "id": Uuid::nil(), "lineage_id": Uuid::nil(), "revision": 1,
                "content": content, "version":1, "status":status, "can_approve":false,
                "approval_source":source, "result_detail":null,
                "updated_at":"2026-11-14T11:00:00Z", "settled_at":"2026-11-14T11:00:00Z"
            }))
            .unwrap()
            .task_receipt()
        };
        let email = json!({"channel":"email", "from":"me@example.com",
            "to":["iam4x@me.com"], "cc":[], "bcc":[], "subject":"Proposal", "body":"Hi"});
        let lines = |a: &str, b: &str| (a.to_owned(), b.to_owned());
        assert_eq!(
            card(email.clone(), "discarded", None),
            lines("Proposal", "You discarded this draft · to iam4x@me.com")
        );
        assert_eq!(
            card(email.clone(), "sent", Some("button")),
            lines("Proposal", "You approved · Email sent · to iam4x@me.com")
        );
        assert_eq!(
            card(email, "sent", Some("standing")),
            lines(
                "Proposal",
                "Approved by your standing rule · Email sent · to iam4x@me.com"
            )
        );
        assert_eq!(
            card(
                json!({"channel":"message", "platform":"telegram",
                "destination":"Family", "body":"Hi"}),
                "sent",
                Some("button")
            ),
            lines(
                "Telegram message",
                "You approved · Message sent · to Family"
            )
        );
        assert_eq!(
            card(
                json!({"channel":"message", "platform":"x", "account":"@acme",
                "body":"Hi"}),
                "discarded",
                None
            ),
            lines("X post", "You discarded this draft")
        );
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
        let lead: DraftContent = serde_json::from_value(json!({"channel":"message",
            "platform":"x", "account":"@proliquid_xyz", "destination":"@ada",
            "task":"Initial lead DMs", "body":"Hi"}))
        .unwrap();
        assert_eq!(
            lead.kind_label(),
            "Initial lead DMs: X messages from @proliquid_xyz to anyone"
        );
        assert_eq!(lead.metadata()[0], ("Task", "Initial lead DMs".into()));
        let reply: DraftContent = serde_json::from_value(json!({"channel":"message",
            "platform":"x", "account":"@proliquid_xyz", "destination":null,
            "task":"Initial lead DMs", "body":"Thanks"}))
        .unwrap();
        assert_eq!(
            reply.kind_label(),
            "Initial lead DMs: X messages from @proliquid_xyz to anyone",
            "a task grant on a reply also covers DMs"
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
