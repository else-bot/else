use crate::common;

use std::path::PathBuf;
use std::sync::Arc;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::app::providers::{
    Availability, Profile, Provider, Stop, TurnInput, TurnOutcome, TurnSink,
};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

/// The synthetic provider, with a project directory under `root`.
struct WithProject {
    inner: Synthetic,
    root: PathBuf,
}

#[async_trait::async_trait]
impl Provider for WithProject {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn label(&self) -> &'static str {
        self.inner.label()
    }
    fn availability(&self) -> Availability {
        self.inner.availability()
    }
    fn uses_profile(&self) -> bool {
        self.inner.uses_profile()
    }
    fn validate_profile(&self, profile: &Profile) -> Result<(), String> {
        self.inner.validate_profile(profile)
    }
    fn project_dir(&self, bot: Uuid) -> Option<PathBuf> {
        Some(self.root.join(bot.to_string()))
    }
    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome {
        self.inner.run(input, sink, stop).await
    }
}

async fn setup(root: PathBuf, tools: Vec<Tool>) -> (TestApp, PathBuf, String) {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let registry = else_api::providers::Registry::new(vec![Arc::new(WithProject {
        inner: Synthetic::new(std::time::Duration::from_millis(5)),
        root: root.clone(),
    })]);
    let app = TestApp::with_registry(Arc::new(db), 4, registry);
    let bot = app.main_bot().await;
    let reply = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let token = app
        .scheduler
        .grants()
        .issue(bot.parse().unwrap(), conversation, turn, tools, DEFAULT_TTL)
        .await;
    let project = root.join(&bot);
    std::fs::create_dir_all(project.join("viz")).unwrap();
    (app, project, token)
}

async fn preview(app: &TestApp, token: &str, arguments: Value) -> common::Reply {
    app.call(
        "POST",
        "/tools/v1/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": { "name": "visual_preview", "arguments": arguments } })),
        Some(token),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_reads_the_rules_then_sees_its_visual_as_the_chat_renders_it() {
    let directory = tempfile::tempdir().unwrap();
    let (app, project, token) = setup(directory.path().to_owned(), vec![Tool::Computer]).await;

    let rules = preview(&app, &token, json!({})).await;
    assert_eq!(rules.status, StatusCode::OK, "{}", rules.body);
    let text = rules.body["result"]["content"][0]["text"].as_str().unwrap();
    for color in else_visual::Color::ALL {
        assert!(text.contains(&format!("--{}", color.name())), "{text}");
    }

    std::fs::write(
        project.join("viz/digest.html"),
        r#"<div style="height: 90px; background: var(--card)"><img src="https://x.test/a.png"></div>"#,
    )
    .unwrap();
    let shown = preview(
        &app,
        &token,
        json!({ "path": "viz/digest.html", "width": 400 }),
    )
    .await;
    let content = &shown.body["result"]["content"];
    let meta: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(meta["content_height"], 90, "{meta}");
    assert_eq!(meta["blocked"], json!(["https://x.test/a.png"]));
    assert_eq!(content[1]["mimeType"], "image/png");

    let outside = preview(&app, &token, json!({ "path": "../elsewhere.html" })).await;
    assert_eq!(outside.body["result"]["isError"], true, "{}", outside.body);
    let not_html = preview(&app, &token, json!({ "path": "viz/notes.txt" })).await;
    assert_eq!(
        not_html.body["result"]["isError"], true,
        "{}",
        not_html.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_without_the_computer_cannot_preview_project_files() {
    let directory = tempfile::tempdir().unwrap();
    let (app, _, token) = setup(directory.path().to_owned(), vec![Tool::ReadMemory]).await;
    let reply = preview(&app, &token, json!({ "path": "viz/digest.html" })).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
}
