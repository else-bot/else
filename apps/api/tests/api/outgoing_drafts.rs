use crate::common;
use else_api::db::turns::TurnStatus;

use std::sync::Arc;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, Harness, harness, tool};
use else_api::app::outgoing::{Content, Delivery, send};
use else_api::connectors::{store, types::*};
use else_api::db::outgoing_drafts::{self, Claim, DraftError, MailSend, Payload};
use serde_json::{Value, json};
use uuid::Uuid;
mod always;
mod approval;
mod attachments;
mod channels;
mod connected_email;
mod delegated;
mod uncertain_sends;

/// A caller that may send through connected mail only.
const MAIL: else_api::app::outgoing::Channels = else_api::app::outgoing::Channels {
    mail: true,
    social: false,
};

#[derive(Default)]
struct FakeMail {
    commands: std::sync::Mutex<Vec<MailCommand>>,
    connection_lost: std::sync::atomic::AtomicBool,
    fail_at: std::sync::Mutex<Option<SendPhase>>,
    in_sent: std::sync::atomic::AtomicBool,
    search_fails: std::sync::atomic::AtomicBool,
}

impl FakeMail {
    fn sends(&self) -> Vec<Draft> {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                MailCommand::Send { draft } => Some(draft.clone()),
                _ => None,
            })
            .collect()
    }

    fn searches(&self) -> Vec<String> {
        self.commands
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| match c {
                MailCommand::FindSent { message_id } => Some(message_id.clone()),
                _ => None,
            })
            .collect()
    }

    fn fail_at(&self, phase: Option<SendPhase>) {
        *self.fail_at.lock().unwrap() = phase;
    }
}

#[async_trait::async_trait]
impl MailTransport for FakeMail {
    async fn verify(&self, _: &MailConfig, _: &Auth) -> Result<(), MailError> {
        Ok(())
    }
    async fn execute(
        &self,
        _: &MailConfig,
        _: &Auth,
        command: &MailCommand,
    ) -> Result<Value, MailError> {
        self.commands.lock().unwrap().push(command.clone());
        let flag = |f: &std::sync::atomic::AtomicBool| f.load(std::sync::atomic::Ordering::SeqCst);
        if let MailCommand::FindSent { .. } = command {
            if flag(&self.search_fails) {
                return Err(MailError::new(MailErrorCode::MailTimeout));
            }
            return Ok(json!({ "found": flag(&self.in_sent), "folder": "Sent" }));
        }
        if flag(&self.connection_lost) {
            return Err(MailError::new(MailErrorCode::MailTransportFailed));
        }
        if let Some(phase) = *self.fail_at.lock().unwrap() {
            return Err(MailError::new(MailErrorCode::MailTransportFailed)
                .cause("the mail server closed the connection")
                .at(phase));
        }
        Ok(json!({ "sent": true }))
    }
    async fn fetch(&self, _: &MailConfig, _: &Auth, _: &MessageRef) -> Result<Fetched, MailError> {
        Err(MailError::new(MailErrorCode::MessageNotFound))
    }
    async fn reader(&self, _: &MailConfig, _: &Auth) -> Result<Box<dyn MailReader>, MailError> {
        Err(MailError::new(MailErrorCode::MailTransportFailed))
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

async fn connect(app: &TestApp, bot: &str, address: &str, send: bool) -> Uuid {
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
        &app.db.pool,
        &app.state.secret_key,
        &config,
        &Credential::Password {
            password: "private-app-password".into(),
        },
    )
    .await
    .unwrap();
    permit(app, account.id, bot, send).await;
    account.id
}

async fn permit(app: &TestApp, account: Uuid, bot: &str, send: bool) {
    let path = format!("/v1/connectors/email/accounts/{account}/bots/{bot}");
    let saved = if send {
        let all = json!({ "read": true, "write": true, "send": true, "wake": false });
        app.owner("PUT", &path, Some(all)).await
    } else {
        app.owner("DELETE", &path, None).await
    };
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
}

fn connected_reply(account: Uuid) -> Value {
    json!({ "account_id": account, "reply_to": { "folder": "INBOX", "uid_validity": 7, "uid": 42 },
            "to": ["ada@example.com"], "cc": ["bob@example.com"], "subject": "Re: Lunch",
            "body": "Hi Ada,\n\nFriday at noon works.\n\nMain" })
}

async fn send_with_draft(app: &TestApp, bot: &str, text: &str, draft: Value) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/turns"),
        Some(json!({ "request_id": Uuid::new_v4(), "text": text, "open_draft": draft })),
    )
    .await
}

async fn attempt(app: &TestApp, turn: Uuid) -> i32 {
    sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

/// Send an owner message that joins an unfinished reply, optionally with a draft open.
async fn joined(
    h: &Harness,
    bot: &str,
    into: Uuid,
    previous: &str,
    text: &str,
    draft: Option<Value>,
) -> (Uuid, String) {
    let app = &h.app;
    let active = status(app, into).await == "running";
    let sent = match draft {
        Some(draft) => send_with_draft(app, bot, text, draft).await,
        None => app.send(bot, Uuid::new_v4(), text, None).await,
    };
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(
        sent.body["turn"]["steered_into"],
        into.to_string(),
        "{}",
        sent.body
    );
    let turn = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    if active {
        h.release(into).await;
    }
    (turn, h.continued(into, previous).await)
}

fn card_of(cards: &[Value], id: Uuid) -> Value {
    cards
        .iter()
        .find(|c| c["id"] == id.to_string())
        .unwrap_or_else(|| panic!("no card {id} in {cards:?}"))
        .clone()
}

fn lunch() -> Value {
    json!({ "from": "main@bots.example", "to": ["ada@example.com"], "cc": ["bob@example.com"],
            "subject": "Lunch on Friday?", "body": "Hi Ada,\n\nLunch on Friday at noon?\n\nMain" })
}

fn ship_post() -> Value {
    json!({ "platform": "x", "body": "We shipped native drafts today." })
}

fn family_message() -> Value {
    json!({ "platform": "telegram", "account": "@main_bot", "destination": "Family group",
            "body": "Dinner at 8 on Friday?" })
}

async fn draft(app: &TestApp, token: &str, args: Value) -> Uuid {
    let name = if args.get("platform").is_some() {
        "draft_message"
    } else {
        "draft_email"
    };
    let drafted = tool(app, token, name, args).await;
    let result = &drafted.result;
    assert_eq!(result["status"], "pending", "{}", drafted.body);
    assert_eq!(result["parked"], true);
    result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

async fn owner_action(
    app: &TestApp,
    bot: &str,
    draft: Uuid,
    action: &str,
    body: Value,
) -> common::Reply {
    let (method, path) = match action {
        "edit" => ("PUT", format!("/v1/bots/{bot}/outgoing-drafts/{draft}")),
        _ => (
            "POST",
            format!("/v1/bots/{bot}/outgoing-drafts/{draft}/{action}"),
        ),
    };
    app.owner(method, &path, Some(body)).await
}

async fn status(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn shown(app: &TestApp, bot: &str, turn: Uuid) -> Vec<Value> {
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .expect("the turn is in the snapshot")["outgoing_drafts"]
        .as_array()
        .unwrap()
        .clone()
}

async fn journaled(app: &TestApp, bot: &str, draft: Uuid) -> Value {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, data FROM bot_events WHERE bot_id = ?1 ORDER BY seq")
            .bind(bot.parse::<Uuid>().unwrap())
            .fetch_all(&app.db.pool)
            .await
            .unwrap();
    rows.iter()
        .filter(|(kind, _)| kind == "turn.updated")
        .filter_map(|(_, data)| serde_json::from_str::<Value>(data).ok())
        .filter_map(|data| {
            data["turn"]["outgoing_drafts"]
                .as_array()?
                .iter()
                .find(|d| d["id"] == draft.to_string())
                .cloned()
        })
        .next_back()
        .expect("the draft was journaled")
}

async fn questions(app: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

fn send_args(draft: Uuid) -> Value {
    json!({ "command": "python3 send_mail.py", "effect": "external", "approved_draft": draft })
}

/// A task the main bot delegated to another bot while serving an owner
/// message, running: the bot's ID, the root turn, the delegation, and the
/// task's turn.
async fn delegated_task(h: &Harness) -> (String, Uuid, Uuid, Uuid) {
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Telegram").await;
    let warm = h.send(&leaf, "hello").await;
    app.wait_turn(warm, &["completed"]).await;
    let (root, parent) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &parent,
        "delegate",
        json!({ "target_bot_id": leaf, "task": GATE_EVERY_ATTEMPT }),
    )
    .await;
    let task: Uuid = delegated.result["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("delegated: {}", delegated.body))
        .parse()
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    h.release(root).await;
    (leaf, root, task, child)
}

/// A bot's project directory with `files` written into it.
fn project(root: &std::path::Path, bot: &str, files: &[(&str, &[u8])]) {
    let dir = else_api::providers::host::project_dir(root, bot.parse().unwrap()).unwrap();
    for (name, data) in files {
        std::fs::write(dir.join(name), data).unwrap();
    }
}

const PDF: &[u8] = b"%PDF-1.7\n1 0 obj << >> endobj\ntrailer << >>\n%%EOF\n";

fn files_of(card: &Value) -> Vec<Value> {
    card["content"]["attachments"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}
