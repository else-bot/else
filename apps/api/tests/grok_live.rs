mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{TestApp, TestDb};
use else_api::app::providers::Provider;
use else_api::providers::{Registry, grok::Grok, grok_home, host::Host};
use serde_json::{Value, json};
use uuid::Uuid;

fn installed() -> PathBuf {
    let configured = std::env::var_os("ELSE_GROK_BIN").map(PathBuf::from);
    else_api::providers::executable::executable(configured.as_deref(), "grok")
        .expect("resolve Grok CLI")
        .expect("Grok CLI on PATH or ELSE_GROK_BIN")
}

async fn ask(app: &TestApp, bot: &str, provider: &str, text: &str) -> (Uuid, String) {
    let reply = app.send(bot, Uuid::new_v4(), text, Some(provider)).await;
    assert!(reply.status.is_success(), "{}", reply.body);
    let turn = reply.body["turn"]["id"].as_str().unwrap();
    let status = app
        .wait_turn_for(
            turn,
            &["completed", "failed", "interrupted", "uncertain"],
            Duration::from_secs(180),
        )
        .await;
    let (answer, error): (String, Option<String>) = sqlx::query_as(
        "SELECT coalesce((SELECT group_concat(text, '') FROM items WHERE turn_id=t.id AND kind='assistant'), ''), t.error_message FROM turns t WHERE t.id=?1"
    ).bind(turn.parse::<Uuid>().unwrap()).fetch_one(&app.db.pool).await.unwrap();
    eprintln!("{provider}: {status}, {error:?}, {answer}");
    assert_eq!(status, "completed", "{error:?}");
    (turn.parse().unwrap(), answer)
}

async fn session(app: &TestApp, bot: &str) -> String {
    sqlx::query_scalar("SELECT native_session_id FROM provider_sessions WHERE bot_id=?1 AND provider_id='grok' AND status='active'")
        .bind(bot.parse::<Uuid>().unwrap()).fetch_one(&app.db.pool).await.unwrap()
}

fn grok(data: &Path, host: &Host, tool_url: &str) -> Grok {
    let provider = Grok::detect(
        Some(host.clone()),
        Some(installed()),
        grok_home::host_home(),
    )
    .with_tools(tool_url.into());
    assert_eq!(
        provider.project_dir(Uuid::nil()).unwrap(),
        data.join("projects").join(Uuid::nil().to_string())
    );
    provider
}

#[tokio::test(flavor = "multi_thread")]
async fn grok_conversations_stream_resume_use_host_and_mcp_tools_and_cancel() {
    if std::env::var("ELSE_LIVE_GROK").as_deref() != Ok("1") {
        eprintln!("SKIPPED: set ELSE_LIVE_GROK=1 to run live Grok checks");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().canonicalize().unwrap();
    let host = Host::new(&data, &data.join("projects"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/tools/v1/mcp", listener.local_addr().unwrap());
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let app = TestApp::with_registry(
        db,
        2,
        Registry::new(vec![
            Arc::new(grok(&data, &host, &url)),
            Arc::new(else_api::providers::synthetic::Synthetic::new(
                Duration::from_millis(5),
            )),
        ]),
    );
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
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(
        caps.body["providers"][0]["cli_available"], true,
        "{}",
        caps.body
    );
    let saved = app
        .owner(
            "PATCH",
            "/v1/settings/providers/grok",
            Some(json!({
                "model":"grok-4.7", "reasoning_effort":"low", "service_tier":"default"
            })),
        )
        .await;
    assert!(saved.status.is_success(), "{}", saved.body);
    let invalid = app
        .owner(
            "PATCH",
            "/v1/settings/providers/grok",
            Some(json!({
                "model":"grok-4.7", "reasoning_effort":"low", "service_tier":"priority"
            })),
        )
        .await;
    assert_eq!(invalid.body["error"]["code"], "invalid_profile");
    let a = app.create_bot("Grok Alpha").await;
    let b = app.create_bot("Grok Beta").await;
    let (_, answer) = ask(
        &app,
        &a,
        "grok",
        "We are preparing the ZETA-9 release. Remember that release name for later and acknowledge it in one sentence.",
    )
    .await;
    assert!(answer.contains("ZETA-9"));
    assert_eq!(
        answer.matches("ZETA-9").count(),
        1,
        "stream must not repeat whole assistant frames"
    );
    let first = session(&app, &a).await;
    let (turn, answer) = ask(
        &app,
        &a,
        "grok",
        "What release are we preparing? Reply with the release name.",
    )
    .await;
    assert!(answer.contains("ZETA-9"));
    assert_eq!(session(&app, &a).await, first, "native session resumed");
    let reported: Value = sqlx::query_scalar("SELECT reported_profile FROM turns WHERE id=?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(reported["model"], "grok-4.7");

    let (_, answer) = ask(&app, &a, "grok", "Use your shell to write exactly HOST_GROK_OK into grok-proof.txt in the current directory. Then run pwd and reply with the absolute directory.").await;
    let project = host.project_path(a.parse().unwrap());
    assert_eq!(
        std::fs::read_to_string(project.join("grok-proof.txt")).unwrap(),
        "HOST_GROK_OK"
    );
    assert!(answer.contains(project.to_str().unwrap()), "{answer}");
    let (_turn, answer) = ask(&app, &a, "grok", "Use the elsebot list_bots MCP tool to list the bots. Do not use the shell. Reply with the returned bot names.").await;
    assert!(answer.contains("Grok Beta"), "{answer}");
    let mut called = Vec::new();
    while let Ok(name) = observed_calls.try_recv() {
        called.push(name);
    }
    assert!(
        called.iter().any(|name| name == "list_bots"),
        "MCP call reached the real server: {called:?}"
    );
    ask(
        &app,
        &b,
        "grok",
        "We are preparing the BETA release. Acknowledge it in one sentence.",
    )
    .await;
    assert_ne!(
        session(&app, &b).await,
        first,
        "bots have separate sessions"
    );

    let sent = app
        .send(
            &a,
            Uuid::new_v4(),
            "Run `touch grok-sleep-started; sleep 120` in the shell and wait until it finishes. Then reply DONE.",
            Some("grok"),
        )
        .await;
    let interrupted = sent.body["turn"]["id"].as_str().unwrap();
    app.wait_turn_for(interrupted, &["running"], Duration::from_secs(90))
        .await;
    tokio::time::timeout(Duration::from_secs(90), async {
        while !project.join("grok-sleep-started").exists() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("Grok started the actual shell command");
    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{a}/cancel"),
            Some(json!({"request_id":Uuid::new_v4()})),
        )
        .await;
    assert!(stopped.status.is_success(), "{}", stopped.body);
    let status = app
        .wait_turn_for(
            interrupted,
            &["completed", "interrupted", "failed", "uncertain"],
            Duration::from_secs(45),
        )
        .await;
    let error: Option<String> = sqlx::query_scalar("SELECT error_message FROM turns WHERE id=?1")
        .bind(interrupted.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "interrupted", "{error:?}");
    assert!(
        host.live_processes(interrupted.parse().unwrap()).is_empty(),
        "provider and shell reaped"
    );
    let (_, answer) = ask(
        &app,
        &a,
        "grok",
        "What release are we preparing? Reply with the release name.",
    )
    .await;
    assert!(
        answer.contains("ZETA-9"),
        "history rebuilt after interruption"
    );
    assert_ne!(
        session(&app, &a).await,
        first,
        "interrupted session retired"
    );

    ask(&app, &a, "synthetic", "Provider handoff test.").await;
    let (_, answer) = ask(
        &app,
        &a,
        "grok",
        "What release are we preparing? Reply with the release name.",
    )
    .await;
    assert!(
        answer.contains("ZETA-9"),
        "history survives provider switching"
    );
    server.abort();
}
