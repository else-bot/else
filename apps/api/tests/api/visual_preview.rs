use crate::common;

use std::path::PathBuf;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, Harness, harness, tool};
use else_api::grants::Tool;
use serde_json::json;

async fn project(root: PathBuf) -> (Harness, String, PathBuf) {
    let h = harness(TestApp::builder().projects(root.clone())).await;
    let bot = h.app.main_bot().await;
    let project = root.join(&bot);
    std::fs::create_dir_all(project.join("viz")).unwrap();
    (h, bot, project)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_reads_the_rules_then_sees_its_visual_as_the_chat_renders_it() {
    let directory = tempfile::tempdir().unwrap();
    let (h, bot, project) = project(directory.path().to_owned()).await;
    let (_, token) = h.running(&bot, GATE).await;

    let rules = tool(&h.app, &token, "visual_preview", json!({})).await;
    assert_eq!(rules.status, StatusCode::OK, "{}", rules.body);
    let text = rules.result["rules"].as_str().unwrap();
    for color in else_visual::Color::ALL {
        assert!(text.contains(&format!("--{}", color.name())), "{text}");
    }

    std::fs::write(
        project.join("viz/digest.html"),
        r#"<div style="height: 90px; background: var(--card)"><img src="https://x.test/a.png"></div>"#,
    )
    .unwrap();
    let shown = tool(
        &h.app,
        &token,
        "visual_preview",
        json!({ "path": "viz/digest.html", "width": 400 }),
    )
    .await;
    let content = &shown.body["result"]["content"];
    let meta = &shown.result;
    assert_eq!(meta["content_height"], 90, "{meta}");
    assert_eq!(meta["blocked"], json!(["https://x.test/a.png"]));
    assert_eq!(content[1]["mimeType"], "image/png");

    let outside = tool(
        &h.app,
        &token,
        "visual_preview",
        json!({ "path": "../elsewhere.html" }),
    )
    .await;
    assert!(outside.error, "{}", outside.body);
    let not_html = tool(
        &h.app,
        &token,
        "visual_preview",
        json!({ "path": "viz/notes.txt" }),
    )
    .await;
    assert!(not_html.error, "{}", not_html.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_without_the_computer_cannot_preview_project_files() {
    let directory = tempfile::tempdir().unwrap();
    let (h, bot, _) = project(directory.path().to_owned()).await;
    let (turn, _) = h.running(&bot, GATE).await;
    let token = h.grant_with(turn, vec![Tool::ReadMemory]).await;
    let reply = tool(
        &h.app,
        &token,
        "visual_preview",
        json!({ "path": "viz/digest.html" }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.body);
}
