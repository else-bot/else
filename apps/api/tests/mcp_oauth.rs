mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use axum::http::StatusCode;
use common::{Reply, TestApp};
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

struct Fixture {
    base: String,
    client: reqwest::Client,
    _child: tokio::process::Child,
}

impl Fixture {
    async fn start() -> Self {
        let mut child = tokio::process::Command::new("python3")
            .args([
                "-u",
                concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/mcp_oauth.py"),
            ])
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let base = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("Python fixture starts")
            .unwrap()
            .expect("Python fixture prints its URL");
        Self {
            base,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            _child: child,
        }
    }

    fn installation(&self) -> Value {
        json!({
            "id": "oauth-fixture", "name": "OAuth fixture", "description": "Local test MCP",
            "skill": "Discover tools/list, then use fixture_read.",
            "documentation": "This test MCP uses owner OAuth consent.",
            "config": {"type": "http", "url": format!("{}/mcp", self.base)}
        })
    }

    async fn control(&self, body: Value) {
        assert!(
            self.client
                .post(format!("{}/test", self.base))
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }

    async fn events(&self) -> Vec<Value> {
        self.client
            .get(format!("{}/test", self.base))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["events"]
            .as_array()
            .unwrap()
            .clone()
    }

    async fn browser_callback(&self, authorization: &Value, deny: bool) -> Value {
        let mut url =
            reqwest::Url::parse(authorization["authorization_url"].as_str().unwrap()).unwrap();
        if deny {
            url.query_pairs_mut().append_pair("deny", "true");
        }
        let response = self.client.get(url).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        let location = reqwest::Url::parse(
            response.headers()[reqwest::header::LOCATION]
                .to_str()
                .unwrap(),
        )
        .unwrap();
        let mut callback = json!({"request_id": authorization["request_id"]});
        for (key, value) in location.query_pairs() {
            callback[key.as_ref()] = json!(value);
        }
        callback
    }
}

async fn grant(app: &TestApp, bot: &str, slow: bool) -> (Uuid, String) {
    let text = if slow {
        "[synthetic:slow] Connect the fixture MCP so I can read its tools after I authorize it in my browser."
    } else {
        "[synthetic:hang] Read the fixture MCP."
    };
    let sent = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let turn: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    (turn, continuation_grant(app, bot, turn).await)
}

async fn continuation_grant(app: &TestApp, bot: &str, turn: Uuid) -> String {
    let conversation = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.scheduler
        .grants()
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![Tool::Mcp, Tool::ManageMcp, Tool::AskOwner],
            DEFAULT_TTL,
        )
        .await
}

async fn tool(app: &TestApp, token: &str, name: &str, arguments: Value) -> Reply {
    app.call("POST", "/tools/v1/mcp",
        Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})), Some(token)).await
}

fn result(reply: &Reply) -> Value {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_ne!(reply.body["result"]["isError"], true, "{}", reply.body);
    serde_json::from_str(reply.body["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

async fn list_tools(app: &TestApp, token: &str) -> Value {
    result(&tool(app, token, "mcp_request",
        json!({"id":"oauth-fixture","method":"tools/list","params":{},"purpose":"Read fixture tools"})).await)
}

fn connection_path(bot: &str) -> String {
    format!("/v1/bots/{bot}/mcp/oauth-fixture/connection")
}
fn oauth_path(bot: &str, action: &str) -> String {
    format!("/v1/bots/{bot}/mcp/oauth-fixture/oauth/{action}")
}

async fn authorize(app: &TestApp, bot: &str) -> Value {
    let response = app
        .owner(
            "POST",
            &oauth_path(bot, "start"),
            Some(json!({"redirect_uri":"http://127.0.0.1:49177/mcp/callback"})),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    response.body
}

async fn complete(app: &TestApp, bot: &str, callback: Value) -> Reply {
    app.owner("POST", &oauth_path(bot, "complete"), Some(callback))
        .await
}

async fn cancel(app: &TestApp, bot: &str, authorization: &Value) {
    let response = app
        .owner(
            "DELETE",
            &oauth_path(bot, authorization["request_id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::NO_CONTENT);
}

async fn connect(app: &TestApp, fixture: &Fixture, bot: &str) {
    let authorization = authorize(app, bot).await;
    let callback = fixture.browser_callback(&authorization, false).await;
    let response = complete(app, bot, callback).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
}

async fn owner_connection(app: &TestApp, bot: &str) -> Value {
    let response = app.owner("GET", &connection_path(bot), None).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    response.body
}

fn assert_private_files(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let bytes = std::fs::read(entry.path()).unwrap();
        for secret in ["fixture-access-", "fixture-refresh-", "fixture-client-"] {
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|part| part == secret.as_bytes()),
                "OAuth credentials appeared in plaintext at {}",
                entry.path().display()
            );
        }
        assert_eq!(
            entry.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_consent_resumes_a_parked_read_and_one_sign_in_serves_every_bot() {
    let fixture = Fixture::start().await;
    let db = std::sync::Arc::new(common::TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = std::sync::Arc::new(else_api::providers::synthetic::Synthetic::new(
        Duration::from_millis(5),
    ));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    let app = TestApp::with_registry(db, 4, registry);
    let bot = app.create_bot("OAuth owner").await;
    let other = app.create_bot("Other bot").await;
    let (turn, token) = grant(&app, &bot, true).await;
    result(&tool(&app, &token, "mcp_install", fixture.installation()).await);
    let initial = result(
        &tool(
            &app,
            &token,
            "mcp_connection_status",
            json!({"id":"oauth-fixture"}),
        )
        .await,
    );
    assert_eq!(initial["status"], "disconnected");
    let required = list_tools(&app, &token).await;
    assert_eq!(required["status"], "auth_required", "{required}");
    assert_eq!(required["mcp_id"], "oauth-fixture");
    assert_eq!(required["parked"], true);
    assert!(!required.to_string().contains("authorization_url"));
    assert!(!required.to_string().contains("code_verifier"));
    assert!(app.scheduler.grants().is_parked(turn).await);
    app.wait_turn(&turn.to_string(), &["waiting"]).await;
    let question = required["question_id"].as_str().unwrap();
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let card = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["id"] == turn.to_string())
        .unwrap()["questions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["id"] == question)
        .unwrap()
        .clone();
    assert!(card["action"].get("tool").is_none());
    assert_eq!(card["action"]["mcp_connection"]["id"], "oauth-fixture");
    assert_eq!(card["action"]["mcp_connection"]["provider"], fixture.base);
    assert_eq!(
        card["action"]["mcp_connection"]["scopes"],
        json!(["fixture.read"])
    );
    assert!(card["context"].as_str().unwrap().contains("fixture.read"));
    assert!(!card["context"].as_str().unwrap().contains("fixture.write"));
    connect(&app, &fixture, &bot).await;
    let answered = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({"choice":"approve"})),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    assert_eq!(answered.body["requeued"], true);
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let packet = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(packet) = synthetic
                .packets()
                .await
                .into_iter()
                .find(|p| p.contains("Account sign-in completed."))
            {
                break packet;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the resumed provider receives the connection outcome");
    assert!(packet.contains("Retry the original read using mcp_request."));
    assert!(!packet.contains("Approved: call mcp_connect"));
    let resumed = continuation_grant(&app, &bot, turn).await;
    let tools = list_tools(&app, &resumed).await;
    assert_eq!(tools["result"]["tools"][0]["name"], "fixture_read");
    let attempt: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id=?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(attempt >= 2);
    assert_eq!(owner_connection(&app, &bot).await["status"], "connected");
    assert_eq!(owner_connection(&app, &other).await["status"], "connected");
    let events = fixture.events().await;
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "authorize" && event["scope"] == "fixture.read")
    );
    assert!(
        events
            .iter()
            .filter(|event| matches!(
                event["kind"].as_str(),
                Some("discovery" | "register" | "token")
            ))
            .all(|event| event["authorization"] == false)
    );
    let root = Path::new(app.db.url.expose().strip_prefix("sqlite://").unwrap())
        .parent()
        .unwrap()
        .join("mcp/oauth-fixture");
    assert_private_files(&root);
    let shared = root.join(format!("oauth-{}.json", Uuid::nil()));
    assert!(shared.is_file());
    assert!(!root.join(format!("oauth-{bot}.json")).exists());
    let app = app.restart().await;
    assert_eq!(owner_connection(&app, &bot).await["status"], "connected");
    let (_, token) = grant(&app, &bot, false).await;
    assert_eq!(
        list_tools(&app, &token).await["result"]["tools"][0]["name"],
        "fixture_read"
    );
    let (_, other_token) = grant(&app, &other, true).await;
    let other_connected = result(
        &tool(
            &app,
            &other_token,
            "mcp_connect",
            json!({"id":"oauth-fixture","purpose":"Read fixture tools for this bot"}),
        )
        .await,
    );
    assert_eq!(other_connected["status"], "connected", "{other_connected}");
    let disconnected = app.owner("DELETE", &connection_path(&other), None).await;
    assert_eq!(disconnected.status, StatusCode::NO_CONTENT);
    assert!(!shared.exists());
    for bot in [&bot, &other] {
        assert_eq!(owner_connection(&app, bot).await["status"], "disconnected");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn callback_state_issuer_replay_and_owner_auth_are_enforced() {
    let fixture = Fixture::start().await;
    let app = TestApp::installed().await;
    let bot = app.create_bot("Callback checks").await;
    let (_, token) = grant(&app, &bot, false).await;
    result(&tool(&app, &token, "mcp_install", fixture.installation()).await);
    let denied = app
        .call(
            "POST",
            &oauth_path(&bot, "start"),
            Some(json!({"redirect_uri":"http://127.0.0.1:49177/mcp/callback"})),
            Some(&token),
        )
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    for invalid in [
        "http://public.example/callback",
        "https://example.com/callback#fragment",
    ] {
        let response = app
            .owner(
                "POST",
                &oauth_path(&bot, "start"),
                Some(json!({"redirect_uri":invalid})),
            )
            .await;
        assert_eq!(
            response.status,
            StatusCode::BAD_REQUEST,
            "{}",
            response.body
        );
    }
    let authorization = authorize(&app, &bot).await;
    let mut wrong = fixture.browser_callback(&authorization, false).await;
    wrong["state"] = json!("forged-state");
    let response = complete(&app, &bot, wrong).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
    assert_ne!(owner_connection(&app, &bot).await["status"], "connected");
    cancel(&app, &bot, &authorization).await;
    let authorization = authorize(&app, &bot).await;
    let mut wrong = fixture.browser_callback(&authorization, false).await;
    wrong["iss"] = json!("https://wrong-issuer.example");
    let response = complete(&app, &bot, wrong).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event["kind"] == "token")
    );
    cancel(&app, &bot, &authorization).await;
    let authorization = authorize(&app, &bot).await;
    let callback = fixture.browser_callback(&authorization, false).await;
    let response = complete(&app, &bot, callback.clone()).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let cancellation = app
        .owner("DELETE", oauth_path(&bot, "").trim_end_matches('/'), None)
        .await;
    assert_eq!(cancellation.status, StatusCode::OK, "{}", cancellation.body);
    assert_eq!(
        cancellation.body["status"], "connected",
        "cancelling a pending flow cannot revoke a completed account"
    );
    let replay = complete(&app, &bot, callback).await;
    assert_eq!(replay.status, StatusCode::BAD_REQUEST, "{}", replay.body);
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event["kind"] == "token")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_access_refreshes_once_and_revoked_consent_requires_owner_auth() {
    let fixture = Fixture::start().await;
    let app = TestApp::installed().await;
    let bot = app.create_bot("Refresh checks").await;
    let (_, token) = grant(&app, &bot, false).await;
    result(&tool(&app, &token, "mcp_install", fixture.installation()).await);
    fixture.control(json!({"next_ttl":1})).await;
    connect(&app, &fixture, &bot).await;
    fixture.control(json!({"next_ttl":3600})).await;
    let before = fixture.events().await;
    assert_eq!(
        list_tools(&app, &token).await["result"]["tools"][0]["name"],
        "fixture_read"
    );
    let proactive = fixture.events().await;
    assert_eq!(
        proactive
            .iter()
            .filter(|event| event["kind"] == "token" && event["grant_type"] == "refresh_token")
            .count(),
        1
    );
    assert_eq!(
        proactive
            .iter()
            .filter(|event| event["kind"] == "unauthorized")
            .count(),
        before
            .iter()
            .filter(|event| event["kind"] == "unauthorized")
            .count()
    );
    fixture.control(json!({"expire_access":true})).await;
    assert_eq!(
        list_tools(&app, &token).await["result"]["tools"][0]["name"],
        "fixture_read"
    );
    assert_eq!(
        fixture
            .events()
            .await
            .iter()
            .filter(|event| event["kind"] == "token" && event["grant_type"] == "refresh_token")
            .count(),
        2
    );
    fixture
        .control(json!({"expire_access":true,"revoked":true}))
        .await;
    let required = list_tools(&app, &token).await;
    assert_eq!(required["status"], "auth_required");
    assert_eq!(
        owner_connection(&app, &bot).await["status"],
        "reauth_required"
    );
    assert!(!required.to_string().contains("fixture-refresh-"));
    assert!(!required.to_string().contains("fixture-access-"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelled_consent_never_connects_or_exchanges_a_code() {
    let fixture = Fixture::start().await;
    let app = TestApp::installed().await;
    let bot = app.create_bot("Cancelled consent").await;
    let (_, token) = grant(&app, &bot, false).await;
    result(&tool(&app, &token, "mcp_install", fixture.installation()).await);
    let abandoned = authorize(&app, &bot).await;
    let abandoned_callback = fixture.browser_callback(&abandoned, false).await;
    let cleared = app
        .owner("DELETE", oauth_path(&bot, "").trim_end_matches('/'), None)
        .await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.body);
    assert_eq!(cleared.body["status"], "disconnected");
    assert_eq!(
        complete(&app, &bot, abandoned_callback).await.status,
        StatusCode::BAD_REQUEST
    );
    let authorization = authorize(&app, &bot).await;
    let callback = fixture.browser_callback(&authorization, true).await;
    let response = complete(&app, &bot, callback).await;
    assert!(response.status.is_client_error(), "{}", response.body);
    assert_ne!(owner_connection(&app, &bot).await["status"], "connected");
    assert!(
        !fixture
            .events()
            .await
            .iter()
            .any(|event| event["kind"] == "token")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unauthorized_mutation_is_not_replayed_after_token_refresh() {
    let fixture = Fixture::start().await;
    let app = TestApp::installed().await;
    let bot = app.create_bot("Mutation checks").await;
    let (_, token) = grant(&app, &bot, true).await;
    result(&tool(&app, &token, "mcp_install", fixture.installation()).await);
    connect(&app, &fixture, &bot).await;
    let arguments = json!({
        "id":"oauth-fixture", "method":"tools/call",
        "params":{"name":"fixture_read","arguments":{}}, "effect":"local"
    });
    let first = result(&tool(&app, &token, "mcp_request", arguments.clone()).await);
    assert_eq!(first["result"]["content"][0]["text"], "fixture-value");
    fixture.control(json!({"expire_access":true})).await;
    let second = result(&tool(&app, &token, "mcp_request", arguments).await);
    assert_eq!(second["status"], "auth_required");
    let events = fixture.events().await;
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "mcp" && event["method"] == "tools/call")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "unauthorized" && event["method"] == "tools/call")
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| event["kind"] == "token" && event["grant_type"] == "refresh_token")
    );
}
