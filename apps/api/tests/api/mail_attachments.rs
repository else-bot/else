use crate::common;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use base64::{Engine, engine::general_purpose::STANDARD};
use common::recorder::{self, GATE_EVERY_ATTEMPT, tool};
use else_api::connectors::{store, types::*};
use serde_json::{Value, json};
use uuid::Uuid;

const NOTES: &str = "Agenda\nDividend proposal: 1.20 per share\nOther business";
const MINUTES: &str =
    "<html><body><p>Board minutes</p><p>The dividend was approved.</p></body></html>";

/// A PDF from object bodies, numbered from 1, with a correct
/// cross-reference table. Object 1 is the catalog.
fn pdf(objects: &[String]) -> Vec<u8> {
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend(format!("{} 0 obj\n{object}\nendobj\n", i + 1).bytes());
    }
    let xref = out.len();
    out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).bytes());
    for offset in offsets {
        out.extend(format!("{offset:010} 00000 n \n").bytes());
    }
    out.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .bytes(),
    );
    out
}

fn stream(body: &str, dict: &str) -> String {
    format!(
        "<< {dict} /Length {} >>\nstream\n{body}\nendstream",
        body.len()
    )
}

/// Two pages of Helvetica text.
fn report() -> Vec<u8> {
    let page = |contents: usize| {
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 3 0 R >> >> /Contents {contents} 0 R >>"
        )
    };
    pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [4 0 R 6 0 R] /Count 2 >>".into(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
        page(5),
        stream("BT /F1 12 Tf 72 720 Td (Q3 summary) Tj ET", ""),
        page(7),
        stream(
            "BT /F1 12 Tf 72 720 Td (Dividend of 1.20 per share) Tj ET",
            "",
        ),
    ])
}

/// A page that draws a form which draws itself, forever.
fn recursive() -> Vec<u8> {
    pdf(&[
        "<< /Type /Catalog /Pages 2 0 R >>".into(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
         /Resources << /XObject << /X 4 0 R >> >> /Contents 5 0 R >>"
            .into(),
        stream(
            "/X Do",
            "/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources << /XObject << /X 4 0 R >> >>",
        ),
        stream("/X Do", ""),
    ])
}

/// An inbox with one message whose attachments are a text file, an image,
/// an HTML file, a report PDF and a hostile PDF.
#[derive(Default)]
struct Inbox {
    opened: Mutex<Vec<Vec<usize>>>,
}

#[async_trait::async_trait]
impl MailTransport for Inbox {
    async fn verify(&self, _: &MailConfig, _: &Auth) -> Result<(), MailError> {
        Ok(())
    }
    async fn execute(
        &self,
        _: &MailConfig,
        _: &Auth,
        command: &MailCommand,
    ) -> Result<Value, MailError> {
        let MailCommand::OpenAttachments { parts, .. } = command else {
            return Ok(json!({}));
        };
        self.opened.lock().unwrap().push(parts.clone());
        let file = |part: usize| match part {
            0 => json!({"part":0,"name":"notes.txt","media_type":"text/plain",
                "size":NOTES.len(),"data_base64":STANDARD.encode(NOTES)}),
            1 => json!({"part":1,"name":"photo.png","media_type":"image/png",
                "size":3,"data_base64":STANDARD.encode([1, 2, 3])}),
            2 => json!({"part":2,"name":"minutes.html","media_type":"text/html",
                "size":MINUTES.len(),"data_base64":STANDARD.encode(MINUTES)}),
            3 => json!({"part":3,"name":"report.pdf","media_type":"application/pdf",
                "size":report().len(),"data_base64":STANDARD.encode(report())}),
            4 => json!({"part":4,"name":"loop.pdf","media_type":"application/octet-stream",
                "size":recursive().len(),"data_base64":STANDARD.encode(recursive())}),
            part => json!({"part":part,"error":"attachment_not_found"}),
        };
        Ok(
            json!({"attachments":parts.iter().map(|p| file(*p)).collect::<Vec<_>>(),
            "subject":"Shareholder documents","from":["registrar@example.org"]}),
        )
    }
    async fn scan(&self, _: &MailConfig, _: &Auth, _: Option<&Cursor>) -> Result<Scan, MailError> {
        Ok(Scan {
            cursor: Cursor {
                uid_validity: 1,
                last_uid: 0,
            },
            arrivals: vec![],
            more: false,
        })
    }
    async fn wait(
        &self,
        _: &MailConfig,
        _: &Auth,
        _: &Cursor,
        stop: tokio_util::sync::CancellationToken,
    ) -> Result<(), MailError> {
        stop.cancelled().await;
        Ok(())
    }
}

async fn inbox(h: &recorder::Harness, bot: &str, address: &str) -> Uuid {
    let config = MailConfig {
        provider: Provider::Imap,
        address: address.into(),
        username: address.into(),
        imap: Endpoint {
            host: "mail.example.com".into(),
            port: 993,
            start_tls: false,
        },
        smtp: Endpoint {
            host: "mail.example.com".into(),
            port: 465,
            start_tls: false,
        },
        archive_folder: "Archive".into(),
        trash_folder: "Trash".into(),
        drafts_folder: "Drafts".into(),
        sent_folder: "Sent".into(),
    };
    let account = store::create(
        &h.app.db.pool,
        h.app.state.secret_key.as_ref().unwrap(),
        &config,
        &Credential::Password {
            password: "private-app-password".into(),
        },
    )
    .await
    .unwrap();
    let path = format!("/v1/connectors/email/accounts/{}/bots/{bot}", account.id);
    let read = json!({ "read": true, "write": false, "send": false, "wake": false });
    let saved = h.app.owner("PUT", &path, Some(read)).await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    account.id
}

const MESSAGE: fn() -> Value = || json!({"folder": "INBOX", "uid_validity": 7, "uid": 42});

fn search(account: Uuid, part: usize, query: &str) -> Value {
    json!({"account_id": account, "command": {"operation": "attachment_search",
        "message": MESSAGE(), "part": part, "query": query}})
}

fn page(account: Uuid, part: usize, page: usize) -> Value {
    json!({"account_id": account, "command": {"operation": "attachment_page",
        "message": MESSAGE(), "part": part, "page": page}})
}

fn open(account: Uuid, parts: &[usize]) -> Value {
    json!({"account_id": account, "command": {"operation": "open_attachments",
        "message": {"folder": "INBOX", "uid_validity": 7, "uid": 42}, "parts": parts}})
}

async fn card(h: &recorder::Harness, bot: &str, question: &str) -> Value {
    h.app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["questions"].as_array().unwrap().clone())
        .find(|q| q["id"] == question)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_opens_attachments_the_owner_approved_and_searches_them_by_page() {
    let mail = Arc::new(Inbox::default());
    let h = recorder::harness_with(4, Some(mail.clone())).await;
    let bot = h.app.create_bot("Mail helper").await;
    let account = inbox(&h, &bot, "owner@example.com").await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;

    let parked = tool(&h.app, &token, "mail_action", open(account, &[0, 1, 2])).await;
    assert_eq!(parked.result["parked"], true, "{}", parked.body);
    let question = parked.result["question_id"].as_str().unwrap().to_owned();
    let shown = card(&h, &bot, &question).await;
    let files = &shown["action"]["mail_attachments"]["files"];
    assert_eq!(
        files
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["notes.txt", "minutes.html"],
        "the card lists only files that can be opened: {shown}"
    );
    assert_eq!(files[0]["size"], NOTES.len());
    assert_eq!(
        shown["action"]["mail_attachments"]["subject"],
        "Shareholder documents"
    );
    assert!(
        shown["choices"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["id"] == "approve" || c["id"] == "deny"),
        "no standing grant is offered: {shown}"
    );

    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["waiting"]).await;
    let answered = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({"choice":"approve"})),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let token = h.continued(turn, &token).await;
    let opened = tool(&h.app, &token, "mail_action", open(account, &[2, 1, 0])).await;
    assert!(!opened.error, "{}", opened.body);
    let statuses: Vec<_> = opened.result["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            (
                f["file"]["part"].clone(),
                f["status"].clone(),
                f["error"].clone(),
            )
        })
        .collect();
    assert_eq!(
        statuses,
        vec![
            (json!(0), json!("opened"), Value::Null),
            (
                json!(1),
                json!("unavailable"),
                json!("attachment_type_not_supported")
            ),
            (json!(2), json!("opened"), Value::Null),
        ],
        "the same files in another order reuse the approval: {}",
        opened.body
    );
    let found = tool(
        &h.app,
        &token,
        "mail_action",
        search(account, 0, "DIVIDEND"),
    )
    .await;
    assert!(!found.error, "{}", found.body);
    assert_eq!(found.result["total"], 1);
    assert_eq!(found.result["matches"][0]["page"], 1);
    assert_eq!(found.result["matches"][0]["line"], 2);
    assert_eq!(
        found.result["matches"][0]["text"],
        "Dividend proposal: 1.20 per share"
    );
    let found = tool(
        &h.app,
        &token,
        "mail_action",
        search(account, 2, "approved"),
    )
    .await;
    assert_eq!(found.result["total"], 1, "{}", found.body);
    let shown = tool(&h.app, &token, "mail_action", page(account, 0, 1)).await;
    assert_eq!(shown.result["text"], NOTES, "{}", shown.body);
    let missing = tool(&h.app, &token, "mail_action", page(account, 0, 2)).await;
    assert!(missing.error, "{}", missing.body);
    let closed = tool(
        &h.app,
        &token,
        "mail_action",
        search(account, 3, "dividend"),
    )
    .await;
    assert!(
        closed.error,
        "a part never opened is not searchable: {}",
        closed.body
    );

    let again = tool(&h.app, &token, "mail_action", open(account, &[0])).await;
    assert_eq!(
        again.result["files"][0]["status"], "opened",
        "an opened file needs no second card: {}",
        again.body
    );
    let raw = tool(
        &h.app,
        &token,
        "mail_action",
        json!({"account_id": account, "command": {"operation": "attachment",
            "message": {"folder": "INBOX", "uid_validity": 7, "uid": 42}, "part": 0}}),
    )
    .await;
    assert!(
        raw.error,
        "no operation hands out attachment bytes: {}",
        raw.body
    );
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_card_keeps_the_files_closed_and_a_bot_without_read_gets_no_card() {
    let mail = Arc::new(Inbox::default());
    let h = recorder::harness_with(4, Some(mail.clone())).await;
    let bot = h.app.create_bot("Mail helper").await;
    let account = inbox(&h, &bot, "owner@example.com").await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let parked = tool(&h.app, &token, "mail_action", open(account, &[2])).await;
    let question = parked.result["question_id"].as_str().unwrap().to_owned();
    assert_eq!(
        card(&h, &bot, &question).await["title"],
        "Open minutes.html"
    );
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["waiting"]).await;
    h.app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({"choice":"deny"})),
        )
        .await;
    let token = h.continued(turn, &token).await;
    let declined = tool(&h.app, &token, "mail_action", open(account, &[2])).await;
    assert!(!declined.error, "{}", declined.body);
    assert_eq!(declined.result["files"][0]["status"], "declined");
    assert!(declined.result["files"][0]["attachment_id"].is_null());
    let unsupported = tool(&h.app, &token, "mail_action", open(account, &[1, 9])).await;
    assert_eq!(
        unsupported.result["files"],
        json!([
            {"file":{"part":1,"name":"photo.png","media_type":"image/png","size":3},
             "status":"unavailable","error":"attachment_type_not_supported"},
            {"file":{"part":9,"name":"attachment","media_type":"application/octet-stream","size":0},
             "status":"unavailable","error":"attachment_not_found"},
        ]),
        "files that cannot be opened need no card: {}",
        unsupported.body
    );
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;

    let other = h.app.create_bot("Other").await;
    let path = format!("/v1/connectors/email/accounts/{account}/bots/{other}");
    let revoked = h.app.owner("DELETE", &path, None).await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);
    let (turn, token) = h.running(&other, GATE_EVERY_ATTEMPT).await;
    let forbidden = tool(&h.app, &token, "mail_action", open(account, &[0])).await;
    assert!(
        forbidden.error,
        "a bot without read gets no card: {}",
        forbidden.body
    );
    assert_eq!(
        mail.opened.lock().unwrap().len(),
        3,
        "a bot that cannot read the inbox fetches nothing"
    );
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pdfs_are_read_in_a_worker_that_a_hostile_file_cannot_take_down() {
    else_api::connectors::documents::pdf::use_worker(env!("CARGO_BIN_EXE_else-api").into());
    let mail = Arc::new(Inbox::default());
    let h = recorder::harness_with(4, Some(mail.clone())).await;
    let bot = h.app.create_bot("Mail helper").await;
    let account = inbox(&h, &bot, "owner@example.com").await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let parked = tool(&h.app, &token, "mail_action", open(account, &[3, 4])).await;
    let question = parked.result["question_id"].as_str().unwrap().to_owned();
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["waiting"]).await;
    h.app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({"choice":"approve"})),
        )
        .await;
    let token = h.continued(turn, &token).await;
    let opened = tool(&h.app, &token, "mail_action", open(account, &[3, 4])).await;
    assert!(!opened.error, "{}", opened.body);
    assert_eq!(
        opened.result["files"][0]["status"], "opened",
        "{}",
        opened.body
    );
    assert_eq!(opened.result["files"][0]["pages"], 2);
    assert_eq!(opened.result["files"][1]["status"], "unavailable");
    assert!(
        ["attachment_unreadable", "attachment_too_complex"]
            .contains(&opened.result["files"][1]["error"].as_str().unwrap()),
        "{}",
        opened.body
    );
    let found = tool(
        &h.app,
        &token,
        "mail_action",
        search(account, 3, "dividend"),
    )
    .await;
    assert_eq!(found.result["matches"][0]["page"], 2, "{}", found.body);

    let other = inbox(&h, &bot, "other@example.com").await;
    let elsewhere = tool(&h.app, &token, "mail_action", search(other, 3, "dividend")).await;
    assert!(
        elsewhere.error,
        "a part opened from one inbox is not searchable through another: {}",
        elsewhere.body
    );
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
}
