//! Owner reactions to assistant messages.

use super::*;

async fn react(
    app: &TestApp,
    bot: &str,
    request: Uuid,
    item: &serde_json::Value,
    kind: &str,
) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/turns"),
        Some(json!({
            "request_id": request,
            "text": "👍 Feedback on your earlier message",
            "provider_id": "synthetic",
            "reaction": { "item_id": item, "kind": kind },
        })),
    )
    .await
}

async fn exchange(app: &TestApp, bot: &str) -> (serde_json::Value, serde_json::Value) {
    let sent = app
        .send(bot, Uuid::new_v4(), "Hello there", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");
    let assistant: Uuid = sqlx::query_scalar(
        "SELECT id FROM items WHERE turn_id = ?1 AND kind = 'assistant' ORDER BY seq",
    )
    .bind(turn.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    (sent.body["item"]["id"].clone(), json!(assistant))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_is_saved_with_its_feedback_message() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let (_, assistant) = exchange(&app, &bot).await;
    let request = Uuid::new_v4();
    let sent = react(&app, &bot, request, &assistant, "thumbs_up").await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{:?}", sent.body);
    let expected = json!({ "item_id": assistant, "kind": "thumbs_up" });
    assert_eq!(sent.body["item"]["reaction"], expected);
    assert_eq!(sent.body["turn"]["reaction"], expected);
    let feedback = sent.body["item"]["id"].clone();
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");

    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let items = snapshot.body["items"].as_array().unwrap();
    let saved = items.iter().find(|i| i["id"] == feedback).unwrap();
    assert_eq!(saved["reaction"], expected);
    assert_eq!(saved["text"], "👍 Feedback on your earlier message");
    let page = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation?limit=1"), None)
        .await;
    assert_eq!(page.body["items"].as_array().unwrap().len(), 1);
    assert_eq!(page.body["items"][0]["kind"], "assistant");
    assert_eq!(page.body["turns"][0]["reaction"], expected);
    assert!(
        items
            .iter()
            .filter(|i| i["id"] != feedback)
            .all(|i| i["reaction"].is_null()),
        "only the feedback message carries a reaction"
    );

    let upsert: String = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE kind = 'item.upsert' AND json_extract(data, '$.item.id') = ?1",
    )
    .bind(feedback.as_str().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let upsert: serde_json::Value = serde_json::from_str(&upsert).unwrap();
    assert_eq!(upsert["item"]["reaction"], expected);

    let lookup = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/turns/by-request/{request}"),
            None,
        )
        .await;
    assert_eq!(lookup.body["item"]["reaction"], expected);
    assert_eq!(lookup.body["turn"]["reaction"], expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_retry_must_repeat_the_same_reaction() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let (_, assistant) = exchange(&app, &bot).await;
    let request = Uuid::new_v4();
    let first = react(&app, &bot, request, &assistant, "thumbs_down").await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);

    let retry = react(&app, &bot, request, &assistant, "thumbs_down").await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["item"]["id"], first.body["item"]["id"]);
    assert_eq!(retry.body["item"]["reaction"]["kind"], "thumbs_down");

    let changed = react(&app, &bot, request, &assistant, "thumbs_up").await;
    assert_eq!(changed.status, StatusCode::CONFLICT);
    assert_eq!(changed.body["error"]["code"], "request_conflict");
    let without = app
        .send(
            &bot,
            request,
            "👍 Feedback on your earlier message",
            Some("synthetic"),
        )
        .await;
    assert_eq!(without.status, StatusCode::CONFLICT);
    assert_eq!(without.body["error"]["code"], "request_conflict");
    assert_eq!(turn_count(&app).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_must_target_an_assistant_message_of_the_same_bot() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let other = app.create_bot("Other").await;
    let (owner_item, _) = exchange(&app, &bot).await;
    let (_, others_reply) = exchange(&app, &other).await;
    let before = turn_count(&app).await;

    for target in [owner_item, others_reply, json!(Uuid::new_v4())] {
        let refused = react(&app, &bot, Uuid::new_v4(), &target, "thumbs_up").await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{target}");
        assert_eq!(refused.body["error"]["code"], "invalid_reaction");
    }
    let unknown_kind = react(&app, &bot, Uuid::new_v4(), &json!(Uuid::new_v4()), "heart").await;
    assert_eq!(unknown_kind.status, StatusCode::BAD_REQUEST);
    assert_eq!(unknown_kind.body["error"]["code"], "invalid_body");
    assert_eq!(turn_count(&app).await, before, "nothing was accepted");
}
