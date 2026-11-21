use std::sync::Mutex;

use super::*;
use crate::app::outgoing::{Content, Edits, Email, Message, Status};

#[derive(Debug, Clone, PartialEq)]
struct SocialSend {
    account_id: Uuid,
    action_id: Uuid,
    platform: Platform,
    destination: String,
    body: String,
    reply_to: Option<String>,
    files: Vec<String>,
}

/// Social accounts that record each send and refuse when told to.
#[derive(Default)]
struct Recorded {
    sends: Mutex<Vec<SocialSend>>,
    refuse: bool,
}

impl Recorded {
    fn sends(&self) -> Vec<SocialSend> {
        self.sends.lock().unwrap().clone()
    }
}

#[async_trait]
impl Connected for Recorded {
    async fn send_mail(&self, _: Uuid, _: Uuid, _: MailCommand) -> Result<(), MailError> {
        unreachable!("these drafts are social messages")
    }

    async fn find_sent(&self, _: Uuid, _: String) -> Result<bool, MailError> {
        unreachable!("these drafts are social messages")
    }

    async fn send_social(
        &self,
        account_id: Uuid,
        action_id: Uuid,
        platform: Platform,
        message: SendMessage,
    ) -> Result<(), SocialError> {
        self.sends.lock().unwrap().push(SocialSend {
            account_id,
            action_id,
            platform,
            destination: message.destination,
            body: message.body,
            reply_to: message.reply_to,
            files: message.files.into_iter().map(|file| file.name).collect(),
        });
        if self.refuse {
            return Err(SocialError::Invalid(
                "The connection belongs to another platform.",
            ));
        }
        Ok(())
    }
}

const BOTH: Channels = Channels {
    mail: true,
    social: true,
};

struct Approval {
    db: SqlitePool,
    turn: Uuid,
    draft: Uuid,
    account: Uuid,
}

impl Approval {
    async fn send(&self, via: &Recorded) -> Result<OutgoingDraft, SendError> {
        send_connected(&self.db, self.turn, self.draft, BOTH, via).await
    }

    async fn status(&self) -> Status {
        sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
            .bind(self.draft)
            .fetch_one(&self.db)
            .await
            .unwrap()
    }

    async fn replace_file(&self, id: Uuid, data: &[u8]) {
        sqlx::query("UPDATE attachments SET data = ?2 WHERE id = ?1")
            .bind(id)
            .bind(data)
            .execute(&self.db)
            .await
            .unwrap();
    }
}

/// A Telegram message to chat 100 in reply to 42, drafted as "Provisional
/// text A" and approved by the owner as "Owner-approved text B", with one
/// file holding `file` when given.
async fn approved_message(file: Option<(Uuid, &[u8])>) -> Approval {
    approved(file, |account, attachments| {
        Content::Message(Message {
            platform: Platform::Telegram,
            account_id: Some(account),
            account: None,
            destination: Some("100".into()),
            task: None,
            reply_to: Some("42".into()),
            body: "Provisional text A".into(),
            attachments,
        })
    })
    .await
}

/// An email from its connected account to ana@example.com, approved by the
/// owner as "Owner-approved text B".
async fn approved_email() -> Approval {
    approved(None, |account, attachments| {
        Content::Email(Email {
            from: "owner@example.com".into(),
            to: vec!["ana@example.com".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Update".into(),
            body: "Provisional text A".into(),
            account_id: Some(account),
            reply_to: None,
            attachments,
        })
    })
    .await
}

async fn approved(
    file: Option<(Uuid, &[u8])>,
    content: impl FnOnce(Uuid, Vec<DraftFile>) -> Content,
) -> Approval {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::migrate(&db).await.unwrap();
    let bot = Uuid::new_v4();
    let conversation = Uuid::new_v4();
    let turn = Uuid::new_v4();
    let item = Uuid::new_v4();
    sqlx::query("INSERT INTO bots(id,name) VALUES(?1,'Approved send')")
        .bind(bot)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id) VALUES(?1,?2)")
        .bind(conversation)
        .bind(bot)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO turns(id,bot_id,conversation_id,request_id,request_hash,turn_no,owner_item_id,provider_id,status,attempt) VALUES(?1,?2,?3,?4,zeroblob(32),1,?5,'synthetic','running',1)")
        .bind(turn)
        .bind(bot)
        .bind(conversation)
        .bind(Uuid::new_v4())
        .bind(item)
        .execute(&db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO items(id,conversation_id,seq,turn_id,kind,text) VALUES(?1,?2,1,?3,'user','Send an update')")
        .bind(item)
        .bind(conversation)
        .bind(turn)
        .execute(&db)
        .await
        .unwrap();
    let mut attachments = Vec::new();
    if let Some((id, data)) = file {
        crate::db::attachments::store(&db, bot, id, "chart.png", "image/png", data)
            .await
            .unwrap();
        attachments.push(DraftFile {
            id,
            name: "chart.png".into(),
            media_type: "image/png".into(),
            size: data.len() as u64,
            sha256: hex::encode(Sha256::digest(data)),
        });
    }
    let account = Uuid::new_v4();
    let content = content(account, attachments);
    let draft = outgoing_drafts::propose(&db, turn, &content, None)
        .await
        .unwrap();
    let seen: Vec<Uuid> = file.iter().map(|(id, _)| *id).collect();
    let edit = Edits {
        subject: None,
        body: Some("Owner-approved text B"),
        attachments: Some(&seen),
    };
    outgoing_drafts::approve(&db, bot, draft.id, draft.version, edit, false)
        .await
        .unwrap();
    Approval {
        db,
        turn,
        draft: draft.id,
        account,
    }
}

#[tokio::test]
async fn a_fresh_send_carries_the_owner_approved_text_once() {
    let approval = approved_message(None).await;
    let via = Recorded::default();
    let draft = approval.send(&via).await.unwrap();
    let sends = via.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(
        sends[0],
        SocialSend {
            account_id: approval.account,
            action_id: sends[0].action_id,
            platform: Platform::Telegram,
            destination: "100".into(),
            body: "Owner-approved text B".into(),
            reply_to: Some("42".into()),
            files: Vec::new(),
        }
    );
    assert_eq!(draft.status, Status::Sending);
}

#[tokio::test]
async fn a_repeated_call_resumes_the_send_with_its_action_id() {
    let approval = approved_message(None).await;
    let via = Recorded::default();
    approval.send(&via).await.unwrap();
    approval.send(&via).await.unwrap();
    let sends = via.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].action_id, sends[1].action_id);
}

#[tokio::test]
async fn a_settled_send_returns_without_sending_again() {
    let approval = approved_message(None).await;
    let via = Recorded::default();
    approval.send(&via).await.unwrap();
    sqlx::query("UPDATE outgoing_drafts SET status = 'sent' WHERE id = ?1")
        .bind(approval.draft)
        .execute(&approval.db)
        .await
        .unwrap();
    let draft = approval.send(&via).await.unwrap();
    assert_eq!(draft.status, Status::Sent);
    assert_eq!(via.sends().len(), 1);
}

#[tokio::test]
async fn a_send_the_connection_refused_fails() {
    let approval = approved_message(None).await;
    let via = Recorded {
        refuse: true,
        ..Recorded::default()
    };
    let draft = approval.send(&via).await.unwrap();
    assert_eq!(draft.status, Status::Failed);
    assert_eq!(
        draft.result_detail.as_deref(),
        Some("The connection belongs to another platform.")
    );
}

#[tokio::test]
async fn a_caller_without_the_transport_claims_nothing() {
    let approval = approved_message(None).await;
    let via = Recorded::default();
    let mail_only = Channels {
        mail: true,
        social: false,
    };
    let refused = send_connected(&approval.db, approval.turn, approval.draft, mail_only, &via)
        .await
        .unwrap_err();
    assert!(
        matches!(refused, SendError::Draft(DraftError::NoTransport)),
        "{refused:?}"
    );
    assert_eq!(approval.status().await, Status::Approved);
    assert!(via.sends().is_empty());
}

#[tokio::test]
async fn a_file_changed_before_the_first_send_fails_the_draft() {
    let file = Uuid::new_v4();
    let approval = approved_message(Some((file, b"approved bytes"))).await;
    approval.replace_file(file, b"altered bytes!").await;
    let via = Recorded::default();
    let refused = approval.send(&via).await.unwrap_err();
    assert!(
        matches!(&refused, SendError::FileChanged(FileChanged(name)) if name == "chart.png"),
        "{refused:?}"
    );
    assert_eq!(approval.status().await, Status::Failed);
    assert!(via.sends().is_empty());
}

#[tokio::test]
async fn a_file_changed_after_the_claim_leaves_the_send_unknown() {
    let file = Uuid::new_v4();
    let approval = approved_message(Some((file, b"approved bytes"))).await;
    let via = Recorded::default();
    approval.send(&via).await.unwrap();
    assert_eq!(via.sends()[0].files, ["chart.png"]);
    approval.replace_file(file, b"altered bytes!").await;
    let refused = approval.send(&via).await.unwrap_err();
    assert!(
        matches!(&refused, SendError::FileChangedAfterClaim(FileChanged(name)) if name == "chart.png"),
        "{refused:?}"
    );
    assert_eq!(approval.status().await, Status::Sending);
    assert_eq!(via.sends().len(), 1);
}

#[tokio::test]
async fn a_file_change_never_fails_a_send_already_in_flight() {
    let file = Uuid::new_v4();
    let approval = approved_message(Some((file, b"approved bytes"))).await;
    let via = Recorded::default();
    approval.send(&via).await.unwrap();
    approval.replace_file(file, b"altered bytes!").await;
    let refused = outgoing_drafts::fail_unsent(&approval.db, approval.draft, "changed")
        .await
        .unwrap_err();
    assert!(matches!(refused, DraftError::Connected), "{refused:?}");
    assert_eq!(approval.status().await, Status::Sending);
}

/// The request's first call claims the email and records its send; a retry
/// resumes that claim.
async fn resumed_mail_claim(approval: &Approval) {
    let first = outgoing_drafts::claim_connected(&approval.db, approval.turn, approval.draft, BOTH)
        .await
        .unwrap();
    assert!(matches!(first, Claim::Fresh(_)), "{first:?}");
    let dispatch = outgoing_drafts::next_mail_send(
        &approval.db,
        approval.turn,
        approval.draft,
        "draft@example.com",
    )
    .await
    .unwrap();
    assert!(
        matches!(dispatch, MailSend::Dispatch { .. }),
        "{dispatch:?}"
    );
    let resumed =
        outgoing_drafts::claim_connected(&approval.db, approval.turn, approval.draft, BOTH)
            .await
            .unwrap();
    assert!(matches!(resumed, Claim::Resumed(_)), "{resumed:?}");
}

async fn settle(approval: &Approval, status: &str) {
    sqlx::query("UPDATE outgoing_drafts SET status = ?2 WHERE id = ?1")
        .bind(approval.draft)
        .bind(status)
        .execute(&approval.db)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_resumed_mail_retry_returns_the_receipt_the_first_send_recorded() {
    let approval = approved_email().await;
    resumed_mail_claim(&approval).await;
    settle(&approval, "sent").await;
    let next = outgoing_drafts::next_mail_send(
        &approval.db,
        approval.turn,
        approval.draft,
        "draft@example.com",
    )
    .await
    .unwrap();
    assert!(
        matches!(&next, MailSend::Settled(draft) if draft.status == Status::Sent),
        "{next:?}"
    );
}

#[tokio::test]
async fn a_resumed_mail_retry_of_a_discarded_draft_is_refused() {
    let approval = approved_email().await;
    resumed_mail_claim(&approval).await;
    settle(&approval, "discarded").await;
    let refused = outgoing_drafts::next_mail_send(
        &approval.db,
        approval.turn,
        approval.draft,
        "draft@example.com",
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refused, DraftError::Settled(Status::Discarded)),
        "{refused:?}"
    );
}
