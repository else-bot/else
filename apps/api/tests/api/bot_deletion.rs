use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, Harness, harness, tool};
use serde_json::{Value, json};
use uuid::Uuid;

async fn status(app: &TestApp, bot: &str) -> String {
    sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn card(app: &TestApp, question: &str) -> (Value, Value, String, Option<String>) {
    let (choices, action, context, outcome): (String, String, String, Option<String>) =
        sqlx::query_as(
            "SELECT choices, action, context, use_outcome FROM owner_questions WHERE id = ?1",
        )
        .bind(question.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    (
        serde_json::from_str(&choices).unwrap(),
        serde_json::from_str(&action).unwrap(),
        context,
        outcome,
    )
}

async fn answer(app: &TestApp, bot: &str, question: &str, choice: &str) {
    let answered = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({ "choice": choice })),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
}

async fn with_routine(h: &Harness, bot: &str) {
    let (turn, token) = h.running(bot, GATE).await;
    let created = tool(
        &h.app,
        &token,
        "routine_create",
        json!({ "name": "X Ads budget", "prompt": "Check the X Ads budget.", "cron": "0 9 1 1 *" }),
    )
    .await;
    assert!(!created.error, "{}", created.body);
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_card_deletes_the_bot_once_even_with_a_broad_grant() {
    let h = harness(common::TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let growth = h.app.create_bot("Growth").await;
    with_routine(&h, &growth).await;
    sqlx::query("INSERT INTO bot_permissions (bot_id, scope) VALUES (?1, 'destructive')")
        .bind(main.parse::<Uuid>().unwrap())
        .execute(&h.app.db.pool)
        .await
        .unwrap();

    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let args = json!({ "target_bot_id": growth, "reason": "Marketing Manager owns its work now." });
    let pending = tool(&h.app, &token, "delete_bot", args.clone()).await;
    assert!(!pending.error, "{}", pending.body);
    assert_eq!(pending.result["status"], "pending");
    assert_eq!(pending.result["parked"], true);
    let question = pending.result["question_id"].as_str().unwrap().to_owned();
    let (choices, action, context, _) = card(&h.app, &question).await;
    assert_eq!(
        choices,
        json!([{ "id": "approve", "label": "Approve once" }, { "id": "deny", "label": "Deny" }]),
        "a saved grant is never offered or used"
    );
    assert_eq!(action["bot_deletion"]["bot_id"], growth);
    assert_eq!(action["bot_deletion"]["name"], "Growth");
    assert!(
        context.starts_with("Marketing Manager owns its work now."),
        "{context}"
    );
    assert!(context.contains("- Routine: X Ads budget"), "{context}");
    assert_eq!(status(&h.app, &growth).await, "active");

    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
    answer(&h.app, &main, &question, "approve").await;
    let token = h.continued(turn, &token).await;
    let deleted = tool(&h.app, &token, "delete_bot", args.clone()).await;
    assert!(!deleted.error, "{}", deleted.body);
    assert_eq!(
        deleted.result["removed"]["routines"],
        json!(["X Ads budget"])
    );
    assert!(matches!(
        deleted.result["status"].as_str(),
        Some("deleted" | "deleting")
    ));
    assert_ne!(status(&h.app, &growth).await, "active");
    assert_eq!(
        card(&h.app, &question).await.3.as_deref(),
        Some("completed")
    );

    let retry = tool(&h.app, &token, "delete_bot", args).await;
    assert!(!retry.error, "{}", retry.body);
    assert_eq!(retry.result["status"], "deleted");
    assert!(retry.result.get("question_id").is_none());
    h.release(turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn refused_targets_and_a_denial_leave_the_bot_active() {
    let h = harness(common::TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let growth = h.app.create_bot("Growth").await;

    let (turn, token) = h.running(&growth, GATE).await;
    for (args, code) in [
        (json!({ "target_bot_id": main }), "MAIN_BOT"),
        (json!({ "target_bot_id": growth }), "OWN_BOT"),
        (json!({ "target_bot_id": Uuid::new_v4() }), "BOT_NOT_FOUND"),
        (json!({ "target_bot_id": "Growth" }), "INVALID_ARGUMENTS"),
        (
            json!({ "target_bot_id": main, "reason": "x".repeat(501) }),
            "INVALID_ARGUMENTS",
        ),
    ] {
        let called = tool(&h.app, &token, "delete_bot", args.clone()).await;
        assert!(called.error, "{args}: {}", called.body);
        assert_eq!(called.code(), code, "{args}");
    }
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
    let cards: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(cards, 0);

    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let args = json!({ "target_bot_id": growth });
    let pending = tool(&h.app, &token, "delete_bot", args.clone()).await;
    let question = pending.result["question_id"].as_str().unwrap().to_owned();
    assert!(
        card(&h.app, &question)
            .await
            .2
            .contains("Nothing scheduled or in progress stops.")
    );
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
    answer(&h.app, &main, &question, "deny").await;
    let token = h.continued(turn, &token).await;
    let denied = tool(&h.app, &token, "delete_bot", args).await;
    assert!(!denied.error, "{}", denied.body);
    assert_eq!(denied.result["status"], "denied");
    assert_eq!(status(&h.app, &growth).await, "active");
    h.release(turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_cannot_delete_the_bot_that_delegated_it() {
    let h = harness(common::TestApp::builder()).await;
    let growth = h.app.create_bot("Growth").await;
    let worker = h.app.create_bot("Worker").await;
    let spare = h.app.create_bot("Spare").await;
    let (root, token) = h.running(&growth, GATE).await;
    let delegated = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": worker, "task": format!("{GATE_EVERY_ATTEMPT} Retire bots.") }),
    )
    .await;
    assert!(!delegated.error, "{}", delegated.body);
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    let (_, child_token) = h.started(child).await;
    let refused = tool(
        &h.app,
        &child_token,
        "delete_bot",
        json!({ "target_bot_id": growth }),
    )
    .await;
    assert_eq!(refused.code(), "UPSTREAM_BOT", "{}", refused.body);
    assert_eq!(status(&h.app, &growth).await, "active");
    let task: String =
        sqlx::query_scalar("SELECT status FROM delegations WHERE child_turn_id = ?1")
            .bind(child)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(task, "running");
    let other = tool(
        &h.app,
        &child_token,
        "delete_bot",
        json!({ "target_bot_id": spare }),
    )
    .await;
    assert_eq!(other.result["status"], "pending", "{}", other.body);
    h.release(child).await;
    h.release(root).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_reports_shutdown_pending_until_the_bot_is_deleted() {
    let h = harness(common::TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let growth = h.app.create_bot("Growth").await;
    let (busy, _) = h.running(&growth, GATE).await;

    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let args = json!({ "target_bot_id": growth });
    let pending = tool(&h.app, &token, "delete_bot", args.clone()).await;
    let question = pending.result["question_id"].as_str().unwrap().to_owned();
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
    answer(&h.app, &main, &question, "approve").await;
    let token = h.continued(turn, &token).await;
    let deleted = tool(&h.app, &token, "delete_bot", args.clone()).await;
    assert_eq!(deleted.result["status"], "deleting", "{}", deleted.body);
    let retry = tool(&h.app, &token, "delete_bot", args.clone()).await;
    assert_eq!(retry.result["status"], "deleting", "{}", retry.body);

    h.release(busy).await;
    h.app.wait_turn(busy, &["interrupted", "completed"]).await;
    tokio::time::timeout(common::SETTLE, async {
        while status(&h.app, &growth).await != "deleted" {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the bot finished deleting");
    let settled = tool(&h.app, &token, "delete_bot", args).await;
    assert_eq!(settled.result["status"], "deleted", "{}", settled.body);
    h.release(turn).await;
}
