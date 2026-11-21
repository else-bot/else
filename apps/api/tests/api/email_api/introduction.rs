//! Introducing a connected account to the bots.

use super::*;

#[tokio::test]
async fn accounts_are_available_to_bots_created_later_without_waking_them() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let account = account(&h.app).await;
    let bot = h.app.create_bot("Mail helper").await;
    let (_, token) = h.running(&bot, GATE).await;
    let listed = tool(&h.app, &token, "mail_accounts", json!({})).await;
    assert_eq!(listed.result["accounts"][0]["id"], json!(account.id));
    assert_eq!(listed.result["accounts"][0]["permissions"]["wake"], false);
    assert!(
        !store::set_permissions(
            &h.app.db.pool,
            account.id,
            bot.parse().unwrap(),
            &Permissions {
                read: true,
                write: true,
                send: true,
                wake: false
            }
        )
        .await
        .unwrap()
    );
    assert!(
        store::grants(&h.app.db.pool, account.id)
            .await
            .unwrap()
            .is_empty()
    );
    store::set_permissions(
        &h.app.db.pool,
        account.id,
        bot.parse().unwrap(),
        &Permissions::default(),
    )
    .await
    .unwrap();
    assert!(
        tool(&h.app, &token, "mail_accounts", json!({}))
            .await
            .result["accounts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn introduction_leaves_busy_work_running_and_can_be_retried_when_idle() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let bot = h.app.main_bot().await;
    let account = account(&h.app).await;
    let (running, _) = h.running(&bot, GATE).await;
    let body = json!({"bot_id":bot,"request_id":Uuid::new_v4()});
    let path = format!("/v1/connectors/email/accounts/{}/introduction", account.id);
    let refused = h.app.owner("POST", &path, Some(body.clone())).await;
    assert_eq!(refused.status, StatusCode::CONFLICT);
    assert_eq!(refused.body["error"]["code"], "bot_busy");
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let turn = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == running.to_string())
        .unwrap();
    assert_eq!(turn["status"], "running");
    assert!(
        !snapshot.body["items"]
            .to_string()
            .contains("[else.bot email setup]")
    );
    h.release(running).await;
    h.app.wait_turn(running, &["completed"]).await;
    let accepted = h.app.owner("POST", &path, Some(body.clone())).await;
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);
    assert_eq!(
        h.app.owner("POST", &path, Some(body)).await.body,
        accepted.body
    );
}

#[tokio::test]
async fn introduction_is_private_idempotent_and_reaches_the_model_and_history() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let bot = h.app.main_bot().await;
    let account = account(&h.app).await;
    let path = format!("/v1/connectors/email/accounts/{}/introduction", account.id);
    let request_id = Uuid::new_v4();
    let body = json!({"bot_id":bot,"request_id":request_id});
    let first = h.app.owner("POST", &path, Some(body.clone())).await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let again = h.app.owner("POST", &path, Some(body)).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.body);
    assert_eq!(first.body, again.body);
    let turn: Uuid = first.body["turn_id"].as_str().unwrap().parse().unwrap();
    h.app.wait_turn(turn, &["completed"]).await;
    let inputs = h.inputs(turn).await;
    assert_eq!(inputs.len(), 1);
    assert!(inputs[0].text.contains(&account.id.to_string()));
    assert!(inputs[0].text.contains("bot@example.com"));
    assert!(
        inputs[0]
            .text
            .contains("Ask the owner what this inbox is for")
    );
    assert!(inputs[0].text.contains("Do not inspect the inbox"));
    assert!(!inputs[0].text.contains("private-app-password"));
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    assert!(
        !snapshot.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "user")
    );
    let events = else_api::db::events::after(&h.app.db.pool, bot.parse().unwrap(), 0, 1000)
        .await
        .unwrap();
    assert!(
        !events
            .iter()
            .any(|(_, kind, data)| kind == "item.upsert" && data["item"]["kind"] == "user")
    );
    let (sent_turn, token) = h.running(&bot, GATE).await;
    let later_inputs = h.inputs(sent_turn).await;
    assert!(
        later_inputs[0]
            .history
            .iter()
            .any(|entry| entry.text == inputs[0].text)
    );
    assert_eq!(
        h.app
            .call(
                "POST",
                &path,
                Some(json!({"bot_id":bot,"request_id":Uuid::new_v4()})),
                Some(&token)
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert!(
        store::grants(&h.app.db.pool, account.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        h.app
            .send(&bot, request_id, &inputs[0].text, None)
            .await
            .status,
        StatusCode::CONFLICT
    );
}
