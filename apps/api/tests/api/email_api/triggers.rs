//! Mail triggers a bot sets and the owner approves.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_sets_a_mail_trigger_the_owner_approved_and_the_owner_pauses_or_deletes_it() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let account = account(&h.app).await;
    let bot = h.app.create_bot("Mail helper").await;
    let triggers = || async {
        h.app
            .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
            .await
            .body["mail_triggers"]
            .clone()
    };
    assert_eq!(
        triggers().await,
        json!([]),
        "a readable inbox that wakes nothing is no trigger"
    );
    let args =
        json!({"account_id": account.id, "instructions": " Flag invoices from new vendors. "});
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let parked = tool(&h.app, &token, "mail_trigger_set", args.clone()).await;
    assert_eq!(parked.result["parked"], true, "{}", parked.body);
    assert_eq!(
        triggers().await,
        json!([]),
        "nothing wakes the bot before the owner approves"
    );
    let question = parked.result["question_id"].as_str().unwrap().to_owned();
    let card = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["questions"].as_array().unwrap().clone())
        .find(|q| q["id"] == question)
        .unwrap();
    assert!(
        card["context"]
            .as_str()
            .unwrap()
            .contains("Flag invoices from new vendors."),
        "the card shows the exact instructions: {card}"
    );
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
    let answered = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({"choice":"approve"})),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let token = h.continued(turn, &token).await;
    let set = tool(&h.app, &token, "mail_trigger_set", args).await;
    set.ok();
    assert_eq!(set.result["trigger"]["wake"], true, "{}", set.body);
    let listed = triggers().await;
    assert_eq!(listed[0]["account_id"], json!(account.id));
    assert_eq!(listed[0]["instructions"], "Flag invoices from new vendors.");
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;

    let wake = format!(
        "/v1/connectors/email/accounts/{}/bots/{bot}/wake",
        account.id
    );
    let off = h.app.owner("PUT", &wake, Some(json!({"wake":false}))).await;
    assert_eq!(off.status, StatusCode::OK, "{}", off.body);
    let paused = triggers().await;
    assert_eq!(paused[0]["wake"], false);
    assert_eq!(
        paused[0]["instructions"], "Flag invoices from new vendors.",
        "a paused trigger keeps its card"
    );
    let trigger = format!(
        "/v1/connectors/email/accounts/{}/bots/{bot}/trigger",
        account.id
    );
    let deleted = h.app.owner("DELETE", &trigger, None).await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(deleted.body["deleted"], true);
    assert_eq!(triggers().await, json!([]));
    let bot_id: Uuid = bot.parse().unwrap();
    let left = store::permissions(&h.app.db.pool, account.id, bot_id)
        .await
        .unwrap()
        .unwrap();
    assert!(left.read && !left.wake, "deleting a trigger keeps read");

    h.app.owner("PUT", &wake, Some(json!({"wake":true}))).await;
    assert_eq!(
        triggers().await[0]["instructions"],
        store::DEFAULT_WAKE_INSTRUCTIONS
    );
    h.app.owner("PUT", &wake, Some(json!({"wake":false}))).await;
    assert_eq!(
        triggers().await[0]["wake"],
        false,
        "a trigger turned on without instructions keeps its card when paused"
    );
    h.app
        .owner(
            "DELETE",
            &format!("/v1/connectors/email/accounts/{}/bots/{bot}", account.id),
            None,
        )
        .await;
    assert_eq!(
        triggers().await,
        json!([]),
        "an inbox the bot can't read is no trigger"
    );
    let (turn, token) = h.running(&bot, GATE).await;
    let refused = tool(
        &h.app,
        &token,
        "mail_trigger_set",
        json!({"account_id": account.id, "instructions": "Watch it."}),
    )
    .await;
    assert!(
        refused.error,
        "a trigger never restores read: {}",
        refused.body
    );
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_longest_trigger_instructions_fit_the_approval_card() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let account = account(&h.app).await;
    let bot = h.app.create_bot("Mail helper").await;
    let (turn, token) = h.running(&bot, GATE).await;
    let long = tool(
        &h.app,
        &token,
        "mail_trigger_set",
        json!({"account_id": account.id, "instructions": "x".repeat(3501)}),
    )
    .await;
    assert!(long.error, "{}", long.body);
    assert!(!h.app.state.grants.is_parked(turn).await);
    let fits = tool(
        &h.app,
        &token,
        "mail_trigger_set",
        json!({"account_id": account.id, "instructions": "x".repeat(3500)}),
    )
    .await;
    assert_eq!(fits.result["parked"], true, "{}", fits.body);
    h.release(turn).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_task_cannot_change_mail_triggers() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let account = account(&h.app).await;
    let main = h.app.main_bot().await;
    let (root, token) = h
        .running(&main, &format!("{GATE_EVERY_ATTEMPT} make a Mail bot"))
        .await;
    let created = tool(&h.app, &token, "create_bot", json!({ "name": "Mail" })).await;
    let mail = created.result["bot_id"].as_str().unwrap().to_owned();
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": mail, "task": "[gate] Watch the inbox." }),
    )
    .await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let child: Uuid = sqlx::query_scalar(
        "SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1 ORDER BY created_at DESC",
    )
    .bind(root)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    h.release(root).await;
    h.app.wait_turn(root, &["waiting"]).await;
    let (_, child_token) = h.started(child).await;
    for (name, args) in [
        (
            "mail_trigger_set",
            json!({"account_id": account.id, "instructions": "Watch it."}),
        ),
        ("mail_trigger_delete", json!({"account_id": account.id})),
    ] {
        let refused = tool(&h.app, &child_token, name, args).await;
        assert!(
            refused.error || refused.status == StatusCode::FORBIDDEN,
            "{name}: {}",
            refused.body
        );
        assert_ne!(refused.result["parked"], true, "{name} asks no card");
    }
    let mail_id: Uuid = mail.parse().unwrap();
    assert!(
        !store::permissions(&h.app.db.pool, account.id, mail_id)
            .await
            .unwrap()
            .unwrap()
            .wake
    );
    h.release(child).await;
}
