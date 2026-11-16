use uuid::Uuid;

use crate::common::TestApp;

#[tokio::test(flavor = "multi_thread")]
async fn usage_reports_each_bot_against_the_window_its_provider_reported() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let sent = app
        .send(&bot, Uuid::new_v4(), "[synthetic:usage]", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");

    let report = app.owner("GET", "/v1/usage", None).await.body;

    let provider = report["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["provider_id"] == "synthetic")
        .expect("the provider that ran the turn is listed");
    let window = &provider["windows"][0];
    assert_eq!(window["minutes"], 300);
    let attribution = &window["attribution"];
    // One reading can't teach what a token costs, so the total stays
    // unattributed and bots are shown by share of else.bot's use.
    assert_eq!(attribution["state"], "unattributed");
    assert_eq!(attribution["used"], 12.0);
    assert!(attribution["observed_at"].is_string());
    assert_eq!(window["buckets"].as_array().unwrap().len(), 20);
    let entry = &window["bots"][0];
    assert_eq!(entry["bot_id"], bot.as_str());
    assert_eq!(entry["turns"], 1);
    assert_eq!(entry["input_tokens"], 2000);
    assert_eq!(entry["output_tokens"], 30);
    assert_eq!(entry["share"], 1.0);
    assert_eq!(entry["sources"][0]["source"], "chat");
    assert_eq!(entry["percent"], 100.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn usage_is_empty_before_any_turn_reports_tokens() {
    let app = TestApp::installed().await;
    let report = app.owner("GET", "/v1/usage", None).await;
    assert_eq!(report.status, axum::http::StatusCode::OK);
    assert_eq!(report.body["providers"], serde_json::json!([]));
}
