mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{TestApp, TestDb};
use else_api::app::providers::Provider;
use else_api::providers::cursor::Cursor;
use else_api::providers::cursor_home::{self, Login};
use else_api::providers::host::Host;
use serde_json::{Value, json};
use uuid::Uuid;

async fn installed() -> PathBuf {
    if let Some(configured) = std::env::var_os("ELSE_CURSOR_BIN") {
        return configured.into();
    }
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let link = home.join(cursor_home::INSTALLED);
    if link.is_file() {
        return link;
    }
    else_api::providers::executable::executable(
        &Host::new(&std::env::temp_dir(), &std::env::temp_dir()),
        None,
        "cursor-agent",
    )
    .await
    .expect("resolve Cursor CLI")
    .expect("Cursor CLI in ~/.local/bin, on PATH, or in ELSE_CURSOR_BIN")
    .path()
    .to_owned()
}

fn live() -> bool {
    let on = std::env::var("ELSE_LIVE_CURSOR").as_deref() == Ok("1");
    if !on {
        eprintln!("SKIPPED: set ELSE_LIVE_CURSOR=1 to run live Cursor checks");
    }
    on
}

async fn ask(app: &TestApp, bot: &str, text: &str) -> (Uuid, String) {
    let reply = app.send(bot, Uuid::new_v4(), text, Some("cursor")).await;
    assert!(reply.status.is_success(), "{}", reply.body);
    let turn = reply.body["turn"]["id"].as_str().unwrap();
    let status = app
        .wait_live_turn(turn, &["completed", "failed", "interrupted", "uncertain"])
        .await;
    let (answer, error): (String, Option<String>) = sqlx::query_as(
        "SELECT coalesce((SELECT group_concat(text, '') FROM items WHERE turn_id=t.id AND kind='assistant'), ''), t.error_message FROM turns t WHERE t.id=?1"
    ).bind(turn.parse::<Uuid>().unwrap()).fetch_one(&app.db.pool).await.unwrap();
    eprintln!("cursor: {status}, {error:?}, {answer}");
    assert_eq!(status, "completed", "{error:?}");
    (turn.parse().unwrap(), answer)
}

async fn session(app: &TestApp, bot: &str) -> String {
    sqlx::query_scalar("SELECT native_session_id FROM provider_sessions WHERE bot_id=?1 AND provider_id='cursor' AND status='active'")
        .bind(bot.parse::<Uuid>().unwrap()).fetch_one(&app.db.pool).await.unwrap()
}

/// A signed-in Cursor whose catalog holds the account's models, and the
/// model the checks use: `ELSE_LIVE_CURSOR_MODEL`, else the first listed.
async fn cursor(host: &Host, tool_url: &str) -> (Cursor, String) {
    let provider = Cursor::detect(Some(host.clone()), Some(installed().await), Login::host())
        .await
        .with_tools(tool_url.into());
    let listed = provider.list_models().await.expect("Cursor lists models");
    let model = std::env::var("ELSE_LIVE_CURSOR_MODEL")
        .unwrap_or_else(|_| listed.first().expect("a model").id.clone());
    provider
        .models()
        .unwrap()
        .listed(listed, chrono::Utc::now())
        .unwrap();
    (provider, model)
}

#[tokio::test(flavor = "multi_thread")]
async fn cursor_conversations_stream_resume_use_host_and_mcp_tools_and_cancel() {
    if !live() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().canonicalize().unwrap();
    let host = Host::new(&data, &data.join("projects"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/tools/v1/mcp", listener.local_addr().unwrap());
    let (provider, model) = cursor(&host, &url).await;
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let app = TestApp::builder()
        .db(db)
        .max_active(2)
        .providers(vec![Arc::new(provider)])
        .start()
        .await;
    let (calls, mut observed_calls) = tokio::sync::mpsc::channel(64);
    let router = app.router.clone().layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let calls = calls.clone();
            async move {
                let (parts, body) = request.into_parts();
                let bytes = axum::body::to_bytes(body, 2 * 1024 * 1024).await.unwrap();
                if let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                    && value["method"] == "tools/call"
                    && let Some(name) = value["params"]["name"].as_str()
                {
                    calls.send(name.to_owned()).await.unwrap();
                }
                next.run(axum::extract::Request::from_parts(
                    parts,
                    axum::body::Body::from(bytes),
                ))
                .await
            }
        },
    ));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let saved = app
        .owner(
            "PATCH",
            "/v1/settings/providers/cursor",
            Some(
                json!({ "model": model, "reasoning_effort": "default", "service_tier": "default" }),
            ),
        )
        .await;
    assert!(saved.status.is_success(), "{}", saved.body);
    let a = app.create_bot("Cursor Alpha").await;
    let b = app.create_bot("Cursor Beta").await;

    let (_, answer) = ask(
        &app,
        &a,
        "We are preparing the ZETA-9 release. Acknowledge it in one sentence.",
    )
    .await;
    assert_eq!(
        answer.matches("ZETA-9").count(),
        1,
        "each delta once: {answer}"
    );
    let first = session(&app, &a).await;
    let (_, answer) = ask(
        &app,
        &a,
        "What release are we preparing? Reply with the release name.",
    )
    .await;
    assert!(answer.contains("ZETA-9"), "{answer}");
    assert_eq!(session(&app, &a).await, first, "chat resumed");

    let (_, answer) = ask(&app, &a, "Use your shell to write exactly HOST_CURSOR_OK into cursor-proof.txt in the current directory. Then run pwd and reply with the absolute directory.").await;
    let project = host.project_path(a.parse().unwrap());
    assert_eq!(
        std::fs::read_to_string(project.join("cursor-proof.txt"))
            .unwrap()
            .trim_end(),
        "HOST_CURSOR_OK"
    );
    assert!(answer.contains(project.to_str().unwrap()), "{answer}");

    let (_, answer) = ask(&app, &a, "Use the elsebot list_bots MCP tool to list the bots. Do not use the shell. Reply with the returned bot names.").await;
    assert!(answer.contains("Cursor Beta"), "{answer}");
    let mut called = Vec::new();
    while let Ok(name) = observed_calls.try_recv() {
        called.push(name);
    }
    assert!(
        called.iter().any(|name| name == "list_bots"),
        "the MCP call reached the real server with the grant: {called:?}"
    );

    ask(
        &app,
        &b,
        "We are preparing the BETA release. Acknowledge it in one sentence.",
    )
    .await;
    assert_ne!(session(&app, &b).await, first, "bots have separate chats");

    let sent = app
        .send(
            &a,
            Uuid::new_v4(),
            "Run `touch cursor-sleep-started; sleep 120` in the shell and wait until it finishes. Then reply DONE.",
            Some("cursor"),
        )
        .await;
    let interrupted = sent.body["turn"]["id"].as_str().unwrap();
    tokio::time::timeout(
        common::LIVE,
        common::poll(|| {
            std::future::ready(project.join("cursor-sleep-started").exists().then_some(()))
        }),
    )
    .await
    .expect("Cursor started the shell command");
    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{a}/cancel"),
            Some(json!({"request_id":Uuid::new_v4()})),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    let status = app
        .wait_live_turn(
            interrupted,
            &["completed", "interrupted", "failed", "uncertain"],
        )
        .await;
    assert_eq!(status, "interrupted");
    assert!(
        host.live_processes(interrupted.parse().unwrap()).is_empty(),
        "the CLI and its shell were reaped"
    );
    let (_, answer) = ask(
        &app,
        &a,
        "What release are we preparing? Reply with the release name.",
    )
    .await;
    assert!(answer.contains("ZETA-9"), "history rebuilt: {answer}");
    server.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_newer_owner_message_joins_a_running_cursor_reply() {
    if !live() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().canonicalize().unwrap();
    let host = Host::new(&data, &data.join("projects"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/tools/v1/mcp", listener.local_addr().unwrap());
    let (provider, model) = cursor(&host, &url).await;
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let app = TestApp::builder()
        .db(db)
        .max_active(2)
        .providers(vec![Arc::new(provider)])
        .start()
        .await;
    let router = app.router.clone();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let saved = app
        .owner(
            "PATCH",
            "/v1/settings/providers/cursor",
            Some(
                json!({ "model": model, "reasoning_effort": "default", "service_tier": "default" }),
            ),
        )
        .await;
    assert!(saved.status.is_success(), "{}", saved.body);
    // The CLI takes nothing mid-turn: the message joins the reply's next
    // attempt in the same chat.
    common::steer::joins_mid_tool_and_mid_text(&app, "cursor", false).await;
}
