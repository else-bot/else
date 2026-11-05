use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use axum::extract::State;
use axum::http::header;
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::config;
use crate::providers::codex_home;
use crate::providers::codex_rpc::{AppServer, Incoming};
use crate::providers::host::{Host, LaunchSpec};

fn install_skill(root: &Path, name: &str) {
    let dir = root.join(".agents/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} fixture.\n---\nReply OK.\n"),
    )
    .unwrap();
}

async fn respond(
    State(captured): State<mpsc::Sender<Value>>,
    axum::Json(body): axum::Json<Value>,
) -> impl axum::response::IntoResponse {
    captured.send(body).await.unwrap();
    (
        [(header::CONTENT_TYPE, "text/event-stream")],
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n",
    )
}

#[tokio::test]
async fn only_bot_project_skills_reach_new_and_resumed_model_context() {
    let Some(program) = std::env::var_os("ELSE_CODEX_BIN") else {
        eprintln!("SKIPPED: set ELSE_CODEX_BIN to check installed Codex without inference");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("owner-home");
    let projects = dir.path().join("projects");
    std::fs::create_dir(&projects).unwrap();
    let linked = dir.path().join("linked-projects");
    std::os::unix::fs::symlink(&projects, &linked).unwrap();
    let host = Host::new(&dir.path().join("data"), &linked);
    let dirs = host.bot_dirs(uuid::Uuid::new_v4(), "codex").unwrap();
    std::fs::create_dir(projects.join(".git")).unwrap();
    install_skill(&home, "owner-home-fixture");
    install_skill(&projects, "ancestor-fixture");
    install_skill(&dirs.project, "bot-project-fixture");
    let auth = dir.path().join("auth.json");
    std::fs::write(&auth, "{}").unwrap();
    codex_home::prepare(&dirs, &auth, None, true).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (captured, mut requests) = mpsc::channel(4);
    let router = axum::Router::new()
        .route("/responses", post(respond))
        .with_state(captured);
    let fixture = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let config_path = dirs.provider_home.join("config.toml");
    let mut text = std::fs::read_to_string(&config_path).unwrap();
    text.insert_str(0, "model_provider = \"fixture\"\n");
    text.push_str(&format!(
        "\n[model_providers.fixture]\nname = \"Fixture\"\n\
         base_url = \"http://127.0.0.1:{port}\"\nwire_api = \"responses\"\n\
         requires_openai_auth = false\n"
    ));
    std::fs::write(config_path, text).unwrap();
    let spec = LaunchSpec {
        turn_id: uuid::Uuid::new_v4(),
        program: program.into(),
        args: vec![OsString::from("app-server")],
        env: vec![
            ("HOME".into(), home.to_string_lossy().into_owned()),
            (
                "CODEX_HOME".into(),
                dirs.provider_home.to_string_lossy().into_owned(),
            ),
        ],
        dirs,
    };
    let mut server = AppServer::start(&host, &spec).await.unwrap();
    let overrides = config(&server).await.unwrap();
    let settings = json!({
        "model": "gpt-6.1-sol", "cwd": server.cwd,
        "approvalPolicy": "never", "sandbox": "read-only",
    });
    let baseline = server
        .request("thread/start", settings.clone(), Duration::from_secs(30))
        .await
        .unwrap();
    let baseline_id = baseline["thread"]["id"].as_str().unwrap();
    let input = capture_turn(&mut server, &mut requests, baseline_id).await;
    assert!(
        input.contains("owner-home-fixture"),
        "fixture must reproduce HOME inheritance"
    );
    assert!(
        input.contains("ancestor-fixture"),
        "fixture must reproduce ancestor inheritance"
    );
    assert!(input.contains("bot-project-fixture"));

    let mut settings = settings;
    settings["config"] = overrides;
    let started = server
        .request("thread/start", settings.clone(), Duration::from_secs(30))
        .await
        .unwrap();
    let thread_id = started["thread"]["id"].as_str().unwrap();
    let input = capture_turn(&mut server, &mut requests, thread_id).await;
    assert!(!input.contains("owner-home-fixture"));
    assert!(!input.contains("ancestor-fixture"));
    assert!(input.contains("bot-project-fixture"));
    assert!(!input.contains("skill-creator"));
    assert!(!input.contains("skill-installer"));

    settings["threadId"] = json!(thread_id);
    server
        .request("thread/resume", settings, Duration::from_secs(30))
        .await
        .unwrap();
    let input = capture_turn(&mut server, &mut requests, thread_id).await;
    assert!(!input.contains("owner-home-fixture"));
    assert!(!input.contains("ancestor-fixture"));
    assert!(input.contains("bot-project-fixture"));
    server.shutdown(Duration::from_secs(5)).await;
    fixture.abort();
}

async fn capture_turn(
    server: &mut AppServer,
    requests: &mut mpsc::Receiver<Value>,
    thread_id: &str,
) -> String {
    server
        .request(
            "turn/start",
            json!({ "threadId": thread_id, "input": [{ "type": "text", "text": "Reply OK." }] }),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let request = tokio::time::timeout(Duration::from_secs(30), requests.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(message) = server.next().await {
            if let Incoming::Notification { method, params } = message
                && method == "turn/completed"
            {
                assert_eq!(params["turn"]["status"], "completed");
                return;
            }
        }
        panic!("Codex closed before completing the fixture turn");
    })
    .await
    .unwrap();
    request["input"].to_string()
}
