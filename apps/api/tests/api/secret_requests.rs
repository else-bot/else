use crate::common;

use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, Harness, harness, tool};
use else_api::grants::Tool;
use serde_json::{Value, json};
use uuid::Uuid;

const CANARY: &str = "CANARY-7f3a9c";
const PEM: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nCANARY-7f3a9c-line-one+/=\nsecond line\twith tab\n-----END OPENSSH PRIVATE KEY-----\n";
fn deploy_args() -> Value {
    json!({ "name": "deploy", "reason": "Log in to the web server to deploy the site.",
            "env": ["DEPLOY_USER", "DEPLOY_KEY"] })
}

async fn request_id(app: &TestApp, token: &str) -> Uuid {
    let reply = tool(app, token, "request_secret", deploy_args()).await;
    let result = reply.result;
    assert_eq!(result["status"], "pending", "{}", reply.body);
    result["request_id"].as_str().unwrap().parse().unwrap()
}

fn values() -> Value {
    json!({ "env": { "DEPLOY_USER": "deployer", "DEPLOY_KEY": PEM } })
}

async fn fulfill(app: &TestApp, bot: &str, request: Uuid, body: Value) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/secret-requests/{request}/fulfill"),
        Some(body),
    )
    .await
}

async fn cancel(app: &TestApp, bot: &str, request: Uuid) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/secret-requests/{request}/cancel"),
        None,
    )
    .await
}

async fn request_status(app: &TestApp, request: Uuid) -> (String, Option<i32>) {
    sqlx::query_as("SELECT status, delivered_attempt FROM secret_requests WHERE id = ?1")
        .bind(request)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

/// Waits until `turn` parks again after attempt `attempt`.
async fn parks_after(app: &TestApp, turn: Uuid, attempt: i32) {
    let deadline = std::time::Instant::now() + common::SETTLE;
    loop {
        let now: (i32, String) = sqlx::query_as("SELECT attempt, status FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
        if now.0 >= attempt && now.1 == "waiting" {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{turn}: {now:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn sidebar_status(app: &TestApp, bot: &str) -> Value {
    let list = app.owner("GET", "/v1/bots", None).await;
    list.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == bot)
        .unwrap()["sidebar_status"]
        .clone()
}

/// The owner typed instead of filling the card: the request closes as
/// skipped and the bot leaves Needs you at once, the reply continues with
/// the message and learns the request was skipped, and a late save is
/// refused without storing anything.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_skips_a_waiting_secret_request() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let request = request_id(app, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_ne!(sidebar_status(app, &main).await, Value::Null);

    let correction = "I don't know the password, check for a password reset instead.";
    let sent = app
        .send(&main, Uuid::new_v4(), correction, Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], turn.to_string());
    assert_eq!(request_status(app, request).await.0, "skipped");
    assert_eq!(
        shown_turn(app, &main, turn).await["secret_requests"][0]["status"],
        "skipped"
    );
    assert_eq!(sidebar_status(app, &main).await, Value::Null);

    let late = fulfill(app, &main, request, values()).await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert_eq!(late.body["error"]["code"], "secret_request_settled");
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM task_secrets")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(stored, 0);
    assert_canary_absent_from_database(app).await;

    app.wait_turn(turn, &["completed"]).await;
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    assert_eq!(attempts(app, turn).await, 2);
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains(correction), "{packet}");
    assert!(
        packet.contains(r#"<secret name="deploy" env="DEPLOY_KEY,DEPLOY_USER" status="skipped"/>"#),
        "{packet}"
    );
}

/// The owner typed while the attempt that asked still streamed: the request
/// closes at once, and the reply continues with the message when that
/// attempt ends instead of waiting on the card.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_skips_a_request_while_the_asking_attempt_runs() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let request = request_id(app, &token).await;
    assert_eq!(
        status(app, turn).await,
        "running",
        "the attempt still streams"
    );

    let correction = "Skip the login, deploy from the CI artifacts instead.";
    let sent = app
        .send(&main, Uuid::new_v4(), correction, Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], turn.to_string());
    assert_eq!(request_status(app, request).await.0, "skipped");

    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains(correction), "{packet}");
    assert!(packet.contains(r#"status="skipped""#), "{packet}");
}

async fn status(app: &TestApp, turn: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn attempts(app: &TestApp, turn: Uuid) -> i32 {
    sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn assert_canary_absent_from_database(app: &TestApp) {
    let mut conn = app.db.pool.acquire().await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    for table in tables {
        let rows = common::rows(&mut conn, &format!("SELECT * FROM \"{table}\"")).await;
        for row in rows {
            assert!(!row.contains(CANARY), "{table} holds the value: {row}");
            assert!(
                !row.contains(&hex::encode(CANARY)),
                "{table} holds the value as bytes"
            );
        }
    }
}

async fn resolved(app: &TestApp, name: &str) -> Vec<(String, String)> {
    let env = else_api::db::task_secrets::resolve(
        &app.db.pool,
        Some(&*app.state.secret_key),
        &[name.to_owned()],
    )
    .await
    .unwrap();
    env.vars
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().to_owned()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_after_the_attempt_ends_stores_the_secret_and_continues_the_turn() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let request = request_id(app, &token).await;
    let again = tool(app, &token, "request_secret", deploy_args()).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);

    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await;
    let waiting = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .expect("a waiting turn is in the snapshot")
        .clone();
    let shown = &waiting["secret_requests"][0];
    let listed = app.owner("GET", &format!("/v1/bots/{main}"), None).await;
    assert_eq!(
        {
            let mut status = listed.body["bot"]["sidebar_status"].clone();
            assert!(
                status
                    .as_object_mut()
                    .unwrap()
                    .remove("since")
                    .unwrap()
                    .is_string()
            );
            status
        },
        json!({ "kind": "needs_credentials", "preview": "Log in to the web server to deploy the site." })
    );
    assert_eq!(shown["id"], request.to_string());
    assert_eq!(shown["status"], "pending");
    assert_eq!(shown["name"], "deploy");
    assert_eq!(shown["env"], json!(["DEPLOY_KEY", "DEPLOY_USER"]));
    assert_eq!(
        shown["reason"],
        "Log in to the web server to deploy the site."
    );

    let stored = fulfill(app, &main, request, values()).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(stored.body["request"]["status"], "stored");
    assert_eq!(stored.body["requeued"], true);
    assert!(!stored.body.to_string().contains(CANARY));
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(attempts(app, turn).await, 2, "the same turn continued");
    let listed = app.owner("GET", &format!("/v1/bots/{main}"), None).await;
    assert_eq!(listed.body["bot"]["sidebar_status"], Value::Null);
    assert_eq!(
        request_status(app, request).await,
        ("stored".into(), Some(2))
    );

    let mut env = resolved(app, "deploy").await;
    env.sort();
    assert_eq!(
        env,
        vec![
            ("DEPLOY_KEY".to_owned(), PEM.to_owned()),
            ("DEPLOY_USER".to_owned(), "deployer".to_owned()),
        ]
    );
    let listed = app.owner("GET", "/v1/task-secrets", None).await;
    assert_eq!(listed.body["secrets"][0]["name"], "deploy");
    assert!(!listed.body.to_string().contains(CANARY));

    let replay = fulfill(
        app,
        &main,
        request,
        json!({ "env": { "DEPLOY_USER": "other", "DEPLOY_KEY": "other-key" } }),
    )
    .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body["requeued"], false);
    assert_eq!(resolved(app, "deploy").await.len(), 2);
    assert!(
        resolved(app, "deploy")
            .await
            .contains(&("DEPLOY_KEY".to_owned(), PEM.to_owned()))
    );
    assert_eq!(
        cancel(app, &main, request).await.status,
        StatusCode::CONFLICT
    );

    for packet in h.packets().await {
        assert!(!packet.contains(CANARY), "a provider packet held the value");
    }
    let events = app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await;
    assert!(!events.body.to_string().contains(CANARY));
    assert_canary_absent_from_database(app).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_before_the_attempt_ends_queues_the_continuation_directly() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let request = request_id(app, &token).await;
    let stored = fulfill(app, &main, request, values()).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(
        stored.body["requeued"], false,
        "the attempt is still running"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(attempts(app, turn).await, 2);
    assert_eq!(
        request_status(app, request).await,
        ("stored".into(), Some(2))
    );
    assert_canary_absent_from_database(app).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_request_continues_the_turn_without_a_secret() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let request = request_id(app, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let declined = cancel(app, &main, request).await;
    assert_eq!(declined.status, StatusCode::OK, "{}", declined.body);
    assert_eq!(declined.body["request"]["status"], "cancelled");
    assert_eq!(
        cancel(app, &main, request).await.status,
        StatusCode::OK,
        "retry"
    );
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(
        request_status(app, request).await,
        ("cancelled".into(), Some(2))
    );
    let late = fulfill(app, &main, request, values()).await;
    assert_eq!(late.status, StatusCode::CONFLICT);
    assert!(!late.body.to_string().contains(CANARY));
    let listed = app.owner("GET", "/v1/task-secrets", None).await;
    assert_eq!(listed.body["secrets"], json!([]));

    let (next, token) = h.running(&main, GATE).await;
    assert_ne!(next, turn);
    let fresh = tool(app, &token, "request_secret", deploy_args())
        .await
        .result;
    assert_eq!(fresh["status"], "pending", "a new request may ask again");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_withdraws_the_request_before_the_provider_finishes_draining() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h
        .running(
            &main,
            "[synthetic:ignore-cancel] wait for deployment credentials",
        )
        .await;
    let request = request_id(app, &token).await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success());
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelling", "exercise the provider drain interval");
    assert_eq!(
        fulfill(app, &main, request, values()).await.status,
        StatusCode::CONFLICT,
        "Stop must withdraw the form before the provider acknowledges it"
    );
    let listed = app.owner("GET", "/v1/task-secrets", None).await;
    assert_eq!(listed.body["secrets"], json!([]));
    app.wait_turn(turn, &["interrupted", "uncertain"]).await;
    assert_canary_absent_from_database(app).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_delete_and_restart_settle_waiting_requests() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;

    let (turn, token) = h.running(&main, GATE).await;
    let request = request_id(app, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    app.wait_turn(turn, &["interrupted"]).await;
    assert_eq!(request_status(app, request).await.0, "cancelled");
    assert_eq!(
        fulfill(app, &main, request, values()).await.status,
        StatusCode::CONFLICT
    );

    let (turn, token) = h.running(&main, GATE).await;
    let request = request_id(app, &token).await;
    app.owner(
        "POST",
        &format!("/v1/bots/{main}/cancel"),
        Some(json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    app.wait_turn(turn, &["interrupted"]).await;
    assert_eq!(request_status(app, request).await.0, "cancelled");

    let (turn, token) = h.running(&main, GATE).await;
    let request = request_id(app, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let h = harness(TestApp::builder().db(h.app.stop().await)).await;
    let app = &h.app;
    assert_eq!(app.wait_turn(turn, &["waiting"]).await, "waiting");
    assert_eq!(request_status(app, request).await.0, "pending");
    let stored = fulfill(app, &main, request, values()).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(
        resolved(app, "deploy").await.len(),
        2,
        "stored after restart"
    );

    let other = app.create_bot("Ops").await;
    let (turn, token) = h.running(&other, GATE).await;
    let request = request_id(app, &token).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let deleted = app
        .owner("DELETE", &format!("/v1/bots/{other}"), None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    app.wait_turn(turn, &["interrupted"]).await;
    assert_eq!(request_status(app, request).await.0, "cancelled");
    assert_eq!(
        fulfill(app, &other, request, values()).await.status,
        StatusCode::NOT_FOUND
    );
    assert_canary_absent_from_database(app).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_answers_and_only_for_the_requesting_bot() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let other = app.create_bot("Home").await;
    let (turn, token) = h.running(&main, GATE).await;
    let bare = h.grant_with(turn, vec![Tool::ReadMemory]).await;
    let refused = tool(app, &bare, "request_secret", deploy_args()).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    let request = request_id(app, &token).await;
    let path = format!("/v1/bots/{main}/secret-requests/{request}/fulfill");

    for bearer in [Some(token.as_str()), None, Some("wrong")] {
        let reply = app.call("POST", &path, Some(values()), bearer).await;
        assert!(
            matches!(
                reply.status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ),
            "{bearer:?}: {}",
            reply.status
        );
        assert!(!reply.body.to_string().contains(CANARY));
    }
    for reply in [
        fulfill(app, &other, request, values()).await,
        cancel(app, &other, request).await,
        fulfill(app, &main, Uuid::new_v4(), values()).await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }

    let bad_bodies = [
        json!({ "env": { "DEPLOY_USER": "u" , "DEPLOY_KEY": PEM, "PATH": CANARY } }),
        json!({ "env": { "DEPLOY_USER": "u", "DEPLOY_KEY": PEM }, "website": CANARY }),
        json!({ "env": { "DEPLOY_KEY": PEM } }),
        json!({ "env": { "DEPLOY_USER": CANARY, "DEPLOY_KEY": 7 } }),
        json!({ "env": { "DEPLOY_USER": "u", "DEPLOY_KEY": PEM }, "note": CANARY }),
        json!({ "env": [CANARY] }),
        json!({ "env": { "DEPLOY_USER": "u", "DEPLOY_KEY": format!("{CANARY}\u{0}") } }),
    ];
    for body in bad_bodies {
        let reply = fulfill(app, &main, request, body.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert!(!reply.body.to_string().contains(CANARY), "{}", reply.body);
    }
    let raw = raw_post(
        app,
        &path,
        format!("{{\"env\": {{\"DEPLOY_KEY\": \"{CANARY}"),
    )
    .await;
    assert_eq!(raw.0, StatusCode::BAD_REQUEST);
    assert!(!raw.1.contains(CANARY), "{}", raw.1);
    assert_eq!(request_status(app, request).await.0, "pending");

    assert_canary_absent_from_database(app).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_login_request_saves_the_owners_fields_and_website_in_the_vault() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (_, token) = h.running(&main, GATE).await;
    let asked = tool(
        app,
        &token,
        "request_secret",
        json!({ "name": "x-login", "reason": "Post the weekly thread on X.",
                "env": ["USERNAME", "PASSWORD"], "description": "X account",
                "website": "https://www.X.com/i/flow/login" }),
    )
    .await;
    let asked = asked.result;
    assert_eq!(asked["status"], "pending", "{asked}");
    let request: Uuid = asked["request_id"].as_str().unwrap().parse().unwrap();
    let turn = app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await;
    assert!(
        turn.body.to_string().contains("\"website\":\"x.com\""),
        "the card shows the website: {}",
        turn.body
    );

    let missing = fulfill(
        app,
        &main,
        request,
        json!({ "env": { "USERNAME": "me@example.com" } }),
    )
    .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST, "every asked field");
    let saved = fulfill(
        app,
        &main,
        request,
        json!({ "env": { "USERNAME": "me@example.com", "PASSWORD": CANARY,
                         "TOTP_SECRET": "JBSWY3DPEHPK3PXP" },
                "description": "X, @example" }),
    )
    .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    let vault = app.owner("GET", "/v1/task-secrets", None).await;
    let entry = &vault.body["secrets"][0];
    assert_eq!(entry["name"], "x-login");
    assert_eq!(entry["description"], "X, @example", "the owner's edit wins");
    assert_eq!(entry["website"], "x.com");
    assert_eq!(entry["env"], json!(["PASSWORD", "TOTP_SECRET", "USERNAME"]));
    assert_eq!(entry["last_used"], Value::Null);
    assert!(!vault.body.to_string().contains(CANARY));
}

#[tokio::test(flavor = "multi_thread")]
async fn arguments_carry_metadata_only() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (_, token) = h.running(&main, GATE).await;
    for args in [
        json!({ "name": "deploy", "reason": "r", "env": ["DEPLOY_KEY"], "value": CANARY }),
        json!({ "name": "deploy", "reason": "r", "env": { "DEPLOY_KEY": CANARY } }),
        json!({ "name": "deploy", "reason": "r", "env": [CANARY] }),
        json!({ "name": "deploy", "reason": "r", "env": ["DEPLOY_KEY"], "website": CANARY }),
        json!({ "name": "deploy", "reason": "r", "env": ["DEPLOY_KEY"], "description": format!("a\n{CANARY}") }),
    ] {
        let reply = tool(app, &token, "request_secret", args).await;
        assert_eq!(reply.body["result"]["isError"], true, "{}", reply.body);
        assert!(!reply.body.to_string().contains(CANARY));
    }
    assert_canary_absent_from_database(app).await;
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM secret_requests")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(none, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_delegated_for_an_owner_request_asks_for_a_secret_in_its_own_chat() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, child) = delegated_task(&h).await;
    let (_, child_token) = h.started(child).await;
    let asked = tool(app, &child_token, "request_secret", deploy_args())
        .await
        .result;
    assert_eq!(asked["parked"], true, "{asked}");
    let shown = shown_turn(app, &leaf, child).await;
    assert_eq!(shown["secret_requests"][0]["name"], "deploy", "{shown}");
    assert_eq!(shown["secret_requests"][0]["status"], "pending", "{shown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_delegated_for_an_owner_request_opens_a_social_connection_card_in_its_own_chat() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, child) = delegated_task(&h).await;
    let (_, child_token) = h.started(child).await;
    let opened = tool(
        app,
        &child_token,
        "social_connect",
        json!({ "provider": "telegram", "reason": "Post approved updates to the team channel." }),
    )
    .await
    .result;
    assert_eq!(opened["parked"], true, "{opened}");
    let shown = shown_turn(app, &leaf, child).await;
    assert_eq!(shown["social_setups"][0]["provider"], "telegram", "{shown}");
    assert_eq!(shown["social_setups"][0]["status"], "pending", "{shown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_reply_in_the_bot_chat_joins_a_task_waiting_on_its_connection_card() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, child) = delegated_task(&h).await;
    let (_, child_token) = h.started(child).await;
    let opened = tool(
        app,
        &child_token,
        "social_connect",
        json!({ "provider": "telegram", "reason": "Post updates." }),
    )
    .await;
    assert_eq!(opened.result["parked"], true, "{}", opened.body);
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    let reply = app
        .send(
            &leaf,
            Uuid::new_v4(),
            "Never mind, skip Telegram for now.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(
        reply.body["turn"]["steered_into"],
        child.to_string(),
        "the owner's reply joins the task that asked"
    );
    let reply: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    parks_after(app, child, 2).await;
    let (takeover, setup): (Option<Uuid>, String) = sqlx::query_as(
        "SELECT d.takeover_turn_id, s.status FROM delegations d
         JOIN social_setups s ON s.turn_id = d.child_turn_id WHERE d.child_turn_id = ?1",
    )
    .bind(child)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(takeover, None, "the task keeps its delegating reply");
    assert_eq!(setup, "pending", "the connection card stays for the owner");
    assert_eq!(status(app, reply).await, "queued");
}

/// A running task the main bot delegated to a new bot while serving an owner
/// message: the new bot's ID and the task's turn.
async fn delegated_task(h: &Harness) -> (String, Uuid) {
    let app = &h.app;
    let main = app.main_bot().await;
    let leaf = app.create_bot("Telegram").await;
    let warm = app
        .send(&leaf, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let (root, parent) = h.running(&main, GATE).await;
    let delegated = tool(
        app,
        &parent,
        "delegate",
        json!({ "target_bot_id": leaf, "task": format!("{GATE} set up the Telegram connection") }),
    )
    .await;
    h.release(root).await;
    let task: Uuid = delegated.result["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    (leaf, child)
}

async fn shown_turn(app: &TestApp, bot: &str, turn: Uuid) -> Value {
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .unwrap_or_else(|| panic!("the task turn is in the bot's chat: {}", snapshot.body))
        .clone()
}

async fn raw_post(app: &TestApp, path: &str, body: String) -> (StatusCode, String) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "127.0.0.1:7878")
        .header("authorization", format!("Bearer {}", common::TOKEN))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}
