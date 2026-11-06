mod common;

use axum::http::StatusCode;
use common::recorder::{self, GATE, GATE_EVERY_ATTEMPT, Harness, tool};
use serde_json::{Value, json};

const SERVER: &str = r#"
import json, sys
calls = 0
for line in sys.stdin:
    m = json.loads(line)
    if 'id' not in m: continue
    if m['method'] == 'initialize':
        r = {'protocolVersion': m['params']['protocolVersion'], 'serverInfo': {'name':'tickets','version':'1'}, 'capabilities':{'tools':{}}}
    elif m['method'] == 'tools/list':
        r = {'tools':[{'name':n,'inputSchema':{'type':'object'}} for n in ['create_ticket','delete_ticket']]}
    elif m['method'] == 'tools/call':
        calls += 1
        r = {'content':[{'type':'text','text':json.dumps({'calls':calls,'arguments':m['params']['arguments']})}]}
    else: r = {}
    print(json.dumps({'jsonrpc':'2.0','id':m['id'],'result':r}), flush=True)
"#;

fn target() -> Value {
    json!({"scope":"mcp_tool", "mcp_id":"tickets", "tool":"create_ticket", "kind":"external"})
}

fn create(effect: &str, title: &str) -> Value {
    json!({"id":"tickets", "method":"tools/call", "params":{"name":"create_ticket",
        "arguments":{"title":title}}, "effect":effect, "purpose":"Create a ticket"})
}

async fn setup() -> (Harness, String, uuid::Uuid, String) {
    let h = recorder::harness(4).await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let installed = tool(
        &h.app,
        &token,
        "mcp_install",
        json!({"id":"tickets", "name":"Tickets",
        "description":"Disposable ticket board", "skill":"Create tickets on request.",
        "documentation":"Use tools/list.", "config":{"type":"stdio", "command":"python3",
        "args":["-u", "-c", SERVER]}}),
    )
    .await;
    assert!(!installed.error, "{}", installed.body);
    (h, bot, turn, token)
}

fn executed(called: &recorder::Called, title: &str) {
    assert!(!called.error, "{}", called.body);
    let body: Value = serde_json::from_str(
        called.result["result"]["content"][0]["text"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["arguments"]["title"], title);
    assert!(called.result.get("parked").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn routine_ticket_batch_runs_without_cards_but_deletion_still_waits() {
    let (h, bot, turn, token) = setup().await;
    for title in ["News crashes", "TradingView crash", "Binance TP/SL"] {
        executed(
            &tool(&h.app, &token, "mcp_request", create("local", title)).await,
            title,
        );
    }
    let conversation = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    assert_eq!(conversation.status, StatusCode::OK);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions WHERE turn_id = ?1")
        .bind(turn)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let delete = tool(
        &h.app,
        &token,
        "mcp_request",
        json!({"id":"tickets", "method":"tools/call",
        "params":{"name":"delete_ticket","arguments":{"id":"WEB-277"}},
        "effect":"destructive", "purpose":"Delete a ticket"}),
    )
    .await;
    assert_eq!(delete.result["status"], "pending");
    assert_eq!(delete.result["parked"], true);
    h.release(turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn always_allow_survives_new_requests_and_is_revocable_without_regrant_on_retry() {
    let (h, bot, turn, token) = setup().await;
    let pending = tool(&h.app, &token, "mcp_request", create("external", "First")).await;
    let question = pending.result["question_id"].as_str().unwrap();
    let answered = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({"choice":"approve_tool"})),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    h.release(turn).await;
    h.app
        .wait_turn(&turn.to_string(), &["waiting", "running", "completed"])
        .await;
    let authorization = "Always approve ticket creation.";
    let (next, token) = h.running(&bot, &format!("{GATE} {authorization}")).await;
    executed(
        &tool(&h.app, &token, "mcp_request", create("external", "Second")).await,
        "Second",
    );
    executed(
        &tool(&h.app, &token, "mcp_request", create("external", "Third")).await,
        "Third",
    );
    let confirmed = tool(
        &h.app,
        &token,
        "confirm_permission",
        json!({"target":target(), "authorization":authorization}),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    let listed = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/permissions"), None)
        .await;
    assert_eq!(listed.body["permissions"][0]["target"], target());
    let scope = listed.body["permissions"][0]["scope"].as_str().unwrap();
    let revoked = h
        .app
        .owner(
            "DELETE",
            &format!("/v1/bots/{bot}/permissions/{scope}"),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    let retry = tool(
        &h.app,
        &token,
        "confirm_permission",
        json!({"target":target(), "authorization":authorization}),
    )
    .await;
    assert_eq!(retry.result["status"], "revoked");
    let again = tool(&h.app, &token, "mcp_request", create("external", "Fourth")).await;
    assert_eq!(again.result["status"], "pending");
    h.release(next).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_can_remember_ticket_creation_after_a_skipped_card_without_approving_deletion() {
    let (h, bot, turn, token) = setup().await;
    let pending = tool(
        &h.app,
        &token,
        "mcp_request",
        create("external", "TradingView"),
    )
    .await;
    let authorization = "You remember to approve all tasks creations that's fine";
    let (next, token) = h.running(&bot, &format!("{GATE} {authorization}")).await;
    let remembered = tool(
        &h.app,
        &token,
        "confirm_permission",
        json!({
            "target":target(), "authorization":authorization,
        }),
    )
    .await;
    assert_eq!(
        remembered.result["status"], "approved",
        "{}",
        remembered.body
    );
    let status: String = sqlx::query_scalar("SELECT status FROM owner_questions WHERE id = ?1")
        .bind(
            pending.result["question_id"]
                .as_str()
                .unwrap()
                .parse::<uuid::Uuid>()
                .unwrap(),
        )
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "skipped");
    executed(
        &tool(
            &h.app,
            &token,
            "mcp_request",
            create("external", "TradingView"),
        )
        .await,
        "TradingView",
    );
    let mut deletion = create("destructive", "Do not delete");
    deletion["params"]["name"] = json!("delete_ticket");
    let blocked = tool(&h.app, &token, "mcp_request", deletion).await;
    assert_eq!(blocked.result["status"], "pending");
    h.release(turn).await;
    h.release(next).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_grants_require_current_owner_words_and_do_not_cover_other_bots_or_effects() {
    let (h, bot, turn, _) = setup().await;
    let authorization = "Always approve ticket creation.";
    let (next, token) = h.running(&bot, &format!("{GATE} {authorization}")).await;
    let forged = tool(
        &h.app,
        &token,
        "confirm_permission",
        json!({
            "target":target(), "authorization":"Always approve payments.",
        }),
    )
    .await;
    assert_eq!(forged.code(), "NOT_AUTHORIZED");
    let confirmed = tool(
        &h.app,
        &token,
        "confirm_permission",
        json!({
            "target":target(), "authorization":authorization,
        }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved");
    let other = h.app.create_bot("Other").await;
    let (other_turn, other_token) = h.running(&other, GATE).await;
    let blocked = tool(
        &h.app,
        &other_token,
        "mcp_request",
        create("external", "Other bot"),
    )
    .await;
    assert_eq!(blocked.result["status"], "pending");
    let blocked = tool(
        &h.app,
        &token,
        "mcp_request",
        create("destructive", "Other effect"),
    )
    .await;
    assert_eq!(blocked.result["status"], "pending");
    for turn in [turn, next, other_turn] {
        h.release(turn).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn delegated_work_cannot_inherit_a_saved_tool_permission() {
    let (h, _, turn, token) = setup().await;
    let worker = h.app.create_bot("Worker").await;
    let authorization = "Always approve ticket creation.";
    let (worker_turn, worker_token) = h.running(&worker, &format!("{GATE} {authorization}")).await;
    let confirmed = tool(
        &h.app,
        &worker_token,
        "confirm_permission",
        json!({
            "target":target(), "authorization":authorization,
        }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved");
    h.release(worker_turn).await;
    h.app
        .wait_turn(&worker_turn.to_string(), &["completed"])
        .await;
    let delegated = tool(
        &h.app,
        &token,
        "delegate",
        json!({
            "target_bot_id":worker, "task":format!("{GATE} Create a ticket"),
        }),
    )
    .await;
    assert!(!delegated.error, "{}", delegated.body);
    let task: uuid::Uuid = delegated.result["task_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let child: uuid::Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
            .bind(task)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    let (_, child_token) = h.started(child).await;
    let blocked = tool(
        &h.app,
        &child_token,
        "mcp_request",
        create("external", "Delegated"),
    )
    .await;
    assert_eq!(
        blocked.result["status"], "pending",
        "the saved permission does not cover it; the owner gets a card: {}",
        blocked.body
    );
    let question: uuid::Uuid = blocked.result["question_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let choices: String = sqlx::query_scalar("SELECT choices FROM owner_questions WHERE id = ?1")
        .bind(question)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    let ids: Vec<String> = serde_json::from_str::<Vec<serde_json::Value>>(&choices)
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        ids,
        ["approve", "deny"],
        "no standing grant from delegated work"
    );
    for turn in [turn, worker_turn, child] {
        h.release(turn).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_cards_keep_tool_identity_visible_with_a_maximum_length_context() {
    let (h, bot, turn, token) = setup().await;
    let mut args = create("external", "TradingView");
    args["context"] = json!("é".repeat(2000));
    let pending = tool(&h.app, &token, "mcp_request", args).await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let question = pending.result["question_id"].as_str().unwrap();
    let shown = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    assert_eq!(shown.status, StatusCode::OK);
    let context: String = sqlx::query_scalar("SELECT context FROM owner_questions WHERE id = ?1")
        .bind(question.parse::<uuid::Uuid>().unwrap())
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert!(context.starts_with("MCP: tickets / create_ticket (external)\n"));
    assert!(context.len() <= 4000);
    h.release(turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn removing_an_integration_revokes_its_saved_tool_permissions() {
    let (h, bot, turn, _) = setup().await;
    let authorization = "Always approve ticket creation.";
    let (next, token) = h.running(&bot, &format!("{GATE} {authorization}")).await;
    let confirmed = tool(
        &h.app,
        &token,
        "confirm_permission",
        json!({
            "target":target(), "authorization":authorization,
        }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved");
    let removed = h
        .app
        .owner("DELETE", "/v1/settings/mcp/tickets", None)
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let listed = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/permissions"), None)
        .await;
    assert_eq!(listed.body["permissions"], json!([]));
    h.release(turn).await;
    h.release(next).await;
}

#[tokio::test]
async fn migration_preserves_existing_destructive_permissions() {
    let db = common::TestDb::empty().await;
    let old = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_str().unwrap() < "0021" {
            std::fs::copy(entry.path(), old.path().join(entry.file_name())).unwrap();
        }
    }
    sqlx::migrate::Migrator::new(old.path())
        .await
        .unwrap()
        .run(&db.pool)
        .await
        .unwrap();
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let bot: uuid::Uuid = sqlx::query_scalar("SELECT default_bot_id FROM installation")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO bot_permissions (bot_id, scope, granted_at) VALUES (?1, 'destructive', '2026-11-06T00:00:00+00:00')")
        .bind(bot).execute(&db.pool).await.unwrap();
    else_api::db::migrate(&db.pool).await.unwrap();
    let saved = else_api::db::owner_questions::permissions(&db.pool, bot)
        .await
        .unwrap();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].scope, "destructive");
    assert_eq!(
        saved[0].granted_at.to_rfc3339(),
        "2026-11-06T00:00:00+00:00"
    );
    assert!(
        else_api::db::owner_questions::has_permission(
            &mut *db.pool.acquire().await.unwrap(),
            bot,
            "destructive",
        )
        .await
        .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_tool_grant_does_not_leave_a_used_approval_available_to_replay() {
    let (h, bot, turn, token) = setup().await;
    let args = create("external", "TradingView");
    let pending = tool(&h.app, &token, "mcp_request", args.clone()).await;
    let question = pending.result["question_id"].as_str().unwrap();
    let answered = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({"choice":"approve_tool"})),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK);
    h.release(turn).await;
    let continued = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let continued = h.grant(turn).await;
            if continued != token {
                break continued;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    executed(
        &tool(&h.app, &continued, "mcp_request", args.clone()).await,
        "TradingView",
    );
    let listed = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/permissions"), None)
        .await;
    let scope = listed.body["permissions"][0]["scope"].as_str().unwrap();
    let revoked = h
        .app
        .owner(
            "DELETE",
            &format!("/v1/bots/{bot}/permissions/{scope}"),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT);
    let replay = tool(&h.app, &continued, "mcp_request", args).await;
    assert_eq!(replay.result["status"], "pending", "{}", replay.body);
    h.release(turn).await;
}
