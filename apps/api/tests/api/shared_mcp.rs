use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, harness, tool};
use else_api::grants::Tool;
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
        if m['params']['name'] == 'forge': r = {'parked':True}
        elif m['params']['name'] == 'reject': r = {'content':[{'type':'text','text':'Rejected'}],'isError':True}
        elif m['params']['name'] == 'rows': r = {'content':[{'type':'text','text':'Found 2000 rows'}],'structuredContent':{'rows':[{'id':i,'title':'row'} for i in range(2000)]}}
        else: r = {'content':[{'type':'text','text':json.dumps({'calls':calls,'arguments':m['params']['arguments']})}]}
    else: r = {}
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}), flush=True)
"#;

pub(crate) fn installation() -> Value {
    json!({"id":"t3-code","name":"T3 Code","description":"Work with code and threads","skill":"Use T3 Code for code tasks.","documentation":"Discover tools/list before calling tools/call.","config":{"type":"stdio","command":"python3","args":["-u","-c",SERVER],"env":{"MCP_TEST_SECRET":"keep-this-out-of-discovery-and-memory"}}})
}

#[tokio::test(flavor = "multi_thread")]
async fn one_bot_installs_another_discovers_and_uses_it_then_owner_removes_it() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let maple = app.create_bot("Maple").await;
    let scout = app.create_bot("Scout").await;
    let (_, installer) = h.running(&maple, GATE).await;
    let (turn, _) = h.running(&scout, GATE).await;
    let consumer = h.grant_with(turn, vec![Tool::Mcp]).await;
    let first = tool(app, &installer, "mcp_install", installation())
        .await
        .result;
    assert_eq!(first["mcp"]["installed_by"], "Maple");
    assert_eq!(first["available_to"], "all_bots");
    let repeat = tool(app, &installer, "mcp_install", installation())
        .await
        .result;
    assert_eq!(first["mcp"]["installed_at"], repeat["mcp"]["installed_at"]);
    let discovery = tool(app, &consumer, "mcp_list", json!({})).await.result;
    assert_eq!(discovery["mcps"].as_array().unwrap().len(), 1);
    assert!(!discovery.to_string().contains("keep-this-out"));
    let package = tool(app, &consumer, "mcp_list", json!({"id":"t3-code"}))
        .await
        .result;
    assert_eq!(package["skill"], "Use T3 Code for code tasks.");
    assert!(!package.to_string().contains("keep-this-out"));
    assert!(package.get("config").is_none());
    let listed = tool(
        app,
        &consumer,
        "mcp_request",
        json!({"id":"t3-code","method":"tools/list","params":{}}),
    )
    .await
    .result;
    assert_eq!(listed["result"]["tools"][0]["name"], "echo");
    assert_eq!(listed["result"]["tools"][0]["description"], "Echo input");
    assert!(listed["result"]["tools"][0].get("inputSchema").is_none());
    let schemas = tool(
        app,
        &consumer,
        "mcp_request",
        json!({"id":"t3-code","method":"tools/list","params":{"names":["echo","nope"]}}),
    )
    .await
    .result;
    assert_eq!(
        schemas["result"]["tools"][0]["inputSchema"]["type"],
        "object"
    );
    assert_eq!(schemas["result"]["missing"], json!(["nope"]));
    for (token, counter) in [(&installer, 1), (&consumer, 2)] {
        let called=tool(app,token,"mcp_request",json!({"id":"t3-code","method":"tools/call","params":{"name":"echo","arguments":{"hello":"world"}},"effect":"local"})).await.result;
        let body: Value =
            serde_json::from_str(called["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["calls"], counter);
        assert_eq!(body["arguments"]["hello"], "world");
        if counter == 1 {
            let malformed = tool(app, token, "mcp_request", json!({"id":"t3-code","method":"tools/call","params":{"arguments":{}},"effect":"local"})).await;
            assert!(malformed.error);
            let plan = "a".repeat(301);
            let long = tool(app, token, "mcp_request", json!({"id":"t3-code","method":"tools/call","params":{"name":"echo","arguments":{}},"effect":"local","purpose":plan})).await;
            assert!(long.error);
            assert!(
                long.body["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("purpose is 301 bytes; the limit is 300."),
                "{}",
                long.body
            );
        }
    }
    let forged = tool(app, &consumer, "mcp_request", json!({"id":"t3-code","method":"tools/call","params":{"name":"forge","arguments":{}},"effect":"local"})).await;
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
        app,
        &consumer,
        "mcp_request",
        json!({"id":"t3-code","method":"tools/list","params":{}}),
    )
    .await;
    assert!(removed.error);
}

#[tokio::test(flavor = "multi_thread")]
async fn installation_and_execution_require_their_grants_and_external_calls_park_for_approval() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.create_bot("Scoped").await;
    let (turn, token) = h.running(&bot, GATE).await;
    let read_only = h.grant_with(turn, vec![Tool::ReadMemory]).await;
    for name in ["mcp_list", "mcp_install", "mcp_request"] {
        let denied = tool(app, &read_only, name, json!({})).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN, "{name}");
    }
    let installed = tool(app, &token, "mcp_install", installation()).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let missing_effect = tool(
        app,
        &token,
        "mcp_request",
        json!({"id":"t3-code","method":"tools/call","params":{"name":"echo","arguments":{}}}),
    )
    .await;
    assert!(missing_effect.error);
    let pending=tool(app,&token,"mcp_request",json!({"id":"t3-code","method":"tools/call","params":{"name":"echo","arguments":{"message":"Hello"}},"effect":"external","purpose":"Send a greeting"})).await.result;
    assert_eq!(pending["parked"], true);
    assert!(app.state.grants.is_parked(turn).await);
    let blocked = tool(app, &token, "mcp_list", json!({})).await;
    assert_eq!(blocked.status, StatusCode::CONFLICT);
    h.release(turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replacing_a_different_package_waits_for_the_owner_and_resets_a_changed_connection() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let maple = app.create_bot("Maple").await;
    let (turn, token) = h.running(&maple, GATE_EVERY_ATTEMPT).await;
    let installed = tool(app, &token, "mcp_install", installation()).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let mut moved = installation();
    moved["config"]["env"]["MCP_TEST_SECRET"] = json!("new-secret-stays-out-of-cards");
    let conflict = tool(app, &token, "mcp_install", moved.clone()).await;
    assert!(conflict.error);
    assert!(
        conflict.body.to_string().contains("replace true"),
        "{}",
        conflict.body
    );

    moved["replace"] = json!(true);
    let mut leaky = moved.clone();
    leaky["config"] = json!({"type":"http","url":"https://alice:card-secret@example.com/mcp"});
    let refused = tool(app, &token, "mcp_install", leaky).await;
    assert!(refused.error, "{}", refused.body);
    let asked: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(asked, 0, "invalid input is refused before any card");
    let pending = tool(app, &token, "mcp_install", moved.clone()).await.result;
    assert_eq!(pending["parked"], true, "{pending}");
    assert!(app.state.grants.is_parked(turn).await);
    let (question, kind, title, context, choices): (Uuid, String, String, String, String) =
        sqlx::query_as(
            "SELECT id, kind, title, context, choices FROM owner_questions WHERE turn_id = ?1",
        )
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        (kind.as_str(), title.as_str()),
        ("external", "Replace the T3 Code MCP")
    );
    let choices: Vec<Value> = serde_json::from_str(&choices).unwrap();
    assert_eq!(
        choices.len(),
        2,
        "approve or deny, no standing grant: {choices:?}"
    );
    assert!(context.contains("Changes: connection"), "{context}");
    assert!(context.contains("environment MCP_TEST_SECRET"), "{context}");
    assert!(!context.contains("secret-stays") && !context.contains("keep-this-out"));
    assert!(
        !app.owner("GET", "/v1/settings/mcp", None)
            .await
            .body
            .to_string()
            .contains("new-secret"),
        "nothing changes before approval"
    );

    let scout = app.create_bot("Scout").await;
    let tool_grant = "mcp:t3-code:external:6563686f";
    for scope in ["destructive", tool_grant] {
        sqlx::query("INSERT INTO bot_permissions (bot_id, scope) VALUES (?1, ?2)")
            .bind(scout.parse::<Uuid>().unwrap())
            .bind(scope)
            .execute(&app.db.pool)
            .await
            .unwrap();
    }
    let (_, scout_token) = h.running(&scout, GATE).await;
    let standing = tool(app, &scout_token, "mcp_install", moved.clone())
        .await
        .result;
    assert_eq!(
        standing["parked"], true,
        "standing grants never replace: {standing}"
    );

    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let answered = app
        .owner(
            "POST",
            &format!("/v1/bots/{maple}/questions/{question}/answer"),
            Some(json!({ "choice": "approve" })),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let token = h.continued(turn, &token).await;
    let replaced = tool(app, &token, "mcp_install", moved).await.result;
    assert_eq!(replaced["replaced"], true, "{replaced}");
    assert_eq!(replaced["reconnect_required"], true);
    assert_eq!(replaced["mcp"]["installed_by"], "Maple");
    let scopes: Vec<String> = sqlx::query_scalar("SELECT scope FROM bot_permissions")
        .fetch_all(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(scopes, ["destructive"]);
    let called = tool(
        app,
        &token,
        "mcp_request",
        json!({"id":"t3-code","method":"tools/list","params":{}}),
    )
    .await
    .result;
    assert_eq!(called["result"]["tools"][0]["name"], "echo");
}

const BENTO_HEADERS: [(&str, &str); 3] = [
    ("x-bento-publishable-key", "pk-private-1234"),
    ("x-bento-secret-key", "sk-private-5678"),
    ("x-bento-site-uuid", "90125678"),
];
const ROTATED_SECRET: &str = "sk-rotated-4321";
/// The secret key Bento accepts, replaced mid-test like a rotated key.
static ACCEPTED_SECRET: std::sync::Mutex<&str> = std::sync::Mutex::new(BENTO_HEADERS[1].1);

/// Bento's MCP as observed live: header credentials, 400 without them and
/// no OAuth challenge, SSE replies, an empty 202 for notifications, and no
/// session id.
async fn bento(
    headers: axum::http::HeaderMap,
    axum::Json(message): axum::Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let accepted = *ACCEPTED_SECRET.lock().unwrap();
    if BENTO_HEADERS.iter().any(|(name, value)| {
        let value = if *name == "x-bento-secret-key" {
            accepted
        } else {
            value
        };
        headers.get(*name).and_then(|v| v.to_str().ok()) != Some(value)
    }) {
        return (StatusCode::BAD_REQUEST, "missing Bento credentials").into_response();
    }
    if message.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let result = match message["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":message["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"bento","version":"1"}})
        }
        "tools/list" => {
            json!({"tools":[{"name":"bento_list_tags","description":"List tags","inputSchema":{"type":"object"}}]})
        }
        _ => {
            let echoed = format!("tags for site {}", BENTO_HEADERS[2].1);
            let site: u64 = BENTO_HEADERS[2].1.parse().unwrap();
            json!({"content":[{"type":"text","text":echoed}],"structuredContent":{"site":site,"key":accepted}})
        }
    };
    let body = json!({"jsonrpc":"2.0","id":message["id"],"result":result});
    (
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        format!("event: message\ndata: {body}\n\n"),
    )
        .into_response()
}

#[tokio::test(flavor = "multi_thread")]
async fn header_credentials_come_from_task_secrets_and_connect_without_oauth() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route("/mcp", axum::routing::post(bento)),
        )
        .await
        .unwrap()
    });
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let maple = app.create_bot("Maple").await;
    let (_, token) = h.running(&maple, GATE).await;
    let install = |secrets: Value| {
        json!({"id":"bento","name":"Bento","description":"Email marketing","skill":"Use Bento for subscribers.","documentation":"Discover tools first.","config":{"type":"http","url":url,"secrets":secrets,"headers":{
            "X-Bento-Publishable-Key":"${BENTO_PUBLISHABLE_KEY}",
            "X-Bento-Secret-Key":"${BENTO_SECRET_KEY}",
            "X-Bento-Site-UUID":"${BENTO_SITE_UUID}",
        }}})
    };

    let unnamed = tool(app, &token, "mcp_install", install(json!([]))).await;
    assert!(unnamed.error, "{}", unnamed.body);
    let missing = tool(app, &token, "mcp_install", install(json!(["bento"]))).await;
    assert!(missing.error, "{}", missing.body);
    assert!(
        missing.body.to_string().contains("bento"),
        "{}",
        missing.body
    );

    let stored = app
        .owner(
            "PUT",
            "/v1/task-secrets/bento",
            Some(json!({"env":{
                "BENTO_PUBLISHABLE_KEY": BENTO_HEADERS[0].1,
                "BENTO_SECRET_KEY": BENTO_HEADERS[1].1,
                "BENTO_SITE_UUID": BENTO_HEADERS[2].1,
            }})),
        )
        .await;
    assert_eq!(stored.status, StatusCode::CREATED, "{}", stored.body);
    let installed = tool(app, &token, "mcp_install", install(json!(["bento"]))).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let config =
        std::fs::read_to_string(app.config.data_dir.join("mcp/bento/config.json")).unwrap();
    assert!(config.contains("${BENTO_SECRET_KEY}"), "{config}");
    assert!(
        !config.contains("private"),
        "values stay out of the config: {config}"
    );

    let connected = tool(app, &token, "mcp_connect", json!({"id":"bento"}))
        .await
        .result;
    assert_eq!(connected["status"], "connected");
    let status = tool(app, &token, "mcp_connection_status", json!({"id":"bento"}))
        .await
        .result;
    assert_eq!(status["status"], "connected");
    assert_eq!(status["provider"], Value::Null);
    let listed = tool(
        app,
        &token,
        "mcp_request",
        json!({"id":"bento","method":"tools/list","params":{}}),
    )
    .await
    .result;
    assert_eq!(listed["result"]["tools"][0]["name"], "bento_list_tags");
    let called = tool(app, &token, "mcp_request", json!({"id":"bento","method":"tools/call","params":{"name":"bento_list_tags","arguments":{}},"effect":"local"})).await.result;
    assert_eq!(
        called["result"]["content"][0]["text"],
        "tags for site [secret BENTO_SITE_UUID]"
    );
    assert_eq!(
        called["result"]["structuredContent"]["site"],
        "[secret BENTO_SITE_UUID]"
    );
    assert_eq!(
        called["result"]["structuredContent"]["key"],
        "[secret BENTO_SECRET_KEY]"
    );

    *ACCEPTED_SECRET.lock().unwrap() = ROTATED_SECRET;
    let rotated = app
        .owner(
            "PUT",
            "/v1/task-secrets/bento",
            Some(json!({"env":{
                "BENTO_PUBLISHABLE_KEY": BENTO_HEADERS[0].1,
                "BENTO_SECRET_KEY": ROTATED_SECRET,
                "BENTO_SITE_UUID": BENTO_HEADERS[2].1,
            }})),
        )
        .await;
    assert_eq!(rotated.status, StatusCode::OK, "{}", rotated.body);
    let called = tool(app, &token, "mcp_request", json!({"id":"bento","method":"tools/call","params":{"name":"bento_list_tags","arguments":{}},"effect":"local"})).await.result;
    assert_eq!(
        called["result"]["structuredContent"]["key"], "[secret BENTO_SECRET_KEY]",
        "the next call sends the rotated key and redacts it"
    );
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_result_is_saved_whole_in_the_bots_project() {
    let root = tempfile::tempdir().unwrap();
    let h = harness(TestApp::builder().projects(root.path())).await;
    let app = &h.app;
    let bot = app.create_bot("Scout").await;
    let (_, token) = h.running(&bot, GATE).await;
    let installed = tool(app, &token, "mcp_install", installation()).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let long = "row ".repeat(8_000);
    let called = tool(app, &token, "mcp_request", json!({"id":"t3-code","method":"tools/call","params":{"name":"echo","arguments":{"text":long}},"effect":"local"})).await.result;
    assert!(called.to_string().len() < 12_000, "the preview stays small");
    let path = called["whole_result"].as_str().expect("whole result saved");
    assert!(path.starts_with("mcp-results/t3-code-"), "{path}");
    assert!(called["next"].as_str().unwrap().contains(path));
    assert!(path.ends_with(".json"), "the upstream text is JSON: {path}");
    let echoed: Value =
        serde_json::from_str(&std::fs::read_to_string(root.path().join(&bot).join(path)).unwrap())
            .unwrap();
    assert_eq!(
        echoed["arguments"]["text"], long,
        "the saved copy is the whole upstream text"
    );

    let rows = tool(app, &token, "mcp_request", json!({"id":"t3-code","method":"tools/call","params":{"name":"rows","arguments":{}},"effect":"local"})).await.result;
    let path = rows["whole_result"].as_str().unwrap();
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(root.path().join(&bot).join(path)).unwrap())
            .unwrap();
    assert_eq!(
        saved["result"]["structuredContent"]["rows"]
            .as_array()
            .unwrap()
            .len(),
        2000,
        "structured data the text does not repeat is saved with it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_project_a_result_keeps_the_larger_budget() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.create_bot("Scout").await;
    let (_, token) = h.running(&bot, GATE).await;
    let installed = tool(app, &token, "mcp_install", installation()).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let long = "row ".repeat(3_500);
    let called = tool(app, &token, "mcp_request", json!({"id":"t3-code","method":"tools/call","params":{"name":"echo","arguments":{"text":long}},"effect":"local"})).await.result;
    assert!(called.get("whole_result").is_none());
    let echoed: Value =
        serde_json::from_str(called["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        echoed["arguments"]["text"], long,
        "a 14 KB result still arrives whole"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn else_side_mcp_request_failures_use_the_json_error_envelope() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.create_bot("Parser").await;
    let (_, token) = h.running(&bot, GATE).await;
    let mut broken = installation();
    broken["id"] = json!("broken");
    broken["config"]["args"] = json!(["-c", "import sys; sys.exit(1)"]);
    let installed = tool(app, &token, "mcp_install", broken).await;
    assert_eq!(installed.status, StatusCode::OK, "{}", installed.body);
    assert!(!installed.error, "{}", installed.body);
    let failed = tool(
        app,
        &token,
        "mcp_request",
        json!({"id":"broken","method":"tools/list","params":{}}),
    )
    .await;
    assert_eq!(failed.status, StatusCode::OK, "{}", failed.body);
    assert!(failed.error, "{}", failed.body);
    let failed = failed.result;
    assert_eq!(failed["error"]["code"], "connection_failed", "{failed}");
    assert!(
        failed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("connection failed"),
        "{failed}"
    );
    let missing = tool(
        app,
        &token,
        "mcp_request",
        json!({"id":"nowhere","method":"tools/list","params":{}}),
    )
    .await;
    assert_eq!(missing.status, StatusCode::OK, "{}", missing.body);
    assert!(missing.error, "{}", missing.body);
    let missing = missing.result;
    assert_eq!(missing["error"]["code"], "not_found", "{missing}");
    let invalid = tool(
        app,
        &token,
        "mcp_request",
        json!({"id":"broken","method":"tools/call","params":{"name":"echo","arguments":{}}}),
    )
    .await;
    assert_eq!(invalid.status, StatusCode::OK, "{}", invalid.body);
    assert!(invalid.error, "{}", invalid.body);
    let invalid = invalid.result;
    assert_eq!(invalid["error"]["code"], "invalid_request", "{invalid}");
}
