//! Secret requests: a bot asks for credentials over MCP, the owner answers
//! through the owner API, and the same turn continues. Values reach only
//! the encrypted task secret, never items, events, memory, request rows,
//! provider packets, or response bodies.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const CANARY: &str = "CANARY-7f3a9c";
/// A multiline private key: the value must survive byte for byte.
const PEM: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nCANARY-7f3a9c-line-one+/=\nsecond line\twith tab\n-----END OPENSSH PRIVATE KEY-----\n";
/// Long enough (a few seconds) to call tools while it streams.
const SLOW: &str = "[synthetic:slow] deploy the site; I need the server login for it.";

struct Harness {
    app: TestApp,
    synthetic: Arc<Synthetic>,
}

async fn harness() -> Harness {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    Harness {
        app: TestApp::with_registry(db, 4, registry),
        synthetic,
    }
}

/// Start a slow turn on `bot` and return it with a grant for its attempt.
async fn running(app: &TestApp, bot: &str, tools: Vec<Tool>) -> (Uuid, String) {
    let sent = app.send(bot, Uuid::new_v4(), SLOW, Some("synthetic")).await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let turn_id: Uuid = turn.parse().unwrap();
    (turn_id, grant(app, turn_id, tools).await)
}

async fn grant(app: &TestApp, turn_id: Uuid, tools: Vec<Tool>) -> String {
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    app.scheduler
        .grants()
        .issue(bot, conversation, turn_id, tools, DEFAULT_TTL)
        .await
}

fn owner_tools() -> Vec<Tool> {
    vec![Tool::ReadHistory, Tool::ReadMemory, Tool::RequestSecret]
}

async fn ask(app: &TestApp, token: &str, args: Value) -> common::Reply {
    app.call(
        "POST",
        "/tools/v1/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": { "name": "request_secret", "arguments": args } })),
        Some(token),
    )
    .await
}

fn deploy_args() -> Value {
    json!({ "name": "deploy", "reason": "Log in to the web server to deploy the site.",
            "env": ["DEPLOY_USER", "DEPLOY_KEY"] })
}

fn tool_text(reply: &common::Reply) -> Value {
    let text = reply.body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    serde_json::from_str(text).unwrap_or_else(|_| json!(text))
}

async fn request_id(app: &TestApp, token: &str) -> Uuid {
    let reply = ask(app, token, deploy_args()).await;
    let result = tool_text(&reply);
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

async fn wait(app: &TestApp, turn: Uuid, status: &str) {
    app.wait_turn_for(&turn.to_string(), &[status], Duration::from_secs(30))
        .await;
}

async fn attempts(app: &TestApp, turn: Uuid) -> i32 {
    sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

/// Every row of every table, as text, must not contain the canary. Task
/// secret ciphertext is included: it must not reveal the value either.
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

async fn resolved(app: &TestApp, bot: &str, name: &str) -> Vec<(String, String)> {
    let env = else_api::db::task_secrets::resolve(
        &app.db.pool,
        app.state.secret_key.as_deref(),
        bot.parse().unwrap(),
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
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main, owner_tools()).await;
    let request = request_id(&app, &token).await;
    // The attempt is parked: further calls, a retry included, are refused.
    let again = ask(&app, &token, deploy_args()).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{}", again.body);

    // The attempt ends and the turn waits for the owner, holding no slot.
    wait(&app, turn, "waiting").await;
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
    assert_eq!(shown["id"], request.to_string());
    assert_eq!(shown["status"], "pending");
    assert_eq!(shown["name"], "deploy");
    assert_eq!(shown["env"], json!(["DEPLOY_KEY", "DEPLOY_USER"]));
    assert_eq!(
        shown["reason"],
        "Log in to the web server to deploy the site."
    );

    let stored = fulfill(&app, &main, request, values()).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(stored.body["request"]["status"], "stored");
    assert_eq!(stored.body["requeued"], true);
    assert!(!stored.body.to_string().contains(CANARY));
    wait(&app, turn, "completed").await;
    assert_eq!(attempts(&app, turn).await, 2, "the same turn continued");
    assert_eq!(
        request_status(&app, request).await,
        ("stored".into(), Some(2))
    );

    // The value is exact (newlines and tabs kept) and only in the
    // encrypted task secret.
    let mut env = resolved(&app, &main, "deploy").await;
    env.sort();
    assert_eq!(
        env,
        vec![
            ("DEPLOY_KEY".to_owned(), PEM.to_owned()),
            ("DEPLOY_USER".to_owned(), "deployer".to_owned()),
        ]
    );
    let listed = app
        .owner("GET", &format!("/v1/bots/{main}/task-secrets"), None)
        .await;
    assert_eq!(listed.body["secrets"][0]["name"], "deploy");
    assert!(!listed.body.to_string().contains(CANARY));

    // A repeated answer changes nothing and never overwrites the value.
    let replay = fulfill(
        &app,
        &main,
        request,
        json!({ "env": { "DEPLOY_USER": "other", "DEPLOY_KEY": "other-key" } }),
    )
    .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body["requeued"], false);
    assert_eq!(resolved(&app, &main, "deploy").await.len(), 2);
    assert!(
        resolved(&app, &main, "deploy")
            .await
            .contains(&("DEPLOY_KEY".to_owned(), PEM.to_owned()))
    );
    // Declining after it was stored is refused.
    assert_eq!(
        cancel(&app, &main, request).await.status,
        StatusCode::CONFLICT
    );

    for packet in synthetic.packets().await {
        assert!(!packet.contains(CANARY), "a provider packet held the value");
    }
    let events = app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await;
    assert!(!events.body.to_string().contains(CANARY));
    assert_canary_absent_from_database(&app).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_before_the_attempt_ends_queues_the_continuation_directly() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main, owner_tools()).await;
    let request = request_id(&app, &token).await;
    let stored = fulfill(&app, &main, request, values()).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    assert_eq!(
        stored.body["requeued"], false,
        "the attempt is still running"
    );
    wait(&app, turn, "completed").await;
    assert_eq!(attempts(&app, turn).await, 2);
    assert_eq!(
        request_status(&app, request).await,
        ("stored".into(), Some(2))
    );
    assert_canary_absent_from_database(&app).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_request_continues_the_turn_without_a_secret() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main, owner_tools()).await;
    let request = request_id(&app, &token).await;
    wait(&app, turn, "waiting").await;
    let declined = cancel(&app, &main, request).await;
    assert_eq!(declined.status, StatusCode::OK, "{}", declined.body);
    assert_eq!(declined.body["request"]["status"], "cancelled");
    assert_eq!(
        cancel(&app, &main, request).await.status,
        StatusCode::OK,
        "retry"
    );
    wait(&app, turn, "completed").await;
    assert_eq!(
        request_status(&app, request).await,
        ("cancelled".into(), Some(2))
    );
    let late = fulfill(&app, &main, request, values()).await;
    assert_eq!(late.status, StatusCode::CONFLICT);
    assert!(!late.body.to_string().contains(CANARY));
    let listed = app
        .owner("GET", &format!("/v1/bots/{main}/task-secrets"), None)
        .await;
    assert_eq!(listed.body["secrets"], json!([]));

    // A new owner request may ask again.
    let (next, token) = running(&app, &main, owner_tools()).await;
    assert_ne!(next, turn);
    let fresh = tool_text(&ask(&app, &token, deploy_args()).await);
    assert_eq!(fresh["status"], "pending", "a new request may ask again");
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_delete_and_restart_settle_waiting_requests() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;

    // Stop work ends the waiting turn and withdraws its request.
    let (turn, token) = running(&app, &main, owner_tools()).await;
    let request = request_id(&app, &token).await;
    wait(&app, turn, "waiting").await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    wait(&app, turn, "interrupted").await;
    assert_eq!(request_status(&app, request).await.0, "cancelled");
    assert_eq!(
        fulfill(&app, &main, request, values()).await.status,
        StatusCode::CONFLICT
    );

    // Stop while the attempt still runs withdraws it as the turn ends.
    let (turn, token) = running(&app, &main, owner_tools()).await;
    let request = request_id(&app, &token).await;
    app.owner(
        "POST",
        &format!("/v1/bots/{main}/cancel"),
        Some(json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    wait(&app, turn, "interrupted").await;
    assert_eq!(request_status(&app, request).await.0, "cancelled");

    // A restart keeps a waiting request answerable; the answer continues it.
    let (turn, token) = running(&app, &main, owner_tools()).await;
    let request = request_id(&app, &token).await;
    wait(&app, turn, "waiting").await;
    let app = app.restart().await;
    assert_eq!(
        app.wait_turn(&turn.to_string(), &["waiting"]).await,
        "waiting"
    );
    assert_eq!(request_status(&app, request).await.0, "pending");
    let stored = fulfill(&app, &main, request, values()).await;
    assert_eq!(stored.status, StatusCode::OK, "{}", stored.body);
    wait(&app, turn, "completed").await;
    assert_eq!(
        resolved(&app, &main, "deploy").await.len(),
        2,
        "stored after restart"
    );

    // Deleting the bot withdraws its request; the request is then unknown.
    let other = app.create_bot("Ops", "Servers").await;
    let (turn, token) = running(&app, &other, owner_tools()).await;
    let request = request_id(&app, &token).await;
    wait(&app, turn, "waiting").await;
    let deleted = app
        .owner("DELETE", &format!("/v1/bots/{other}"), None)
        .await;
    assert!(deleted.status.is_success(), "{}", deleted.body);
    wait(&app, turn, "interrupted").await;
    assert_eq!(request_status(&app, request).await.0, "cancelled");
    assert_eq!(
        fulfill(&app, &other, request, values()).await.status,
        StatusCode::NOT_FOUND
    );
    assert_canary_absent_from_database(&app).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_answers_and_only_for_the_requesting_bot() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let other = app.create_bot("Home", "Family").await;
    let (turn, token) = running(&app, &main, owner_tools()).await;
    let request = request_id(&app, &token).await;
    let path = format!("/v1/bots/{main}/secret-requests/{request}/fulfill");

    // A task grant never authenticates owner routes, and no token fails.
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
    // Another bot's path cannot see or answer the request.
    for reply in [
        fulfill(&app, &other, request, values()).await,
        cancel(&app, &other, request).await,
        fulfill(&app, &main, Uuid::new_v4(), values()).await,
    ] {
        assert_eq!(reply.status, StatusCode::NOT_FOUND);
    }

    // Bodies that are not exactly the requested variables are refused
    // without echoing anything sent.
    let bad_bodies = [
        json!({ "env": { "DEPLOY_USER": "u" , "DEPLOY_KEY": PEM, "EXTRA": CANARY } }),
        json!({ "env": { "DEPLOY_KEY": PEM } }),
        json!({ "env": { "DEPLOY_USER": CANARY, "DEPLOY_KEY": 7 } }),
        json!({ "env": { "DEPLOY_USER": "u", "DEPLOY_KEY": PEM }, "note": CANARY }),
        json!({ "env": [CANARY] }),
        json!({ "env": { "DEPLOY_USER": "u", "DEPLOY_KEY": format!("{CANARY}\u{0}") } }),
    ];
    for body in bad_bodies {
        let reply = fulfill(&app, &main, request, body.clone()).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{body}");
        assert!(!reply.body.to_string().contains(CANARY), "{}", reply.body);
    }
    // Malformed JSON text is refused generically too.
    let raw = raw_post(
        &app,
        &path,
        format!("{{\"env\": {{\"DEPLOY_KEY\": \"{CANARY}"),
    )
    .await;
    assert_eq!(raw.0, StatusCode::BAD_REQUEST);
    assert!(!raw.1.contains(CANARY), "{}", raw.1);
    assert_eq!(request_status(&app, request).await.0, "pending");

    // A grant without the tool is refused before anything is recorded.
    let bare = grant(&app, turn, vec![Tool::ReadMemory]).await;
    let refused = ask(&app, &bare, deploy_args()).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_canary_absent_from_database(&app).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn arguments_carry_metadata_only_and_task_conversations_cannot_ask() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (_, token) = running(&app, &main, owner_tools()).await;
    for args in [
        json!({ "name": "deploy", "reason": "r", "env": ["DEPLOY_KEY"], "value": CANARY }),
        json!({ "name": "deploy", "reason": "r", "env": { "DEPLOY_KEY": CANARY } }),
        json!({ "name": "deploy", "reason": "r", "env": [CANARY] }),
    ] {
        let reply = ask(&app, &token, args).await;
        assert_eq!(reply.body["result"]["isError"], true, "{}", reply.body);
        assert!(!reply.body.to_string().contains(CANARY));
    }
    // Nothing of a refused call is recorded, not even in tool activity.
    assert_canary_absent_from_database(&app).await;
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM secret_requests")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(none, 0);

    // A delegated task runs in a task conversation nobody sees: refused.
    let leaf = app.create_bot("Ops", "Servers").await;
    let warm = app
        .send(&leaf, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    app.wait_turn(warm.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let parent = grant_with_delegate(&app, &main).await;
    let delegated = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "delegate",
                                     "arguments": { "target_bot_id": leaf,
                                                    "task": "[synthetic:slow] check the server disk usage now" } } })),
            Some(&parent),
        )
        .await;
    let task: Uuid = tool_text(&delegated)["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.wait_turn_for(&child.to_string(), &["running"], Duration::from_secs(30))
        .await;
    let child_token = grant(&app, child, owner_tools()).await;
    let refused = tool_text(&ask(&app, &child_token, deploy_args()).await);
    assert!(
        refused.as_str().unwrap().starts_with("NOT_ALLOWED"),
        "{refused}"
    );
}

/// A grant for a new running main-bot turn that may delegate.
async fn grant_with_delegate(app: &TestApp, main: &str) -> String {
    let (turn, _) = running(app, main, owner_tools()).await;
    grant(app, turn, vec![Tool::ReadMemory, Tool::Delegate]).await
}

/// Post a body that is not valid JSON with the owner token.
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
