use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::Value;
use uuid::Uuid;

async fn say(app: &TestApp, bot: &str, text: &str) {
    let sent = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{:?}", sent.body);
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");
}

async fn shown(app: &TestApp, bot: &str) -> Vec<Value> {
    let snapshot = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/conversation?limit=200"),
            None,
        )
        .await;
    snapshot.body["items"].as_array().unwrap().clone()
}

fn seqs(items: &Value) -> Vec<i64> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["seq"].as_i64().unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn search_counts_rendered_matches_newest_first() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    say(&app, &bot, "Plan the **Café** launch").await;
    say(
        &app,
        &bot,
        "Replying to my earlier message:\n> Plan the cafe launch\n\nNothing to see",
    )
    .await;
    say(
        &app,
        &bot,
        "Read [the notes](https://cafe.example.com) on cafe and CAFE",
    )
    .await;
    let items = shown(&app, &bot).await;
    let id = |text: &str, kind: &str| {
        items
            .iter()
            .find(|item| item["kind"] == kind && item["text"].as_str().unwrap().contains(text))
            .map(|item| item["id"].clone())
            .unwrap()
    };

    let found = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/conversation/search?q=caf%C3%A9"),
            None,
        )
        .await;
    assert_eq!(found.status, StatusCode::OK, "{:?}", found.body);
    assert_eq!(
        found.body["total"], 7,
        "the quote in the owner reply is folded away"
    );
    let hits: Vec<(Value, i64)> = found.body["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| (hit["item_id"].clone(), hit["count"].as_i64().unwrap()))
        .collect();
    assert_eq!(
        hits,
        [
            (id("Read [the notes]", "assistant"), 2),
            (id("Read [the notes]", "user"), 2),
            (id("Replying to", "assistant"), 1),
            (id("Plan the", "assistant"), 1),
            (id("Plan the", "user"), 1),
        ]
    );

    let blank = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/conversation/search?q=%20%20"),
            None,
        )
        .await;
    assert_eq!(blank.body["total"], 0);
    assert_eq!(blank.body["hits"], serde_json::json!([]));

    let long = "a".repeat(201);
    let rejected = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/conversation/search?q={long}"),
            None,
        )
        .await;
    assert_eq!(rejected.status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_window_loads_around_a_message_and_counts_what_is_still_missing() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    for n in 1..=6 {
        say(&app, &bot, &format!("message {n}")).await;
    }
    let all: Vec<i64> = shown(&app, &bot)
        .await
        .iter()
        .map(|item| item["seq"].as_i64().unwrap())
        .collect();
    assert_eq!(all.len(), 12, "each message and its echo");

    let window = app
        .owner(
            "GET",
            &format!(
                "/v1/bots/{bot}/conversation/window?anchor_seq={}&older=2&newer=1&before_seq={}",
                all[4], all[10]
            ),
            None,
        )
        .await;
    assert_eq!(window.status, StatusCode::OK, "{:?}", window.body);
    assert_eq!(seqs(&window.body["items"]), &all[3..6]);
    assert_eq!(window.body["older_gap"], 3);
    assert_eq!(window.body["newer_gap"], 4);
    assert_eq!(window.body["turns"].as_array().unwrap().len(), 2);

    let gap = app
        .owner(
            "GET",
            &format!(
                "/v1/bots/{bot}/conversation/window?anchor_seq={}&older=50&newer=0&after_seq={}&before_seq={}",
                all[10] - 1,
                all[5],
                all[10]
            ),
            None,
        )
        .await;
    assert_eq!(seqs(&gap.body["items"]), &all[6..10]);
    assert_eq!(gap.body["older_gap"], 0);
    assert_eq!(gap.body["newer_gap"], 0);

    let crossed = app
        .owner(
            "GET",
            &format!(
                "/v1/bots/{bot}/conversation/window?anchor_seq=1&after_seq={}&before_seq={}",
                all[5], all[5]
            ),
            None,
        )
        .await;
    assert_eq!(crossed.status, StatusCode::BAD_REQUEST);
}
