//! Accepting a send: retries, conflicts, and the items a turn records.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn send_retry_and_conflict_create_one_message() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let request = Uuid::new_v4();
    let first = app
        .send(&bot, request, "Hello there", Some("synthetic"))
        .await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.body);
    assert_eq!(first.body["item"]["kind"], "user");
    assert_eq!(first.body["item"]["text"], "Hello there");
    assert_eq!(first.body["item"]["request_id"], request.to_string());
    assert_eq!(first.body["turn"]["status"], "queued");
    assert!(first.body["turn"]["started_at"].is_null());
    assert_eq!(first.body["turn"]["provider_id"], "synthetic");

    let retry = app
        .send(&bot, request, "Hello there", Some("synthetic"))
        .await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["item"]["id"], first.body["item"]["id"]);
    assert_eq!(retry.body["turn"]["id"], first.body["turn"]["id"]);

    for (text, provider) in [
        ("Changed", Some("synthetic")),
        ("Hello there", Some("codex")),
    ] {
        let conflict = app.send(&bot, request, text, provider).await;
        assert_eq!(conflict.status, StatusCode::CONFLICT);
        assert_eq!(conflict.body["error"]["code"], "request_conflict");
    }
    let lookup = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/turns/by-request/{request}"),
            None,
        )
        .await;
    assert_eq!(lookup.status, StatusCode::OK);
    assert_eq!(lookup.body["turn"]["id"], first.body["turn"]["id"]);
    let missing = app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/turns/by-request/{}", Uuid::new_v4()),
            None,
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);

    let turn = first.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let settled = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn)
        .unwrap();
    let started: chrono::DateTime<chrono::Utc> =
        settled["started_at"].as_str().unwrap().parse().unwrap();
    let finished: chrono::DateTime<chrono::Utc> =
        settled["finished_at"].as_str().unwrap().parse().unwrap();
    assert!(finished >= started);
    let history = items(&app, &bot).await;
    assert_eq!(
        history.len(),
        2,
        "one owner bubble and one assistant bubble"
    );
    assert_eq!(history[0], (1, "user".into(), "Hello there".into()));
    assert_eq!(history[1].1, "assistant");
    assert_eq!(
        history[1].2,
        "Synthetic reply ✓ from Assistant (turn 1): Hello there"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn each_agent_message_is_its_own_item() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let sent = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:messages]",
            Some("synthetic"),
        )
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");
    let history = items(&app, &bot).await;
    let replies: Vec<&str> = history[1..]
        .iter()
        .map(|(_, _, text)| text.as_str())
        .collect();
    assert_eq!(
        replies,
        vec![
            "Synthetic reply ✓ from Assistant (turn 1): [synthetic:messages]",
            else_api::providers::synthetic::SECOND_MESSAGE,
        ],
        "one assistant item per agent message"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_duplicate_sends_accept_once() {
    let app = Arc::new(TestApp::installed().await);
    let bot = app.main_bot().await;
    let request = Uuid::new_v4();
    let calls: Vec<_> = Vec::from_fn(6, |_| {
        let (app, bot) = (app.clone(), bot.clone());
        tokio::spawn(async move {
            app.send(&bot, request, "once", Some("synthetic"))
                .await
                .status
        })
    });
    let mut statuses = Vec::new();
    for call in calls {
        statuses.push(call.await.unwrap());
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::ACCEPTED)
            .count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::ACCEPTED || *s == StatusCode::OK)
    );
    assert_eq!(turn_count(&app).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_acceptance_publishes_nothing() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    sqlx::raw_sql(
        "CREATE TRIGGER fail_item BEFORE INSERT ON items WHEN NEW.text = 'trigger-failure' BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .execute(&app.db.pool)
    .await
    .unwrap();
    let request = Uuid::new_v4();
    let reply = app
        .send(&bot, request, "trigger-failure", Some("synthetic"))
        .await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(reply.body["error"]["code"], "internal");
    assert_eq!(turn_count(&app).await, 0);
    assert!(items(&app, &bot).await.is_empty());
    let (last_turn, last_seq, selected): (i64, i64, Option<String>) = sqlx::query_as(
        "SELECT c.last_turn_no, c.last_item_seq, b.selected_provider_id
         FROM conversations c JOIN bots b ON b.id = c.bot_id WHERE b.id = ?1",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        (last_turn, last_seq, selected),
        (0, 0, None),
        "no counter or picker change"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_and_rejected_sends_create_nothing() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    for (text, provider, status) in [
        ("   ", Some("synthetic"), StatusCode::BAD_REQUEST),
        (
            &*"x".repeat(32 * 1024 + 1),
            Some("synthetic"),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
        ("nul\u{0}", Some("synthetic"), StatusCode::BAD_REQUEST),
        ("hi", Some("nope"), StatusCode::BAD_REQUEST),
    ] {
        let reply = app.send(&bot, Uuid::new_v4(), text, provider).await;
        assert_eq!(reply.status, status);
    }
    let unknown = app
        .send(
            &Uuid::new_v4().to_string(),
            Uuid::new_v4(),
            "hi",
            Some("synthetic"),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    let deleted = app.create_bot("Gone").await;
    app.owner("DELETE", &format!("/v1/bots/{deleted}"), None)
        .await;
    let reply = app
        .send(&deleted, Uuid::new_v4(), "hello?", Some("synthetic"))
        .await;
    assert_eq!(reply.status, StatusCode::GONE);
    assert_eq!(reply.body["error"]["code"], "bot_deleted");
    assert_eq!(turn_count(&app).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn retry_succeeds_after_provider_removed_and_patch_needs_adapter() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let request = Uuid::new_v4();
    let first = app
        .send(&bot, request, "before removal", Some("synthetic"))
        .await;
    app.wait_turn(first.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let db = app.db.clone();
    drop(app);
    let app = TestApp::builder().db(db).providers(vec![]).start().await;
    let retry = app
        .send(&bot, request, "before removal", Some("synthetic"))
        .await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["turn"]["id"], first.body["turn"]["id"]);
    let fresh = app
        .send(&bot, Uuid::new_v4(), "new", Some("synthetic"))
        .await;
    assert_eq!(fresh.status, StatusCode::BAD_REQUEST);
    let none = app.send(&bot, Uuid::new_v4(), "new", None).await;
    assert_eq!(none.status, StatusCode::SERVICE_UNAVAILABLE);
    let patch = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "high" })),
        )
        .await;
    assert_eq!(patch.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(patch.body["error"]["code"], "provider_unavailable");
    let get = app.owner("GET", "/v1/settings/providers/codex", None).await;
    assert_eq!(get.body["profile"]["reasoning_effort"], "medium");
}
