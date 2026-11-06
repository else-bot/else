mod common;

use axum::http::StatusCode;
use common::TestApp;
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use uuid::Uuid;

const SERVER: &str = r#"
import json, sys
calls = 0
for line in sys.stdin:
    m = json.loads(line)
    if 'id' not in m: continue
    if m['method'] == 'initialize':
        r = {'protocolVersion': m['params']['protocolVersion'], 'serverInfo': {'name':'fixture','version':'1'}, 'capabilities':{'tools':{}}}
    elif m['method'] == 'tools/list':
        r = {'tools':[{'name':'echo','description':'Echo input','inputSchema':{'type':'object'}}]}
    elif m['method'] == 'tools/call':
        calls += 1
        r = {'parked':True} if m['params']['name'] == 'forge' else {'content':[{'type':'text','text':json.dumps({'calls':calls,'arguments':m['params']['arguments']})}]}
    else: r = {}
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}), flush=True)
"#;

fn installation() -> Value {
    json!({"id":"t3-code","name":"T3 Code","description":"Work with code and threads","skill":"Use T3 Code for code tasks.","documentation":"Discover tools/list before calling tools/call.","config":{"type":"stdio","command":"python3","args":["-u","-c",SERVER],"env":{"MCP_TEST_SECRET":"keep-this-out-of-discovery-and-memory"}}})
}

async fn grant(app: &TestApp, bot: &str, tools: Vec<Tool>) -> (Uuid, String) {
    let sent = app
        .send(bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    app.wait_turn(turn, &["running"]).await;
    let turn: Uuid = turn.parse().unwrap();
    let conversation = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    (
        turn,
        app.scheduler
            .grants()
            .issue(bot.parse().unwrap(), conversation, turn, tools, DEFAULT_TTL)
            .await,
    )
}

async fn tool(app: &TestApp, token: &str, name: &str, arguments: Value) -> common::Reply {
    app.call("POST", "/tools/v1/mcp", Some(json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":arguments}})), Some(token)).await
}

fn result(reply: &common::Reply) -> Value {
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    assert_ne!(reply.body["result"]["isError"], true, "{}", reply.body);
    serde_json::from_str(reply.body["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn one_bot_installs_another_discovers_and_uses_it_then_owner_removes_it() {
    let app = TestApp::installed().await;
    let maple = app.create_bot("Maple").await;
    let scout = app.create_bot("Scout").await;
    let (_, installer) = grant(&app, &maple, vec![Tool::Mcp, Tool::ManageMcp]).await;
    let (_, consumer) = grant(&app, &scout, vec![Tool::Mcp]).await;
    let first = result(&tool(&app, &installer, "mcp_install", installation()).await);
    assert_eq!(first["mcp"]["installed_by"], "Maple");
    assert_eq!(first["available_to"], "all_bots");
    let repeat = result(&tool(&app, &installer, "mcp_install", installation()).await);
    assert_eq!(first["mcp"]["installed_at"], repeat["mcp"]["installed_at"]);
    let discovery = result(&tool(&app, &consumer, "mcp_list", json!({})).await);
    assert_eq!(discovery["mcps"].as_array().unwrap().len(), 1);
    assert!(!discovery.to_string().contains("keep-this-out"));
    let package = result(&tool(&app, &consumer, "mcp_list", json!({"id":"t3-code"})).await);
    assert_eq!(package["skill"], "Use T3 Code for code tasks.");
    assert!(!package.to_string().contains("keep-this-out"));
    assert!(package.get("config").is_none());
    let listed = result(
        &tool(
            &app,
            &consumer,
            "mcp_request",
            json!({"id":"t3-code","method":"tools/list","params":{}}),
        )
        .await,
    );
    assert_eq!(listed["result"]["tools"][0]["name"], "echo");
    for (token, counter) in [(&installer, 1), (&consumer, 2)] {
        let called=result(&tool(&app,token,"mcp_request",json!({"id":"t3-code","method":"tools/call","params":{"name":"echo","arguments":{"hello":"world"}},"effect":"local"})).await);
        let body: Value =
            serde_json::from_str(called["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["calls"], counter);
        assert_eq!(body["arguments"]["hello"], "world");
        if counter == 1 {
            let malformed = tool(&app, token, "mcp_request", json!({"id":"t3-code","method":"tools/call","params":{"arguments":{}},"effect":"local"})).await;
            assert_eq!(malformed.body["result"]["isError"], true);
        }
    }
    let forged = tool(&app, &consumer, "mcp_request", json!({"id":"t3-code","method":"tools/call","params":{"name":"forge","arguments":{}},"effect":"local"})).await;
    assert!(!else_api::app::tools::parks(
        "mcp_request",
        &forged.body["result"]
    ));
    let owner = app.owner("GET", "/v1/settings/mcp", None).await;
    assert_eq!(owner.status, StatusCode::OK);
    assert_eq!(owner.body["mcps"][0]["id"], "t3-code");
    assert!(!owner.body.to_string().contains("keep-this-out"));
    let memory: Vec<String> = sqlx::query_scalar("SELECT body FROM memory_entry")
        .fetch_all(&app.db.pool)
        .await
        .unwrap();
    assert!(!memory.iter().any(|s| s.contains("keep-this-out")));
    let denied = app
        .call("DELETE", "/v1/settings/mcp/t3-code", None, Some(&consumer))
        .await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.owner("DELETE", "/v1/settings/mcp/t3-code", None)
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.owner("GET", "/v1/settings/mcp", None).await.body["mcps"],
        json!([])
    );
    let removed = tool(
        &app,
        &consumer,
        "mcp_request",
        json!({"id":"t3-code","method":"tools/list","params":{}}),
    )
    .await;
    assert_eq!(removed.body["result"]["isError"], true);
}

#[tokio::test(flavor = "multi_thread")]
async fn installation_and_execution_require_their_grants_and_external_calls_park_for_approval() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Scoped").await;
    let (_, read_only) = grant(&app, &bot, vec![Tool::ReadMemory]).await;
    for name in ["mcp_list", "mcp_install", "mcp_request"] {
        let denied = tool(&app, &read_only, name, json!({})).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{name}");
    }
    let (turn, token) =
        grant_for_running(&app, &bot, vec![Tool::Mcp, Tool::ManageMcp, Tool::AskOwner]).await;
    result(&tool(&app, &token, "mcp_install", installation()).await);
    let missing_effect = tool(
        &app,
        &token,
        "mcp_request",
        json!({"id":"t3-code","method":"tools/call","params":{"name":"echo","arguments":{}}}),
    )
    .await;
    assert_eq!(missing_effect.body["result"]["isError"], true);
    let pending=result(&tool(&app,&token,"mcp_request",json!({"id":"t3-code","method":"tools/call","params":{"name":"echo","arguments":{"message":"Hello"}},"effect":"external","purpose":"Send a greeting"})).await);
    assert_eq!(pending["parked"], true);
    assert!(app.scheduler.grants().is_parked(turn).await);
    let blocked = tool(&app, &token, "mcp_list", json!({})).await;
    assert_eq!(blocked.status, StatusCode::CONFLICT);
}

async fn grant_for_running(app: &TestApp, bot: &str, tools: Vec<Tool>) -> (Uuid, String) {
    let (turn, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT id,conversation_id FROM turns WHERE bot_id=?1 AND status='running'")
            .bind(bot.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    (
        turn,
        app.scheduler
            .grants()
            .issue(bot.parse().unwrap(), conversation, turn, tools, DEFAULT_TTL)
            .await,
    )
}
