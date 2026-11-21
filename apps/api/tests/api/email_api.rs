use crate::common;
use else_api::db::turns::TurnStatus;

use axum::http::StatusCode;
use common::{
    TestApp,
    recorder::{self, GATE, GATE_EVERY_ATTEMPT, tool},
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

mod app_passwords;
mod oauth_sign_in_again;
mod reader;
mod removal;

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
        &app.state.secret_key,
        &config(),
        &Credential::Password {
            password: "private-app-password".into(),
        },
    )
    .await
    .unwrap()
}
/// Waits until the account's runtime reports `want`.
async fn health_becomes(app: &TestApp, account: Uuid, want: Health) {
    tokio::time::timeout(common::SETTLE, async {
        while store::account(&app.db.pool, account)
            .await
            .unwrap()
            .unwrap()
            .health
            != want
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the inbox never became {want:?}"));
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
    let h = recorder::harness(common::TestApp::builder()).await;
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
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let app = &h.app;
    let config = config();
    let rejected = app
        .owner(
            "POST",
            "/v1/connectors/email/accounts",
            Some(json!({"config":config,"credential":{"kind":"password","password":REVOKED}})),
        )
        .await;
    assert_eq!(
        rejected.status,
        StatusCode::BAD_REQUEST,
        "{}",
        rejected.body
    );
    assert_eq!(rejected.body["error"]["code"], "mail_authentication_failed");
    assert!(
        app.owner("GET", "/v1/connectors/email/accounts", None)
            .await
            .body["accounts"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a rejected password saves no account"
    );
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
    let replaced = app
        .owner(
            "PUT",
            &format!("{path}/credentials"),
            Some(json!({"kind":"password","password":REVOKED})),
        )
        .await;
    assert_eq!(
        replaced.status,
        StatusCode::BAD_REQUEST,
        "{}",
        replaced.body
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
}

#[tokio::test]
async fn owner_routes_never_return_credentials_and_task_tokens_cannot_manage_accounts() {
    let h = recorder::harness(common::TestApp::builder()).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_sets_a_mail_trigger_the_owner_approved_and_the_owner_pauses_or_deletes_it() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let account = account(&h.app).await;
    let bot = h.app.create_bot("Mail helper").await;
    let triggers = || async {
        h.app
            .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
            .await
            .body["mail_triggers"]
            .clone()
    };
    assert_eq!(
        triggers().await,
        json!([]),
        "a readable inbox that wakes nothing is no trigger"
    );
    let args =
        json!({"account_id": account.id, "instructions": " Flag invoices from new vendors. "});
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let parked = tool(&h.app, &token, "mail_trigger_set", args.clone()).await;
    assert_eq!(parked.result["parked"], true, "{}", parked.body);
    assert_eq!(
        triggers().await,
        json!([]),
        "nothing wakes the bot before the owner approves"
    );
    let question = parked.result["question_id"].as_str().unwrap().to_owned();
    let card = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["questions"].as_array().unwrap().clone())
        .find(|q| q["id"] == question)
        .unwrap();
    assert!(
        card["context"]
            .as_str()
            .unwrap()
            .contains("Flag invoices from new vendors."),
        "the card shows the exact instructions: {card}"
    );
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
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
    let set = tool(&h.app, &token, "mail_trigger_set", args).await;
    assert!(!set.error, "{}", set.body);
    assert_eq!(set.result["trigger"]["wake"], true, "{}", set.body);
    let listed = triggers().await;
    assert_eq!(listed[0]["account_id"], json!(account.id));
    assert_eq!(listed[0]["instructions"], "Flag invoices from new vendors.");
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;

    let wake = format!(
        "/v1/connectors/email/accounts/{}/bots/{bot}/wake",
        account.id
    );
    let off = h.app.owner("PUT", &wake, Some(json!({"wake":false}))).await;
    assert_eq!(off.status, StatusCode::OK, "{}", off.body);
    let paused = triggers().await;
    assert_eq!(paused[0]["wake"], false);
    assert_eq!(
        paused[0]["instructions"], "Flag invoices from new vendors.",
        "a paused trigger keeps its card"
    );
    let trigger = format!(
        "/v1/connectors/email/accounts/{}/bots/{bot}/trigger",
        account.id
    );
    let deleted = h.app.owner("DELETE", &trigger, None).await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(deleted.body["deleted"], true);
    assert_eq!(triggers().await, json!([]));
    let bot_id: Uuid = bot.parse().unwrap();
    let left = store::permissions(&h.app.db.pool, account.id, bot_id)
        .await
        .unwrap()
        .unwrap();
    assert!(left.read && !left.wake, "deleting a trigger keeps read");

    h.app.owner("PUT", &wake, Some(json!({"wake":true}))).await;
    assert_eq!(
        triggers().await[0]["instructions"],
        store::DEFAULT_WAKE_INSTRUCTIONS
    );
    h.app.owner("PUT", &wake, Some(json!({"wake":false}))).await;
    assert_eq!(
        triggers().await[0]["wake"],
        false,
        "a trigger turned on without instructions keeps its card when paused"
    );
    h.app
        .owner(
            "DELETE",
            &format!("/v1/connectors/email/accounts/{}/bots/{bot}", account.id),
            None,
        )
        .await;
    assert_eq!(
        triggers().await,
        json!([]),
        "an inbox the bot can't read is no trigger"
    );
    let (turn, token) = h.running(&bot, GATE).await;
    let refused = tool(
        &h.app,
        &token,
        "mail_trigger_set",
        json!({"account_id": account.id, "instructions": "Watch it."}),
    )
    .await;
    assert!(
        refused.error,
        "a trigger never restores read: {}",
        refused.body
    );
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_longest_trigger_instructions_fit_the_approval_card() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let account = account(&h.app).await;
    let bot = h.app.create_bot("Mail helper").await;
    let (turn, token) = h.running(&bot, GATE).await;
    let long = tool(
        &h.app,
        &token,
        "mail_trigger_set",
        json!({"account_id": account.id, "instructions": "x".repeat(3501)}),
    )
    .await;
    assert!(long.error, "{}", long.body);
    assert!(!h.app.state.grants.is_parked(turn).await);
    let fits = tool(
        &h.app,
        &token,
        "mail_trigger_set",
        json!({"account_id": account.id, "instructions": "x".repeat(3500)}),
    )
    .await;
    assert_eq!(fits.result["parked"], true, "{}", fits.body);
    h.release(turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_task_cannot_change_mail_triggers() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let account = account(&h.app).await;
    let main = h.app.main_bot().await;
    let (root, token) = h
        .running(&main, &format!("{GATE_EVERY_ATTEMPT} make a Mail bot"))
        .await;
    let created = tool(&h.app, &token, "create_bot", json!({ "name": "Mail" })).await;
    let mail = created.result["bot_id"].as_str().unwrap().to_owned();
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": mail, "task": "[gate] Watch the inbox." }),
    )
    .await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let child: Uuid = sqlx::query_scalar(
        "SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1 ORDER BY created_at DESC",
    )
    .bind(root)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    h.release(root).await;
    h.app.wait_turn(root, &["waiting"]).await;
    let (_, child_token) = h.started(child).await;
    for (name, args) in [
        (
            "mail_trigger_set",
            json!({"account_id": account.id, "instructions": "Watch it."}),
        ),
        ("mail_trigger_delete", json!({"account_id": account.id})),
    ] {
        let refused = tool(&h.app, &child_token, name, args).await;
        assert!(
            refused.error || refused.status == StatusCode::FORBIDDEN,
            "{name}: {}",
            refused.body
        );
        assert_ne!(refused.result["parked"], true, "{name} asks no card");
    }
    let mail_id: Uuid = mail.parse().unwrap();
    assert!(
        !store::permissions(&h.app.db.pool, account.id, mail_id)
            .await
            .unwrap()
            .unwrap()
            .wake
    );
    h.release(child).await;
}

#[tokio::test]
async fn accounts_are_available_to_bots_created_later_without_waking_them() {
    let h = recorder::harness(common::TestApp::builder()).await;
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
    let h = recorder::harness(common::TestApp::builder()).await;
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
    h.app.wait_turn(running, &["completed"]).await;
    let accepted = h.app.owner("POST", &path, Some(body.clone())).await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);
    assert_eq!(
        h.app.owner("POST", &path, Some(body)).await.body,
        accepted.body
    );
}

#[tokio::test]
async fn introduction_is_private_idempotent_and_reaches_the_model_and_history() {
    let h = recorder::harness(common::TestApp::builder()).await;
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
    h.app.wait_turn(turn, &["completed"]).await;
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
    /// Marks dispatching actions uncertain while a command is in flight, as
    /// startup recovery or a restore does.
    recovers: Option<sqlx::SqlitePool>,
}

/// A password QuietInbox's server rejects, as Apple rejects a revoked
/// app-specific password.
const REVOKED: &str = "revoked-app-password";

#[derive(Default)]
struct QuietInbox {
    scans: AtomicUsize,
    waits: AtomicUsize,
    ready: tokio::sync::Notify,
    /// Rejects every new sign-in, as after the owner revokes the password.
    /// A watch session that already signed in keeps waiting.
    revoked: std::sync::atomic::AtomicBool,
    /// Holds the next scan after it signs in, until `release_scan` or the
    /// watch stops.
    hold_scan: std::sync::atomic::AtomicBool,
    scan_held: tokio::sync::Notify,
    release_scan: tokio::sync::Notify,
}
impl QuietInbox {
    fn signs_in(&self, auth: &Auth) -> Result<(), MailError> {
        if auth.secret.as_str() == REVOKED || self.revoked.load(Ordering::SeqCst) {
            Err(MailError::new(MailErrorCode::MailAuthenticationFailed))
        } else {
            Ok(())
        }
    }
}
#[async_trait::async_trait]
impl MailTransport for QuietInbox {
    async fn verify(&self, _: &MailConfig, auth: &Auth) -> Result<(), MailError> {
        self.signs_in(auth)
    }
    async fn execute(
        &self,
        _: &MailConfig,
        auth: &Auth,
        _: &MailCommand,
    ) -> Result<Value, MailError> {
        self.signs_in(auth)?;
        Ok(json!({"folders":[]}))
    }
    async fn fetch(&self, _: &MailConfig, _: &Auth, _: &MessageRef) -> Result<Fetched, MailError> {
        Err(MailError::new(MailErrorCode::MessageNotFound))
    }
    async fn reader(&self, _: &MailConfig, _: &Auth) -> Result<Box<dyn MailReader>, MailError> {
        Err(MailError::new(MailErrorCode::MailTransportFailed))
    }
    async fn scan(
        &self,
        _: &MailConfig,
        auth: &Auth,
        _: Option<&Cursor>,
    ) -> Result<Scan, MailError> {
        self.signs_in(auth)?;
        if self.hold_scan.swap(false, Ordering::SeqCst) {
            self.scan_held.notify_one();
            self.release_scan.notified().await;
        }
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
    let transport = Arc::new(QuietInbox::default());
    let h = recorder::harness(common::TestApp::builder().mail(transport.clone())).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let (turn, _) = h.running(&bot.to_string(), GATE).await;
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let manager = h.app.state.connectors.clone();
    manager.changed(account.id).await;
    common::until("the mail watcher to be ready", transport.ready.notified()).await;
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
}
#[tokio::test]
async fn a_scan_that_began_before_a_rejection_never_reports_connected() {
    for already_rejected in [false, true] {
        scan_then_rejection(already_rejected).await;
    }
}

async fn scan_then_rejection(already_rejected: bool) {
    let inbox = Arc::new(QuietInbox::default());
    inbox.hold_scan.store(true, Ordering::SeqCst);
    let h = recorder::harness(common::TestApp::builder().mail(inbox.clone())).await;
    let account = account(&h.app).await;
    let reject = || async {
        store::set_health(
            &h.app.db.pool,
            account.id,
            account.generation,
            &Health::CredentialRejected,
        )
        .await
        .unwrap()
    };
    if already_rejected {
        assert!(reject().await);
    }
    let manager = h.app.state.connectors.clone();
    manager.changed(account.id).await;
    tokio::time::timeout(common::SETTLE, inbox.scan_held.notified())
        .await
        .expect("the watch signs in and scans");
    assert_eq!(reject().await, !already_rejected);
    inbox.release_scan.notify_one();
    tokio::time::timeout(common::SETTLE, inbox.ready.notified())
        .await
        .expect("the held scan finishes and the watch waits");
    assert_eq!(
        store::account(&h.app.db.pool, account.id)
            .await
            .unwrap()
            .unwrap()
            .health,
        Health::CredentialRejected,
        "a sign-in older than the rejection cannot clear it (already rejected: {already_rejected})"
    );

    manager.changed(account.id).await;
    health_becomes(&h.app, account.id, Health::Connected).await;
}

#[async_trait::async_trait]
impl MailTransport for Transport {
    async fn verify(&self, _: &MailConfig, _: &Auth) -> Result<(), MailError> {
        Ok(())
    }
    async fn execute(&self, _: &MailConfig, _: &Auth, _: &MailCommand) -> Result<Value, MailError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        if let Some(pool) = &self.recovers {
            store::recover(pool).await.unwrap();
        }
        if self.uncertain {
            Err(MailError::new(MailErrorCode::MailTransportFailed))
        } else {
            Ok(json!({"sent":true}))
        }
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
        stop: CancellationToken,
    ) -> Result<(), MailError> {
        stop.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn duplicate_and_uncertain_send_retries_never_repeat_transport_effects() {
    for uncertain in [false, true] {
        let h = recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default())))
            .await;
        let bot: Uuid = h.app.main_bot().await.parse().unwrap();
        let (turn, _) = h.running(&bot.to_string(), GATE).await;
        let account = account(&h.app).await;
        store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
            .await
            .unwrap();
        let transport = Arc::new(Transport {
            sends: AtomicUsize::new(0),
            uncertain,
            recovers: None,
        });
        let manager = Arc::new(ConnectorManager::with_transport(
            h.app.db.pool.clone(),
            h.app.state.secret_key.clone(),
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
                message_id: None,
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
async fn an_action_recovery_settled_first_reports_the_recorded_receipt() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let (turn, _) = h.running(&bot.to_string(), GATE).await;
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let manager = Arc::new(ConnectorManager::with_transport(
        h.app.db.pool.clone(),
        h.app.state.secret_key.clone(),
        h.app.scheduler.clone(),
        Arc::new(Transport {
            sends: AtomicUsize::new(0),
            uncertain: false,
            recovers: Some(h.app.db.pool.clone()),
        }),
    ));
    let action = Uuid::new_v4();
    let receipt = manager
        .execute(
            account.id,
            bot,
            turn,
            Some(action),
            MailCommand::CreateFolder {
                folder: "Receipts".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(receipt, json!({"action_id": action, "status": "uncertain"}));
    assert_eq!(
        store::ACTIONS
            .receipt(&h.app.db.pool, account.id, bot, action)
            .await
            .unwrap(),
        Some(receipt)
    );
    manager.forget(account.id).await;
}

#[tokio::test]
async fn store_refusals_reach_the_bot_under_their_own_codes() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let (turn, _) = h.running(&bot.to_string(), GATE).await;
    let account = account(&h.app).await;
    let pool = &h.app.db.pool;
    store::set_permissions(pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let manager = Arc::new(ConnectorManager::with_transport(
        pool.clone(),
        h.app.state.secret_key.clone(),
        h.app.scheduler.clone(),
        Arc::new(Transport {
            sends: AtomicUsize::new(0),
            uncertain: false,
            recovers: None,
        }),
    ));
    let folder = |name: &str| MailCommand::CreateFolder {
        folder: name.into(),
    };
    let code = |turn: Uuid, action: Option<Uuid>, command: MailCommand| {
        let manager = manager.clone();
        async move {
            manager
                .execute(account.id, bot, turn, action, command)
                .await
                .unwrap_err()
                .code
                .as_str()
        }
    };
    let action = Uuid::new_v4();
    manager
        .execute(account.id, bot, turn, Some(action), folder("One"))
        .await
        .unwrap();
    assert_eq!(
        code(turn, Some(action), folder("Two")).await,
        "mail_action_conflict"
    );
    store::set_active(pool, account.id, false).await.unwrap();
    assert_eq!(
        code(turn, None, MailCommand::Folders).await,
        "mail_account_paused"
    );
    store::set_active(pool, account.id, true).await.unwrap();
    store::set_permissions(pool, account.id, bot, &Permissions::default())
        .await
        .unwrap();
    assert_eq!(
        code(turn, None, MailCommand::Folders).await,
        "mail_forbidden"
    );
    store::set_permissions(pool, account.id, bot, &permissions())
        .await
        .unwrap();
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
    assert_eq!(
        code(turn, None, MailCommand::Folders).await,
        "mail_not_running"
    );
    manager.forget(account.id).await;
}

/// A mail-triggered turn for one arrival in `folder` of `account`, whose
/// trigger tells the bot `instructions`.
async fn mail_event(
    h: &recorder::Harness,
    account: &Account,
    folder: &str,
    instructions: &str,
) -> Uuid {
    let initial = Scan {
        cursor: Cursor {
            uid_validity: 1,
            last_uid: 8,
        },
        arrivals: vec![],
        more: false,
    };
    assert!(
        store::record_scan(
            &h.app.db.pool,
            account.id,
            account.generation,
            None,
            &initial,
        )
        .await
        .unwrap(),
        "no watch recorded a scan first"
    );
    // Before the arrival: the scheduler may admit it on its next scan.
    sqlx::query("UPDATE connector_grants SET wake_instructions = ?1")
        .bind(instructions)
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    let arrival = Scan {
        cursor: Cursor {
            uid_validity: 1,
            last_uid: 9,
        },
        arrivals: vec![Arrival {
            message: MessageRef {
                folder: folder.into(),
                uid_validity: 1,
                uid: 9,
            },
            envelope: Envelope {
                message_id: Some("fixture@coupons.test".into()),
                subject: Some("Verify your email".into()),
                sender: Some("Coupon Co".into()),
            },
        }],
        more: false,
    };
    assert!(
        store::record_scan(
            &h.app.db.pool,
            account.id,
            account.generation,
            Some(&initial.cursor),
            &arrival,
        )
        .await
        .unwrap()
    );
    else_api::connectors::delivery::admit_pending(&h.app.db.pool, &h.app.state.registry, 32)
        .await
        .unwrap();
    h.app.scheduler.wake();
    sqlx::query_scalar("SELECT turn_id FROM connector_deliveries WHERE turn_id IS NOT NULL LIMIT 1")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn mail_event_origin_removes_computer_authority() {
    let transport = Arc::new(Transport {
        sends: AtomicUsize::new(0),
        uncertain: false,
        recovers: None,
    });
    let h = recorder::harness(common::TestApp::builder().mail(transport.clone())).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let turn = mail_event(
        &h,
        &account,
        &format!("INBOX {GATE}"),
        "Summarize it for me.",
    )
    .await;
    let notice: String =
        sqlx::query_scalar("SELECT text FROM items WHERE turn_id = ?1 AND kind = 'connector'")
            .bind(turn)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert!(
        notice.contains("instructions for this trigger: Summarize it for me."),
        "{notice}"
    );
    let (_, token) = h.started(turn).await;
    let grant = h.app.state.grants.resolve(&token).await.unwrap();
    assert!(grant.allows(Tool::Mail));
    assert!(
        grant.allows(Tool::DraftEmail),
        "the owner reviews its drafts"
    );
    for forbidden in [
        Tool::Computer,
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
    let search = json!({ "account_id": account.id,
                         "command": { "operation": "search", "folder": "INBOX", "limit": 200 } });
    let wide = tool(&h.app, &token, "mail_action", search).await;
    assert!(
        wide.error
            && wide
                .body
                .to_string()
                .contains("invalid_mail_command: limit must be 1 to 100"),
        "{}",
        wide.body
    );
    for name in [
        "ask_owner",
        "confirm_draft_approval",
        "mail_trigger_set",
        "mail_trigger_delete",
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
    assert!(
        snapshot
            .items
            .iter()
            .all(|item| item.kind != else_api::db::turns::ItemKind::Connector)
    );
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
    h.app.wait_turn(turn, &["completed"]).await;
}

/// An inbox bot passes a notice on as its trigger says (issue #518). The
/// task runs on email content, so it sees only the task: never the other
/// bot's memory or mail, which the email could then have read back.
#[tokio::test]
async fn a_mail_event_hands_a_notice_to_another_bot_without_its_memory_or_mail() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let inbox: Uuid = h.app.main_bot().await.parse().unwrap();
    let marketing = h.app.create_bot("Marketing Manager").await;
    let marketing_id: Uuid = marketing.parse().unwrap();
    let finance = h.app.create_bot("Finance").await;
    let mut tx = h.app.db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::memory::append(
        &mut tx,
        marketing_id,
        &else_api::db::memory::NewEntry {
            request_key: "budget",
            kind: else_api::db::memory::Kind::OwnerMessage,
            body: "The ad budget for Q4 is 40k.",
            source: json!({}),
            supersedes_seq: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    else_api::memory_worker::drain(&h.app.db.pool)
        .await
        .unwrap();
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, inbox, &permissions())
        .await
        .unwrap();
    let turn = mail_event(
        &h,
        &account,
        &format!("INBOX {GATE_EVERY_ATTEMPT}"),
        "Pass X Ads emails to the Marketing Manager bot.",
    )
    .await;
    let (_, token) = h.started(turn).await;
    let registry = &h.inputs(turn).await[0].delegation.registry;
    assert!(
        registry.iter().any(|(id, _)| *id == marketing_id),
        "{registry:?}"
    );
    let with_files = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": marketing, "task": "Read the notes.",
                "title": "X Ads notes", "files": ["notes.md"] }),
    )
    .await;
    assert!(with_files.error, "{}", with_files.body);
    let council = tool(
        &h.app,
        &token,
        "council",
        json!({ "target_bot_ids": [marketing, finance], "task": "Review this ad.",
                "title": "X Ads review" }),
    )
    .await;
    assert!(
        council.error && council.body.to_string().contains("COUNCIL_REFUSED"),
        "{}",
        council.body
    );
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": marketing,
                "task": format!("{GATE} X Ads: your October invoice is ready."),
                "title": "X Ads invoice notice" }),
    )
    .await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(turn)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
    let (_, child_token) = h.started(child).await;
    let grant = h.app.state.grants.resolve(&child_token).await.unwrap();
    assert!(grant.allows(Tool::ReadHistory));
    for forbidden in [
        Tool::ReadMemory,
        Tool::Mail,
        Tool::DraftEmail,
        Tool::Delegate,
        Tool::Computer,
        Tool::Mcp,
    ] {
        assert!(!grant.allows(forbidden), "{forbidden:?}");
    }
    let input = &h.inputs(child).await[0];
    assert_eq!(
        input.delegation.origin,
        else_api::app::providers::Origin::ConnectorEvent
    );
    assert!(input.memory.is_none(), "{:?}", input.memory);
    let onward = tool(
        &h.app,
        &child_token,
        "delegate",
        json!({ "target_bot_id": finance, "task": "Pay the X Ads invoice.", "title": "Pay" }),
    )
    .await;
    assert!(
        onward.error && onward.body.to_string().contains("handed on from email"),
        "{}",
        onward.body
    );
    h.release(child).await;
    h.app.wait_turn(child, &["completed"]).await;
    h.continued(turn, &token).await;
    let inputs = h.inputs(turn).await;
    let outcomes = &inputs.last().unwrap().delegation.outcomes;
    assert!(
        outcomes.iter().any(|o| o.target_bot_id == marketing_id
            && o.status == else_api::db::delegations::Status::Completed
            && o.result.is_some()),
        "{outcomes:?}"
    );
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
}

#[tokio::test]
async fn a_mail_event_drafts_a_reply_the_owner_approves_in_a_card() {
    let transport = Arc::new(Transport {
        sends: AtomicUsize::new(0),
        uncertain: false,
        recovers: None,
    });
    let h = recorder::harness(common::TestApp::builder().mail(transport.clone())).await;
    let bot = h.app.main_bot().await;
    let account = account(&h.app).await;
    store::set_permissions(
        &h.app.db.pool,
        account.id,
        bot.parse().unwrap(),
        &permissions(),
    )
    .await
    .unwrap();
    let folder = format!("INBOX {GATE_EVERY_ATTEMPT}");
    let turn = mail_event(
        &h,
        &account,
        &folder,
        "Draft replies to support mail for my review.",
    )
    .await;
    let (_, token) = h.started(turn).await;

    let message = json!({ "folder": folder, "uid_validity": 1, "uid": 9 });
    for command in [
        json!({ "operation": "draft", "draft": { "to": ["customer@example.com"],
                "subject": "Re: Support", "body": "Thanks.", "reply_to": message } }),
        json!({ "operation": "reply", "message": message, "body": "Thanks." }),
        json!({ "operation": "reply", "message": message, "body": "Thanks.", "send": false }),
        json!({ "operation": "forward", "message": message, "to": ["team@example.com"],
                "body": "FYI" }),
    ] {
        let saved = tool(
            &h.app,
            &token,
            "mail_action",
            json!({ "account_id": account.id, "action_id": Uuid::new_v4(), "command": command }),
        )
        .await;
        assert!(
            saved.error && saved.body.to_string().contains("draft_email"),
            "a mailbox draft is refused toward the card: {}",
            saved.body
        );
    }
    assert_eq!(
        transport.sends.load(Ordering::SeqCst),
        0,
        "no mailbox draft was saved"
    );

    let reply = json!({ "account_id": account.id,
                        "reply_to": { "folder": folder, "uid_validity": 1, "uid": 9 },
                        "to": ["customer@example.com"], "subject": "Re: Support",
                        "body": "Thanks, we are looking into it." });
    let drafted = tool(&h.app, &token, "draft_email", reply).await;
    assert!(!drafted.error, "{}", drafted.body);
    assert_eq!(drafted.result["status"], "pending", "{}", drafted.body);
    let id = drafted.result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;

    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let card = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .expect("the mail turn is in the owner's chat")["outgoing_drafts"][0]
        .clone();
    assert_eq!(card["id"], id, "{card}");
    assert_eq!(card["status"], "pending");
    assert_eq!(card["content"]["from"], "bot@example.com");
    assert_eq!(
        transport.sends.load(Ordering::SeqCst),
        0,
        "nothing sent before approval"
    );

    let approved = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/outgoing-drafts/{id}/approve"),
            Some(json!({ "version": 1 })),
        )
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    for (name, args) in [
        ("ask_owner", json!({})),
        ("confirm_draft_approval", json!({ "draft_id": id })),
        ("draft_message", json!({ "platform": "x", "body": "Hi" })),
    ] {
        let refused = tool(&h.app, &token, name, args).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{name}: {}",
            refused.body
        );
    }
    let sent = tool(&h.app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    assert_eq!(transport.sends.load(Ordering::SeqCst), 1);
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
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
            .state
            .gate
            .close(common::SETTLE, else_api::maintenance::Pause::backup(None))
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
                envelope: Default::default(),
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
        else_api::connectors::delivery::admit_pending(&app.db.pool, &app.state.registry, 32)
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
            common::seed::force_status(&app.db.pool, mail_turn, TurnStatus::Running).await;
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
        let key = app.state.secret_key.clone();
        let worker = BackupWorker::new(
            app.db.pool.clone(),
            key.clone(),
            BackupConfig {
                dir: dir.path().join("backups"),
                keep: 1,
                max_bytes: 1 << 28,
                cadence: else_api::backup::capture::Cadence::Every(Duration::from_secs(3600)),
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
        assert_eq!(account.state, AccountState::Paused);
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
                store::ACTIONS
                    .receipt(&restored, account.id, bot, action_id)
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
            else_api::connectors::delivery::admit_pending(&restored, &app.state.registry, 32)
                .await
                .unwrap(),
            0
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_asks_the_owner_to_sign_in_to_an_inbox_from_chat() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h
        .running(
            &bot,
            &format!("{GATE_EVERY_ATTEMPT} Handle my Gmail inbox owner@gmail.com"),
        )
        .await;
    let unsupported = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"yahoo","address":"owner@yahoo.com"}),
    )
    .await;
    assert!(unsupported.error, "{}", unsupported.body);
    assert!(!app.state.grants.is_parked(turn).await);

    let parked = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"gmail","address":" Owner@Gmail.com ","purpose":"Triage your inbox."}),
    )
    .await;
    assert_eq!(
        parked.result["status"], "pending_owner_auth",
        "{}",
        parked.body
    );
    assert_eq!(parked.result["parked"], true);
    assert!(app.state.grants.is_parked(turn).await);
    let question: Uuid = parked.result["question_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let card = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["questions"].as_array().unwrap())
        .find(|q| q["id"] == question.to_string())
        .unwrap()
        .clone();
    assert_eq!(
        card["action"]["email_connection"],
        json!({"provider":"gmail","address":"owner@gmail.com"})
    );
    assert!(
        card["context"]
            .as_str()
            .unwrap()
            .contains("New mail there will wake this bot."),
        "the card says the inbox wakes the bot: {card}"
    );
    assert!(
        card["context"]
            .as_str()
            .unwrap()
            .contains("Triage your inbox.")
    );

    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let answer_path = format!("/v1/bots/{bot}/questions/{question}/answer");
    let approve = || app.owner("POST", &answer_path, Some(json!({"choice":"approve"})));
    let early = approve().await;
    assert_eq!(early.status, StatusCode::CONFLICT, "{}", early.body);
    assert_eq!(early.body["error"]["code"], "inbox_not_connected");
    let unrelated = account(app).await;
    let still_early = approve().await;
    assert_eq!(
        still_early.status,
        StatusCode::CONFLICT,
        "an IMAP inbox is not this sign-in"
    );
    let other_mailbox = store::create(
        &app.db.pool,
        &app.state.secret_key,
        &Provider::Gmail.config("other@gmail.com".into()).unwrap(),
        &Credential::OAuth2 {
            client_id: "desktop".into(),
            client_secret: None,
            refresh_token: "private-refresh".into(),
            access_token: None,
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let answered = approve().await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let bot_id: Uuid = bot.parse().unwrap();
    let woken = |account| store::permissions(&app.db.pool, account, bot_id);
    assert!(
        woken(other_mailbox.id).await.unwrap().unwrap().wake,
        "the signed-in inbox wakes the bot that asked for it"
    );
    assert!(!woken(unrelated.id).await.unwrap().unwrap().wake);
    let triggers = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["mail_triggers"]
        .clone();
    assert_eq!(
        triggers
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["address"].as_str().unwrap(),
                    t["wake"].as_bool().unwrap(),
                    t["instructions"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>(),
        [("other@gmail.com", true, store::DEFAULT_WAKE_INSTRUCTIONS)],
        "the sign-in card turns on a trigger with default instructions"
    );
    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.into_iter().last().unwrap();
    assert!(packet.contains("<email_connection"), "{packet}");
    assert!(packet.contains("Call mail_accounts"), "{packet}");
    let resumed = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"gmail","address":"owner@gmail.com"}),
    )
    .await;
    assert_eq!(resumed.result["status"], "connected", "{}", resumed.body);

    let known = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"microsoft","address":"BOT@example.com"}),
    )
    .await;
    assert_eq!(known.result["status"], "connected", "{}", known.body);
    assert_eq!(known.result["account_id"], unrelated.id.to_string());
    let chosen = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"gmail","address":"other@gmail.com"}),
    )
    .await;
    assert_eq!(chosen.result["account_id"], other_mailbox.id.to_string());
    h.release(turn).await;
}

fn gmail(address: &str) -> (MailConfig, Credential) {
    (
        Provider::Gmail.config(address.into()).unwrap(),
        Credential::OAuth2 {
            client_id: "desktop".into(),
            client_secret: None,
            refresh_token: "private-refresh".into(),
            access_token: None,
            expires_at: None,
        },
    )
}

/// Parks a turn on an inbox card for owner@gmail.com and returns the bot and
/// the card's answer path once the turn waits.
async fn inbox_card(h: &recorder::Harness) -> (String, String) {
    let bot = h.app.main_bot().await;
    let (turn, token) = h
        .running(&bot, &format!("{GATE} Watch owner@gmail.com"))
        .await;
    let parked = tool(
        &h.app,
        &token,
        "mail_connect",
        json!({"provider":"gmail","address":"owner@gmail.com"}),
    )
    .await;
    let question = parked.result["question_id"].as_str().unwrap().to_owned();
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
    let path = format!("/v1/bots/{bot}/questions/{question}/answer");
    (bot, path)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inbox_card_wakes_no_bot_when_its_inbox_is_ambiguous() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let (bot, path) = inbox_card(&h).await;
    let mut ids = vec![];
    for address in ["first@gmail.com", "second@gmail.com"] {
        let (config, credential) = gmail(address);
        let key = &h.app.state.secret_key;
        ids.push(
            store::create(&h.app.db.pool, key, &config, &credential)
                .await
                .unwrap()
                .id,
        );
    }
    let answered = h
        .app
        .owner("POST", &path, Some(json!({"choice":"approve"})))
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    for id in ids {
        let p = store::permissions(&h.app.db.pool, id, bot.parse().unwrap())
            .await
            .unwrap();
        assert!(
            !p.unwrap().wake,
            "neither inbox is surely the one signed in"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inbox_card_never_restores_read_the_owner_withdrew() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let (bot, path) = inbox_card(&h).await;
    let (config, credential) = gmail("owner@gmail.com");
    let key = &h.app.state.secret_key;
    let id = store::create(&h.app.db.pool, key, &config, &credential)
        .await
        .unwrap()
        .id;
    let bot_id: Uuid = bot.parse().unwrap();
    store::set_permissions(&h.app.db.pool, id, bot_id, &Permissions::default())
        .await
        .unwrap();
    let answered = h
        .app
        .owner("POST", &path, Some(json!({"choice":"approve"})))
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let p = store::permissions(&h.app.db.pool, id, bot_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!p.read && !p.wake, "{p:?}");
}
