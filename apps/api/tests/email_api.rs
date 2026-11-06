mod common;

use axum::http::StatusCode;
use common::{
    TestApp,
    recorder::{self, GATE, tool},
};
use else_api::{
    connectors::{ConnectorManager, store, types::*},
    grants::Tool,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn config() -> MailConfig {
    MailConfig {
        provider: Provider::Imap,
        address: "bot@example.com".into(),
        username: "bot@example.com".into(),
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
    }
}
async fn account(app: &TestApp) -> Account {
    store::create(
        &app.db.pool,
        app.state.secret_key.as_ref().unwrap(),
        &config(),
        &Credential::Password {
            password: "private-app-password".into(),
        },
    )
    .await
    .unwrap()
}
fn permissions() -> Permissions {
    Permissions {
        read: true,
        write: true,
        send: true,
        wake: true,
    }
}

#[tokio::test]
async fn microsoft_desktop_sign_in_uses_public_client_pkce_and_outlook_scopes() {
    let app = TestApp::installed().await;
    let client_id = Uuid::new_v4().to_string();
    let started = app
        .owner(
            "POST",
            "/v1/connectors/email/oauth/start",
            Some(json!({
                "provider":"microsoft", "address":"owner@example.com", "client_id":client_id,
                "redirect_uri":"http://localhost:12345/"
            })),
        )
        .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    let url = reqwest::Url::parse(started.body["authorization_url"].as_str().unwrap()).unwrap();
    assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
    assert_eq!(url.path(), "/common/oauth2/v2.0/authorize");
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(params["client_id"], client_id);
    assert_eq!(params["login_hint"], "owner@example.com");
    assert_eq!(params["redirect_uri"], "http://localhost:12345/");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(!params.contains_key("client_secret"));
    let scopes: Vec<_> = params["scope"].split_whitespace().collect();
    for scope in [
        "offline_access",
        "https://outlook.office.com/IMAP.AccessAsUser.All",
        "https://outlook.office.com/SMTP.Send",
    ] {
        assert!(scopes.contains(&scope));
    }
}

#[tokio::test]
async fn desktop_oauth_can_choose_google_account_and_cancel_owner_only() {
    let h = recorder::harness(4).await;
    let bot = h.app.main_bot().await;
    let (_, task_token) = h.running(&bot, GATE).await;
    let start = h.app.owner("POST", "/v1/connectors/email/oauth/start", Some(json!({
        "provider":"gmail", "client_id":"desktop-client", "client_secret":"desktop-client-configuration",
        "redirect_uri":"http://127.0.0.1:45678/google/callback"
    }))).await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.body);
    assert!(
        !start
            .body
            .to_string()
            .contains("desktop-client-configuration")
    );
    let path = format!(
        "/v1/connectors/email/oauth/{}",
        start.body["request_id"].as_str().unwrap()
    );
    assert_eq!(
        h.app
            .call("DELETE", &path, None, Some(&task_token))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        assert_eq!(
            h.app.owner("DELETE", &path, None).await.status,
            StatusCode::OK
        );
    }
    let url = reqwest::Url::parse(start.body["authorization_url"].as_str().unwrap()).unwrap();
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let complete = h
        .app
        .owner(
            "POST",
            "/v1/connectors/email/oauth/complete",
            Some(json!({"request_id":start.body["request_id"],"state":state,"code":"unused"})),
        )
        .await;
    assert_eq!(complete.status, StatusCode::BAD_REQUEST);
    assert_eq!(complete.body["error"]["code"], "oauth_setup_expired");
}

#[tokio::test]
async fn owner_can_connect_pause_replace_credentials_and_disconnect_privately() {
    let app = TestApp::installed().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stub = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            drop(socket);
        }
    });
    let mut config = config();
    config.imap = Endpoint {
        host: "127.0.0.1".into(),
        port,
        start_tls: false,
    };
    config.smtp = config.imap.clone();
    let created = app.owner("POST","/v1/connectors/email/accounts",Some(json!({"config":config,"credential":{"kind":"password","password":"private-app-password"}}))).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    assert!(!created.body.to_string().contains("private-app-password"));
    let duplicate = app.owner("POST", "/v1/connectors/email/accounts", Some(json!({"config":config,"credential":{"kind":"password","password":"private-app-password"}}))).await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.body["error"]["code"], "mail_account_exists");
    let path = format!(
        "/v1/connectors/email/accounts/{}",
        created.body["account"]["id"].as_str().unwrap()
    );
    assert_eq!(
        app.owner("PATCH", &path, Some(json!({"active":false})))
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.owner(
            "PUT",
            &format!("{path}/credentials"),
            Some(json!({"kind":"password","password":"replacement-app-password"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let read = app.owner("GET", &path, None).await;
    assert_eq!(read.body["account"]["state"], "paused");
    assert!(read.body["grants"].as_array().unwrap().is_empty());
    assert!(!read.body.to_string().contains("replacement-app-password"));
    let mut config = read.body["account"]["config"].clone();
    config["archive_folder"] = json!("Archives");
    let saved = app
        .owner("PUT", &format!("{path}/config"), Some(config.clone()))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        saved.body["account"]["config"]["archive_folder"],
        "Archives"
    );
    let again = app
        .owner("PUT", &format!("{path}/config"), Some(config.clone()))
        .await;
    assert_eq!(
        again.body["account"]["generation"],
        saved.body["account"]["generation"]
    );
    config["imap"]["host"] = json!("another.example.com");
    assert_eq!(
        app.owner("PUT", &format!("{path}/config"), Some(config))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.owner("DELETE", &path, None).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.owner("GET", &path, None).await.status,
        StatusCode::NOT_FOUND
    );
    stub.abort();
}

#[tokio::test]
async fn owner_routes_never_return_credentials_and_task_tokens_cannot_manage_accounts() {
    let h = recorder::harness(4).await;
    let bot = h.app.main_bot().await;
    let (_, token) = h.running(&bot, GATE).await;
    let account = account(&h.app).await;
    let path = format!("/v1/connectors/email/accounts/{}", account.id);
    let listed = h
        .app
        .owner("GET", "/v1/connectors/email/accounts", None)
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert!(!listed.body.to_string().contains("private-app-password"));
    assert!(!listed.body.to_string().contains("credential"));
    assert_eq!(
        h.app.call("GET", &path, None, Some(&token)).await.status,
        StatusCode::FORBIDDEN
    );
    let default_access = tool(&h.app, &token, "mail_accounts", json!({})).await;
    assert_eq!(
        default_access.result["accounts"][0]["id"],
        json!(account.id)
    );
    assert_eq!(
        default_access.result["accounts"][0]["permissions"],
        json!({
            "read":true,"write":true,"send":true,"wake":false
        })
    );
    for _ in 0..2 {
        let saved = h
            .app
            .owner(
                "PUT",
                &format!("{path}/bots/{bot}"),
                Some(serde_json::to_value(permissions()).unwrap()),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK);
    }
    let visible = tool(&h.app, &token, "mail_accounts", json!({})).await;
    assert!(!visible.error, "{}", visible.body);
    assert_eq!(visible.result["accounts"][0]["id"], json!(account.id));
    for _ in 0..2 {
        let revoked = h
            .app
            .owner("DELETE", &format!("{path}/bots/{bot}"), None)
            .await;
        assert_eq!(revoked.status, StatusCode::OK);
    }
    assert!(
        tool(&h.app, &token, "mail_accounts", json!({}))
            .await
            .result["accounts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn accounts_are_available_to_bots_created_later_without_waking_them() {
    let h = recorder::harness(4).await;
    let account = account(&h.app).await;
    let bot = h.app.create_bot("Mail helper").await;
    let (_, token) = h.running(&bot, GATE).await;
    let listed = tool(&h.app, &token, "mail_accounts", json!({})).await;
    assert_eq!(listed.result["accounts"][0]["id"], json!(account.id));
    assert_eq!(listed.result["accounts"][0]["permissions"]["wake"], false);
    assert!(
        !store::set_permissions(
            &h.app.db.pool,
            account.id,
            bot.parse().unwrap(),
            &Permissions {
                read: true,
                write: true,
                send: true,
                wake: false
            }
        )
        .await
        .unwrap()
    );
    assert!(
        store::grants(&h.app.db.pool, account.id)
            .await
            .unwrap()
            .is_empty()
    );
    store::set_permissions(
        &h.app.db.pool,
        account.id,
        bot.parse().unwrap(),
        &Permissions::default(),
    )
    .await
    .unwrap();
    assert!(
        tool(&h.app, &token, "mail_accounts", json!({}))
            .await
            .result["accounts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn introduction_leaves_busy_work_running_and_can_be_retried_when_idle() {
    let h = recorder::harness(4).await;
    let bot = h.app.main_bot().await;
    let account = account(&h.app).await;
    let (running, _) = h.running(&bot, GATE).await;
    let body = json!({"bot_id":bot,"request_id":Uuid::new_v4()});
    let path = format!("/v1/connectors/email/accounts/{}/introduction", account.id);
    let refused = h.app.owner("POST", &path, Some(body.clone())).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.body["error"]["code"], "bot_busy");
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let turn = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == running.to_string())
        .unwrap();
    assert_eq!(turn["status"], "running");
    assert!(
        !snapshot.body["items"]
            .to_string()
            .contains("[else.bot email setup]")
    );
    h.release(running).await;
    h.app.wait_turn(&running.to_string(), &["completed"]).await;
    let accepted = h.app.owner("POST", &path, Some(body.clone())).await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);
    assert_eq!(
        h.app.owner("POST", &path, Some(body)).await.body,
        accepted.body
    );
}

#[tokio::test]
async fn introduction_is_private_idempotent_and_reaches_the_model_and_history() {
    let h = recorder::harness(4).await;
    let bot = h.app.main_bot().await;
    let account = account(&h.app).await;
    let path = format!("/v1/connectors/email/accounts/{}/introduction", account.id);
    let request_id = Uuid::new_v4();
    let body = json!({"bot_id":bot,"request_id":request_id});
    let first = h.app.owner("POST", &path, Some(body.clone())).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let again = h.app.owner("POST", &path, Some(body)).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(first.body, again.body);
    let turn: Uuid = first.body["turn_id"].as_str().unwrap().parse().unwrap();
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
    let inputs = h.inputs(turn).await;
    assert_eq!(inputs.len(), 1);
    assert!(inputs[0].text.contains(&account.id.to_string()));
    assert!(inputs[0].text.contains("bot@example.com"));
    assert!(
        inputs[0]
            .text
            .contains("Ask the owner what this inbox is for")
    );
    assert!(inputs[0].text.contains("Do not inspect the inbox"));
    assert!(!inputs[0].text.contains("private-app-password"));
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    assert!(
        !snapshot.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "user")
    );
    let events = else_api::db::events::after(&h.app.db.pool, bot.parse().unwrap(), 0, 1000)
        .await
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|(_, kind, data)| kind == "item.upsert" && data["item"]["kind"] == "user")
    );
    let (sent_turn, token) = h.running(&bot, GATE).await;
    let later_inputs = h.inputs(sent_turn).await;
    assert!(
        later_inputs[0]
            .history
            .iter()
            .any(|entry| entry.text == inputs[0].text)
    );
    assert_eq!(
        h.app
            .call(
                "POST",
                &path,
                Some(json!({"bot_id":bot,"request_id":Uuid::new_v4()})),
                Some(&token)
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert!(
        store::grants(&h.app.db.pool, account.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        h.app
            .send(&bot, request_id, &inputs[0].text, None)
            .await
            .status,
        StatusCode::CONFLICT
    );
}

struct Transport {
    sends: AtomicUsize,
    uncertain: bool,
}

#[derive(Default)]
struct QuietInbox {
    scans: AtomicUsize,
    waits: AtomicUsize,
    ready: tokio::sync::Notify,
}
#[async_trait::async_trait]
impl MailTransport for QuietInbox {
    async fn execute(&self, _: &MailConfig, _: &Auth, _: &MailCommand) -> Result<Value, MailError> {
        Ok(json!({"folders":[]}))
    }
    async fn scan(&self, _: &MailConfig, _: &Auth, _: Option<&Cursor>) -> Result<Scan, MailError> {
        self.scans.fetch_add(1, Ordering::SeqCst);
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
        stop: CancellationToken,
    ) -> Result<(), MailError> {
        self.waits.fetch_add(1, Ordering::SeqCst);
        self.ready.notify_one();
        stop.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn quiet_watchers_survive_bot_commands_and_never_queue_empty_turns() {
    let h = recorder::harness(4).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let (turn, _) = h.running(&bot.to_string(), GATE).await;
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let transport = Arc::new(QuietInbox::default());
    let manager = Arc::new(ConnectorManager::with_transport(
        h.app.db.pool.clone(),
        h.app.state.secret_key.clone().unwrap(),
        h.app.scheduler.clone(),
        transport.clone(),
    ));
    let stop = CancellationToken::new();
    let running = tokio::spawn(manager.clone().run(stop.clone()));
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        transport.ready.notified(),
    )
    .await
    .unwrap();
    for _ in 0..3 {
        manager
            .execute(account.id, bot, turn, None, MailCommand::Folders)
            .await
            .unwrap();
    }
    assert_eq!(transport.scans.load(Ordering::SeqCst), 1);
    assert_eq!(transport.waits.load(Ordering::SeqCst), 1);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM connector_events")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(events, 0);
    stop.cancel();
    running.await.unwrap();
}
#[async_trait::async_trait]
impl MailTransport for Transport {
    async fn execute(&self, _: &MailConfig, _: &Auth, _: &MailCommand) -> Result<Value, MailError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        if self.uncertain {
            Err(MailError::new("connection_lost"))
        } else {
            Ok(json!({"sent":true}))
        }
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
        stop: CancellationToken,
    ) -> Result<(), MailError> {
        stop.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn duplicate_and_uncertain_send_retries_never_repeat_transport_effects() {
    for uncertain in [false, true] {
        let h = recorder::harness(4).await;
        let bot: Uuid = h.app.main_bot().await.parse().unwrap();
        let (turn, _) = h.running(&bot.to_string(), GATE).await;
        let account = account(&h.app).await;
        store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
            .await
            .unwrap();
        let transport = Arc::new(Transport {
            sends: AtomicUsize::new(0),
            uncertain,
        });
        let manager = Arc::new(ConnectorManager::with_transport(
            h.app.db.pool.clone(),
            h.app.state.secret_key.clone().unwrap(),
            h.app.scheduler.clone(),
            transport.clone(),
        ));
        let action = Uuid::new_v4();
        let command = MailCommand::Send {
            draft: Draft {
                to: vec!["owner@example.com".into()],
                cc: vec![],
                bcc: vec![],
                subject: "Report".into(),
                body: "Done".into(),
                attachments: vec![],
                reply_to: None,
            },
        };
        let first = manager
            .execute(account.id, bot, turn, Some(action), command.clone())
            .await
            .unwrap();
        let again = manager
            .execute(account.id, bot, turn, Some(action), command)
            .await
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(transport.sends.load(Ordering::SeqCst), 1);
        assert_eq!(
            first["status"],
            if uncertain { "uncertain" } else { "succeeded" }
        );
        let conflict = manager
            .execute(
                account.id,
                bot,
                turn,
                Some(action),
                MailCommand::CreateFolder {
                    folder: "other".into(),
                },
            )
            .await;
        assert!(conflict.is_err());
        store::set_permissions(&h.app.db.pool, account.id, bot, &Permissions::default())
            .await
            .unwrap();
        assert!(
            manager
                .execute(account.id, bot, turn, None, MailCommand::Folders)
                .await
                .is_err()
        );
        manager.forget(account.id).await;
    }
}

#[tokio::test]
async fn mail_event_origin_removes_computer_and_delegation_authority() {
    let transport = Arc::new(Transport {
        sends: AtomicUsize::new(0),
        uncertain: false,
    });
    let h = recorder::harness_with(4, Some(transport.clone())).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let initial = Scan {
        cursor: Cursor {
            uid_validity: 1,
            last_uid: 8,
        },
        arrivals: vec![],
        more: false,
    };
    store::record_scan(
        &h.app.db.pool,
        account.id,
        account.generation,
        None,
        &initial,
    )
    .await
    .unwrap();
    let arrival = Scan {
        cursor: Cursor {
            uid_validity: 1,
            last_uid: 9,
        },
        arrivals: vec![Arrival {
            message: MessageRef {
                folder: format!("INBOX {GATE}"),
                uid_validity: 1,
                uid: 9,
            },
        }],
        more: false,
    };
    store::record_scan(
        &h.app.db.pool,
        account.id,
        account.generation,
        Some(&initial.cursor),
        &arrival,
    )
    .await
    .unwrap();
    else_api::connectors::delivery::admit_pending(&h.app.db.pool, h.app.scheduler.registry(), 32)
        .await
        .unwrap();
    h.app.scheduler.wake();
    let turn: Uuid = sqlx::query_scalar(
        "SELECT turn_id FROM connector_deliveries WHERE turn_id IS NOT NULL LIMIT 1",
    )
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    let (_, token) = h.started(turn).await;
    let grant = h.app.scheduler.grants().resolve(&token).await.unwrap();
    assert!(grant.allows(Tool::Mail));
    for forbidden in [
        Tool::Computer,
        Tool::Delegate,
        Tool::CreateBot,
        Tool::RequestSecret,
        Tool::AskOwner,
        Tool::Routines,
        Tool::Mcp,
        Tool::ManageMcp,
    ] {
        assert!(!grant.allows(forbidden));
    }
    assert_eq!(grant.memory_scope, Some(vec![bot]));
    let send = json!({ "account_id": account.id, "action_id": Uuid::new_v4(),
                       "command": { "operation": "send", "draft": {
                           "to": ["owner@example.com"], "subject": "New mail", "body": "Summary" } } });
    let sent = tool(&h.app, &token, "mail_action", send).await;
    assert!(!sent.error, "{}", sent.body);
    assert_eq!(sent.result["status"], "succeeded", "{}", sent.body);
    assert_eq!(
        transport.sends.load(Ordering::SeqCst),
        1,
        "a standing send permission still sends"
    );
    for name in [
        "draft_email",
        "send_draft",
        "mcp_list",
        "mcp_install",
        "mcp_request",
    ] {
        let refused = tool(&h.app, &token, name, json!({})).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{name}: {}",
            refused.body
        );
    }
    let inputs = h.inputs(turn).await;
    assert_eq!(
        inputs[0].delegation.origin,
        else_api::app::providers::Origin::ConnectorEvent
    );
    let snapshot = else_api::db::conversation::snapshot(&h.app.db.pool, bot, 100, None)
        .await
        .unwrap()
        .unwrap();
    assert!(snapshot.items.iter().all(|item| item.kind != "connector"));
    let mut conn = h.app.db.pool.acquire().await.unwrap();
    let source: Uuid = sqlx::query_scalar("SELECT owner_item_id FROM turns WHERE id=?1")
        .bind(turn)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    else_api::db::events::item_upsert(&mut conn, bot, source)
        .await
        .unwrap();
    let visible: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bot_events WHERE kind='item.upsert' AND json_extract(data,'$.item.kind')='connector')").fetch_one(&mut *conn).await.unwrap();
    assert!(!visible);
    drop(conn);
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
}

#[tokio::test]
async fn encrypted_backup_preserves_credentials_and_restore_cannot_run_queued_mail() {
    use else_api::backup::{
        DatabaseParams,
        capture::BackupConfig,
        restore::{RestoreRequest, restore},
        worker::BackupWorker,
    };
    use std::time::Duration;

    for in_flight in [false, true] {
        let app = TestApp::installed().await;
        let bot: Uuid = app.main_bot().await.parse().unwrap();
        let _closed = app
            .scheduler
            .gate()
            .close(Duration::from_secs(2))
            .await
            .unwrap()
            .unwrap();
        let account = account(&app).await;
        store::set_permissions(&app.db.pool, account.id, bot, &permissions())
            .await
            .unwrap();
        let initial = Scan {
            cursor: Cursor {
                uid_validity: 1,
                last_uid: 0,
            },
            arrivals: vec![],
            more: false,
        };
        store::record_scan(&app.db.pool, account.id, account.generation, None, &initial)
            .await
            .unwrap();
        let arrival = Scan {
            cursor: Cursor {
                uid_validity: 1,
                last_uid: 1,
            },
            arrivals: vec![Arrival {
                message: MessageRef {
                    folder: "INBOX".into(),
                    uid_validity: 1,
                    uid: 1,
                },
            }],
            more: false,
        };
        store::record_scan(
            &app.db.pool,
            account.id,
            account.generation,
            Some(&initial.cursor),
            &arrival,
        )
        .await
        .unwrap();
        else_api::connectors::delivery::admit_pending(&app.db.pool, app.scheduler.registry(), 32)
            .await
            .unwrap();
        let mail_turn: Uuid = sqlx::query_scalar(
            "SELECT turn_id FROM connector_deliveries WHERE turn_id IS NOT NULL LIMIT 1",
        )
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        let action_id = Uuid::new_v4();
        if in_flight {
            sqlx::query("UPDATE turns SET status='running' WHERE id=?1")
                .bind(mail_turn)
                .execute(&app.db.pool)
                .await
                .unwrap();
            store::begin_action(
                &app.db.pool,
                account.id,
                account.generation,
                bot,
                mail_turn,
                action_id,
                &MailCommand::Archive {
                    message: arrival.arrivals[0].message.clone(),
                },
            )
            .await
            .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let params = DatabaseParams::parse(app.db.url.expose()).unwrap();
        let key = app.state.secret_key.clone().unwrap();
        let worker = BackupWorker::new(
            app.db.pool.clone(),
            params.clone(),
            key.clone(),
            BackupConfig {
                dir: dir.path().join("backups"),
                keep: 1,
                max_bytes: 1 << 28,
                interval: Duration::from_secs(3600),
            },
        );
        let set = worker.run_once().await.unwrap();
        let target = dir.path().join("restored.sqlite");
        restore(RestoreRequest {
            set_dir: &set.path,
            key: &key,
            server: &params,
            target_database: target.to_str().unwrap(),
            staging_parent: &dir.path().join("staging"),
            max_bytes: 1 << 28,
            computers: None,
        })
        .await
        .unwrap();
        let restored = sqlx::SqlitePool::connect(&format!("sqlite://{}", target.display()))
            .await
            .unwrap();
        let account = store::account(&restored, account.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(account.state, "paused");
        match store::credential(&restored, &key, account.id)
            .await
            .unwrap()
        {
            Credential::Password { ref password } => assert_eq!(password, "private-app-password"),
            _ => panic!("wrong credential kind"),
        }
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?1")
            .bind(mail_turn)
            .fetch_one(&restored)
            .await
            .unwrap();
        assert_eq!(
            status,
            if in_flight {
                "uncertain"
            } else {
                "interrupted"
            }
        );
        if in_flight {
            assert_eq!(
                store::action_status(&restored, account.id, bot, action_id)
                    .await
                    .unwrap()
                    .unwrap()["status"],
                "uncertain"
            );
            let uncertain: bool =
                sqlx::query_scalar("SELECT effect_uncertain FROM turns WHERE id=?1")
                    .bind(mail_turn)
                    .fetch_one(&restored)
                    .await
                    .unwrap();
            assert!(uncertain);
        }
        assert!(
            store::permissions(&restored, account.id, bot)
                .await
                .unwrap()
                .unwrap()
                .wake
        );
        assert_eq!(
            else_api::connectors::delivery::admit_pending(&restored, app.scheduler.registry(), 32)
                .await
                .unwrap(),
            0
        );
    }
}
