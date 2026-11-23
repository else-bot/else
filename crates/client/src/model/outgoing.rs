use else_wire::outgoing::OutgoingDraft;
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DraftEdit {
    pub version: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Uuid>>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DraftAttachment {
    pub id: Uuid,
    #[serde(flatten)]
    pub edit: DraftEdit,
}

pub trait OutgoingDraftEdit {
    fn edit(&self) -> DraftEdit;
}

impl OutgoingDraftEdit for OutgoingDraft {
    fn edit(&self) -> DraftEdit {
        DraftEdit {
            version: self.version,
            subject: self.content.subject().map(str::to_owned),
            body: self.content.body().to_owned(),
            attachments: self.content.attachment_ids(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use else_wire::outgoing::{ApprovalSource, DraftContent, DraftStatus};
    use serde_json::json;

    fn file_content(channel: &str, attachments: Option<serde_json::Value>) -> DraftContent {
        let mut value = match channel {
            "email" => json!({"channel":"email", "from":"owner@example.com",
                "to":["ada@example.com"], "cc":[], "bcc":[], "subject":"Flyer", "body":"See flyer."}),
            _ => json!({"channel":"message", "platform":"whatsapp", "body":"See flyer."}),
        };
        if let Some(attachments) = attachments {
            value["attachments"] = attachments;
        }
        serde_json::from_value(value).unwrap()
    }

    fn card(content: DraftContent) -> OutgoingDraft {
        OutgoingDraft {
            id: Uuid::nil(),
            lineage_id: Uuid::nil(),
            revision: 1,
            bot_id: None,
            turn_id: None,
            content,
            version: 3,
            status: DraftStatus::Pending,
            can_approve: true,
            in_reply: false,
            result_detail: None,
            approval_source: None,
            approved_version: None,
            approved_item_id: None,
            approved_at: None,
            delivery: None,
            superseded_by: None,
            kind: None,
            created_at: None,
            updated_at: Utc::now(),
            settled_at: None,
        }
    }

    #[test]
    fn draft_files_decode_for_both_channels_and_keep_metadata_and_order() {
        let ids = [Uuid::from_u128(1), Uuid::from_u128(2)];
        let hash = "ab".repeat(32);
        for channel in ["email", "message"] {
            let draft = card(file_content(
                channel,
                Some(json!([
                    {"id":ids[0], "name":"flyer.png", "media_type":"image/png", "size":820_000, "sha256":hash},
                    {"id":ids[1], "name":"details.pdf", "media_type":"application/pdf", "size":4_200_000, "sha256":hash},
                ])),
            ));
            let files = draft.content.attachments();
            assert_eq!(files.len(), 2);
            assert_eq!(files[0].id, ids[0]);
            assert_eq!(files[0].name, "flyer.png");
            assert_eq!(files[0].media_type, "image/png");
            assert_eq!(files[0].size, 820_000);
            assert_eq!(files[0].sha256, hash);
            assert_eq!(files[1].id, ids[1]);
            assert_eq!(
                serde_json::to_value(draft.edit()).unwrap()["attachments"],
                json!(ids)
            );

            let mut edit = draft.edit();
            edit.attachments.as_mut().unwrap().remove(0);
            let reply = DraftAttachment {
                id: draft.id,
                edit: edit.clone(),
            };
            assert_eq!(
                serde_json::to_value(&edit).unwrap()["attachments"],
                json!([ids[1]])
            );
            assert_eq!(
                serde_json::to_value(reply).unwrap()["attachments"],
                json!([ids[1]])
            );
            edit.attachments.as_mut().unwrap().clear();
            assert_eq!(
                serde_json::to_value(edit).unwrap()["attachments"],
                json!([])
            );
        }
    }

    #[test]
    fn drafts_without_files_omit_attachment_ids_for_older_apis() {
        for channel in ["email", "message"] {
            for attachments in [None, Some(json!([]))] {
                let draft = card(file_content(channel, attachments));
                assert!(draft.content.attachments().is_empty());
                assert!(
                    serde_json::to_value(draft.edit())
                        .unwrap()
                        .get("attachments")
                        .is_none()
                );
                let reply = DraftAttachment {
                    id: draft.id,
                    edit: draft.edit(),
                };
                assert!(
                    serde_json::to_value(reply)
                        .unwrap()
                        .get("attachments")
                        .is_none()
                );
            }
        }
    }

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
                    attachments: None,
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
    fn a_draft_on_an_unknown_channel_stays_closed_and_asks_for_an_update() {
        let draft: OutgoingDraft = serde_json::from_value(json!({
            "id": Uuid::nil(), "lineage_id": Uuid::nil(), "revision": 1,
            "content": {"channel":"fax", "number":"+1 555", "body":"Hi"},
            "version":1, "status":"pending", "can_approve":true, "result_detail":null,
            "updated_at":"2026-11-09T09:36:00Z", "settled_at":null
        }))
        .unwrap();
        assert!(!draft.is_open());
        assert_eq!(draft.label(), "Update else.bot to review this draft");
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
            card(
                "discarded",
                None,
                Some("Your reply moved on without sending it.")
            ),
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
                attachments: None,
            },
        };
        assert_eq!(
            serde_json::to_value(attachment).unwrap(),
            json!({"id":Uuid::nil(),"version":3,"subject":"New subject","body":"Hello\n\nChanged body"})
        );
    }
}
