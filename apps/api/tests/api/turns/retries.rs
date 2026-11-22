//! The owner's retry of a failed message: a new attempt of the same turn.

use super::*;

use base64::Engine;

async fn retry(app: &TestApp, bot: &str, turn: &str, attempt: i64) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/turns/{turn}/retry"),
        Some(json!({ "attempt": attempt })),
    )
    .await
}

async fn turn_view(app: &TestApp, bot: &str, request: &str) -> serde_json::Value {
    app.owner(
        "GET",
        &format!("/v1/bots/{bot}/turns/by-request/{request}"),
        None,
    )
    .await
    .body["turn"]
        .clone()
}

async fn failed(app: &TestApp, bot: &str, text: &str) -> (String, String) {
    let sent = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    assert_eq!(app.wait_turn(&turn, &["failed"]).await, "failed");
    let request = sent.body["turn"]["request_id"].as_str().unwrap().to_owned();
    (turn, request)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_runs_the_failed_message_again_with_its_files() {
    let h = recorder::harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let file = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/attachments"),
            Some(json!({
                "id": Uuid::new_v4(), "name": "notes.txt", "media_type": "text/plain",
                "data": base64::engine::general_purpose::STANDARD.encode(b"todo\n"),
            })),
        )
        .await
        .body;
    let text = "[synthetic:fail] read [File: notes.txt]";
    let parts = json!([
        { "type": "text", "text": "[synthetic:fail] read " },
        { "type": "file", "file": file },
    ]);
    let sent = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/turns"),
            Some(json!({ "request_id": Uuid::new_v4(), "text": text, "parts": parts })),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    let request = sent.body["turn"]["request_id"].as_str().unwrap().to_owned();
    assert_eq!(app.wait_turn(&turn, &["failed"]).await, "failed");
    let view = turn_view(app, &bot, &request).await;
    assert_eq!(view["retryable"], true, "{view}");
    assert_eq!(view["attempt"], 1);

    let retried = retry(app, &bot, &turn, 1).await;
    assert_eq!(retried.status, StatusCode::ACCEPTED, "{}", retried.body);
    assert_eq!(retried.body["id"], turn.as_str());
    assert_eq!(retried.body["status"], "queued");
    assert_eq!(retried.body["error"], json!(null));
    assert_eq!(retried.body["retryable"], false);
    common::until(
        "the retried attempt to fail",
        common::poll(|| async {
            let view = turn_view(app, &bot, &request).await;
            (view["attempt"] == 2 && view["status"] == "failed").then_some(())
        }),
    )
    .await;

    let repeat = retry(app, &bot, &turn, 1).await;
    assert_eq!(repeat.status, StatusCode::OK, "{}", repeat.body);
    assert_eq!(repeat.body["attempt"], 2, "a repeat starts no attempt");

    let inputs = h.inputs(turn.parse().unwrap()).await;
    assert_eq!(inputs.len(), 2);
    for input in &inputs {
        assert_eq!(input.text, text);
        assert_eq!(serde_json::to_value(&input.parts).unwrap(), parts);
        let names: Vec<&str> = input
            .files
            .iter()
            .map(|f| f.attachment.name.as_str())
            .collect();
        assert_eq!(names, ["notes.txt"]);
    }
    let owner_items = items(app, &bot)
        .await
        .into_iter()
        .filter(|(_, kind, _)| kind == "user")
        .count();
    assert_eq!(owner_items, 1, "the owner message is not duplicated");
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_latest_message_after_a_failure_that_may_pass_retries() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;

    let (policy, policy_request) = failed(&app, &bot, "[synthetic:policy]").await;
    assert_eq!(
        turn_view(&app, &bot, &policy_request).await["retryable"],
        false
    );
    let refused = retry(&app, &bot, &policy, 1).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "not_retryable");

    let (older, older_request) = failed(&app, &bot, "[synthetic:fail]").await;
    let later = app
        .send(&bot, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    let later = later.body["turn"]["id"].as_str().unwrap().to_owned();
    let published: String = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE kind = 'turn.updated'
           AND json_extract(data, '$.turn.id') = ?1 ORDER BY seq DESC LIMIT 1",
    )
    .bind(&older)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let published: serde_json::Value = serde_json::from_str(&published).unwrap();
    assert_eq!(
        published["turn"]["retryable"], false,
        "a newer message republishes the older failure without Retry"
    );
    assert_eq!(app.wait_turn(&later, &["completed"]).await, "completed");
    assert_eq!(
        turn_view(&app, &bot, &older_request).await["retryable"],
        false
    );
    let refused = retry(&app, &bot, &older, 1).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);

    let refused = retry(&app, &bot, &later, 1).await;
    assert_eq!(
        refused.status,
        StatusCode::CONFLICT,
        "a completed turn does not retry"
    );

    let missing = retry(&app, &bot, &Uuid::new_v4().to_string(), 1).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failure_after_an_approved_action_ran_does_not_retry() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let (turn, request) = failed(&app, &bot, "[synthetic:fail]").await;
    sqlx::query(
        "INSERT INTO owner_questions (id, bot_id, turn_id, attempt, kind, title, choices,
             action, action_hash, status, answer, used_at)
         VALUES (?1, ?2, ?3, 1, 'destructive', 'Delete merged branches', '[]',
                 '{\"host\":\"Delete merged branches\"}', zeroblob(32), 'answered', 'approve',
                 strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))",
    )
    .bind(Uuid::new_v4())
    .bind(bot.parse::<Uuid>().unwrap())
    .bind(turn.parse::<Uuid>().unwrap())
    .execute(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(turn_view(&app, &bot, &request).await["retryable"], false);
    let refused = retry(&app, &bot, &turn, 1).await;
    assert_eq!(refused.status, StatusCode::CONFLICT, "{}", refused.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_asks_again_what_the_failed_attempt_left_cancelled() {
    let h = recorder::harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let ask = json!({ "question": "Which template?", "choices": ["Plain", "HTML"] });
    let text = format!("{} [synthetic:fail]", recorder::GATE_EVERY_ATTEMPT);
    let (turn, token) = h.running(&bot, &text).await;
    let first = recorder::tool(app, &token, "ask_owner", ask.clone()).await;
    assert_eq!(first.result["status"], "pending", "{}", first.body);
    h.release(turn).await;
    assert_eq!(app.wait_turn(turn, &["failed"]).await, "failed");

    let retried = retry(app, &bot, &turn.to_string(), 1).await;
    assert_eq!(retried.status, StatusCode::ACCEPTED, "{}", retried.body);
    let token = h.continued(turn, &token).await;
    let again = recorder::tool(app, &token, "ask_owner", ask).await;
    assert_eq!(again.result["status"], "pending", "{}", again.body);
    assert_ne!(again.result["question_id"], first.result["question_id"]);
    h.release(turn).await;
}
