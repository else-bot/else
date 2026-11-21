//! Removing an inbox: a bot asks on a card the owner must approve, and the
//! same address connects again afterwards.

use super::*;

async fn connect(app: &TestApp) -> String {
    let created = app
        .owner(
            "POST",
            "/v1/connectors/email/accounts",
            Some(json!({"config":config(),"credential":{"kind":"password","password":"private-app-password"}})),
        )
        .await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    created.body["account"]["id"].as_str().unwrap().to_owned()
}

async fn card(app: &TestApp, bot: &str, question: &str) -> Value {
    app.owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["questions"].as_array().unwrap().clone())
        .find(|q| q["id"] == question)
        .unwrap()
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

async fn triggers(app: &TestApp, bot: &str) -> Value {
    app.owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["mail_triggers"]
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_card_removes_the_inbox_and_its_triggers_even_with_a_broad_grant() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let app = &h.app;
    let account = connect(app).await;
    let main = app.main_bot().await;
    let watcher = app.create_bot("Watcher").await;
    let wake = app
        .owner(
            "PUT",
            &format!("/v1/connectors/email/accounts/{account}/bots/{watcher}/wake"),
            Some(json!({"wake":true})),
        )
        .await;
    assert_eq!(wake.status, StatusCode::OK, "{}", wake.body);
    assert_eq!(triggers(app, &watcher).await[0]["account_id"], account);
    sqlx::query("INSERT INTO bot_permissions (bot_id, scope) VALUES (?1, 'destructive')")
        .bind(main.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();

    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let args = json!({"account_id": account, "reason": "Reconnect it with a fresh sign-in."});
    let pending = tool(app, &token, "mail_disconnect", args.clone()).await;
    pending.ok();
    assert_eq!(pending.result["parked"], true, "{}", pending.body);
    let question = pending.result["question_id"].as_str().unwrap().to_owned();
    let shown = card(app, &main, &question).await;
    assert_eq!(
        shown["choices"],
        json!([{ "id": "approve", "label": "Approve once" }, { "id": "deny", "label": "Deny" }]),
        "a saved grant is never offered or used"
    );
    assert_eq!(shown["title"], "Remove bot@example.com");
    assert_eq!(shown["action"]["email_removal"]["account_id"], account);
    let context = shown["context"].as_str().unwrap();
    assert!(
        context.starts_with("Reconnect it with a fresh sign-in."),
        "{context}"
    );
    assert!(context.contains("Watcher"), "{context}");
    assert!(
        context.contains("Mail triggers that stop: Watcher."),
        "{context}"
    );
    let path = format!("/v1/connectors/email/accounts/{account}");
    assert_eq!(
        app.owner("GET", &path, None).await.status,
        StatusCode::OK,
        "nothing is removed before the owner approves"
    );

    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    answer(app, &main, &question, "approve").await;
    let token = h.continued(turn, &token).await;
    let removed = tool(app, &token, "mail_disconnect", args.clone()).await;
    removed.ok();
    assert_eq!(removed.result["status"], "removed", "{}", removed.body);
    assert_eq!(removed.result["address"], "bot@example.com");
    assert_eq!(
        app.owner("GET", &path, None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(triggers(app, &watcher).await, json!([]));
    let listed = tool(app, &token, "mail_accounts", json!({})).await;
    assert_eq!(listed.result["accounts"], json!([]), "{}", listed.body);
    assert_eq!(card(app, &main, &question).await["status"], "answered");

    let retry = tool(app, &token, "mail_disconnect", args).await;
    retry.ok();
    assert_eq!(retry.result["status"], "not_connected");
    h.release(turn).await;

    let again = connect(app).await;
    assert_ne!(again, account, "the same address connects as a new account");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_cannot_remove_an_inbox_it_cannot_read_or_one_the_owner_kept() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let app = &h.app;
    let account = connect(app).await;
    let main = app.main_bot().await;
    let growth = app.create_bot("Growth").await;
    let revoked = app
        .owner(
            "DELETE",
            &format!("/v1/connectors/email/accounts/{account}/bots/{growth}"),
            None,
        )
        .await;
    assert_eq!(revoked.status, StatusCode::OK, "{}", revoked.body);

    let (turn, token) = h.running(&growth, GATE).await;
    let refused = tool(
        app,
        &token,
        "mail_disconnect",
        json!({"account_id": account}),
    )
    .await;
    assert!(refused.error, "{}", refused.body);
    assert_eq!(refused.code(), "mail_forbidden", "{}", refused.body);
    let unknown = tool(
        app,
        &token,
        "mail_disconnect",
        json!({"account_id": Uuid::new_v4()}),
    )
    .await;
    assert_eq!(
        unknown.result["status"], "not_connected",
        "{}",
        unknown.body
    );
    let invalid = tool(
        app,
        &token,
        "mail_disconnect",
        json!({"account_id": account, "reason": "x".repeat(501)}),
    )
    .await;
    assert!(invalid.error, "{}", invalid.body);
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;

    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let args = json!({"account_id": account});
    let pending = tool(app, &token, "mail_disconnect", args.clone()).await;
    let question = pending.result["question_id"].as_str().unwrap().to_owned();
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    answer(app, &main, &question, "deny").await;
    let token = h.continued(turn, &token).await;
    let denied = tool(app, &token, "mail_disconnect", args).await;
    assert_eq!(denied.result["status"], "denied", "{}", denied.body);
    assert_eq!(
        app.owner(
            "GET",
            &format!("/v1/connectors/email/accounts/{account}"),
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    h.release(turn).await;
}
