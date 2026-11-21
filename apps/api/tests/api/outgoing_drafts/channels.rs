//! Drafts for X posts and Telegram messages.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn an_x_post_needs_no_destination_and_its_card_approval_sends_it_once() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let emailish = json!({ "platform": "x", "body": "Hi", "subject": "Launch" });
    let refused = tool(app, &token, "draft_message", emailish).await;
    assert!(
        refused.error && refused.code() == "INVALID_DRAFT",
        "a post has no subject"
    );
    let id = draft(app, &token, ship_post()).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[0].clone();
    assert_eq!(
        card["content"],
        json!({ "channel": "message", "platform": "x", "account": null, "destination": null,
                "body": "We shipped native drafts today." }),
        "no addresses, Cc/Bcc, or subject"
    );
    assert_eq!(card["can_approve"], true);

    let subject = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "subject": "Launch" }),
    )
    .await;
    assert_eq!(subject.status, StatusCode::BAD_REQUEST, "{}", subject.body);
    assert_eq!(subject.body["error"]["code"], "invalid_draft");
    assert_eq!(
        shown(app, &main, turn).await[0]["status"],
        "pending",
        "a refused approval approves nothing"
    );
    let edited = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "body": "Native drafts shipped today." }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    let stale = owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(stale.body["error"]["code"], "draft_changed");
    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "body": "Native drafts shipped today!" }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["draft"]["approved_version"], 3);
    assert_eq!(
        approved.body["draft"]["content"]["body"],
        "Native drafts shipped today!"
    );
    let replay = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "body": "Native drafts shipped today!" }),
    )
    .await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body["requeued"], false);

    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("channel=\"message\" platform=\"x\""),
        "{packet}"
    );
    assert!(packet.contains("\nNative drafts shipped today!\n</outgoing_draft>"));
    assert!(!packet.contains("Subject:"), "{packet}");
    assert!(packet.contains("send exactly this X post now"), "{packet}");

    let claimed = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(
        claimed.error && claimed.code() == "COMPUTER_UNAVAILABLE",
        "{}",
        claimed.body
    );
    let again = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(again.error && again.code() == "DRAFT_SETTLED");
    assert_eq!(questions(app).await, 0, "no second approval card");
    let sent = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent", "detail": "post id 1890" }),
    )
    .await
    .result;
    assert_eq!(sent["status"], "sent");
    assert_eq!(sent["medium"], "X post");
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(receipt["result_detail"], "post id 1890");
    assert_eq!(receipt["delivery"]["via"], "tool");
    assert_eq!(receipt["content"]["platform"], "x");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_telegram_message_keeps_its_channel_and_owner_edits_across_revisions() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut nowhere = family_message();
    nowhere["destination"] = Value::Null;
    let refused = tool(app, &token, "draft_message", nowhere).await;
    assert!(
        refused.error && refused.code() == "INVALID_DRAFT",
        "a telegram message names its chat"
    );
    let id = draft(app, &token, family_message()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(card["content"]["channel"], "message");
    assert_eq!(card["content"]["platform"], "telegram");
    assert_eq!(card["content"]["destination"], "Family group");
    assert_eq!(card["content"]["account"], "@main_bot");
    for key in ["from", "to", "cc", "bcc", "subject"] {
        assert!(card["content"].get(key).is_none(), "{key} in {card}");
    }

    let with_subject = send_with_draft(
        app,
        &main,
        "Make it 9.",
        json!({ "id": id, "version": 1, "subject": "Dinner" }),
    )
    .await;
    assert_eq!(
        with_subject.status,
        StatusCode::BAD_REQUEST,
        "{}",
        with_subject.body
    );
    assert_eq!(with_subject.body["error"]["code"], "invalid_draft");
    assert_eq!(
        status(app, first).await,
        "waiting",
        "the message was refused"
    );

    let (_, token) = joined(
        &h,
        &main,
        first,
        &token,
        "Make it 9 and ask who brings dessert.",
        Some(json!({ "id": id, "version": 1, "body": "Dinner at 9 on Friday?" })),
    )
    .await;
    let old = shown(app, &main, first).await[0].clone();
    assert_eq!(old["version"], 2, "the owner's edits were saved");
    assert_eq!(old["content"]["body"], "Dinner at 9 on Friday?");
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("channel=\"message\" platform=\"telegram\""),
        "{packet}"
    );
    assert!(
        packet.contains("Destination: Family group\n\nDinner at 9 on Friday?"),
        "{packet}"
    );
    assert!(
        packet.contains("call draft_message with replaces"),
        "{packet}"
    );

    let mut as_email = lunch();
    as_email["replaces"] = json!(id);
    let switched = tool(app, &token, "draft_email", as_email).await;
    assert!(
        switched.error && switched.code() == "CHANNEL_CHANGED",
        "a message cannot become an email"
    );
    let mut as_post = ship_post();
    as_post["replaces"] = json!(id);
    let switched = tool(app, &token, "draft_message", as_post).await;
    assert!(
        switched.error && switched.code() == "CHANNEL_CHANGED",
        "a Telegram message cannot become an X post"
    );
    assert_eq!(shown(app, &main, first).await[0]["status"], "pending");

    let mut revised = family_message();
    revised["body"] = json!("Dinner at 9 on Friday? Who brings dessert?");
    revised["replaces"] = json!(id);
    let revised = tool(app, &token, "draft_message", revised).await.result;
    assert_eq!(revised["status"], "pending", "{revised}");
    assert_eq!(revised["draft"]["revision"], 2);
    assert_eq!(revised["draft"]["lineage_id"], old["lineage_id"]);
    assert_eq!(revised["draft"]["content"]["platform"], "telegram");
    let new_id: Uuid = revised["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let cards = shown(app, &main, first).await;
    assert_eq!(card_of(&cards, id)["status"], "superseded");
    assert_eq!(card_of(&cards, new_id)["status"], "pending");
    let obsolete = send_with_draft(app, &main, "Send it.", json!({ "id": id, "version": 2 })).await;
    assert_eq!(obsolete.body["error"]["code"], "draft_settled");

    let moved = sqlx::query(
        "UPDATE outgoing_drafts SET content = json_set(content, '$.destination', 'Work group')
         WHERE id = ?1",
    )
    .bind(new_id)
    .execute(&app.db.pool)
    .await;
    assert!(moved.is_err(), "a revision's destination is fixed");
    let to_email = sqlx::query(
        "UPDATE outgoing_drafts SET content = json_set(content, '$.channel', 'email')
         WHERE id = ?1",
    )
    .bind(new_id)
    .execute(&app.db.pool)
    .await;
    assert!(to_email.is_err(), "a revision's channel is fixed");

    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let (_, token) = joined(
        &h,
        &main,
        first,
        &token,
        "Perfect, send it.",
        Some(json!({ "id": new_id, "version": 1 })),
    )
    .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": new_id }),
    )
    .await
    .result;
    assert_eq!(confirmed["status"], "approved", "{confirmed}");
    assert!(
        confirmed["next"]
            .as_str()
            .unwrap()
            .contains("Telegram message")
    );
    let approved = card_of(&shown(app, &main, first).await, new_id);
    assert_eq!(approved["approval_source"], "message");
    assert_eq!(approved["approved_version"], 1);
    let started = tool(
        app,
        &token,
        "start_draft_send",
        json!({ "draft_id": new_id }),
    )
    .await
    .result;
    assert_eq!(started["status"], "sending", "{started}");
    assert_eq!(
        started["draft"]["content"]["body"],
        "Dinner at 9 on Friday? Who brings dessert?"
    );
    let uncertain = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": new_id, "status": "uncertain", "detail": "Bot API timed out" }),
    )
    .await
    .result;
    assert_eq!(uncertain["status"], "uncertain");
    assert_eq!(uncertain["medium"], "Telegram message");
    let receipt = journaled(app, &main, new_id).await;
    assert_eq!(receipt["status"], "uncertain");
    assert_eq!(receipt["delivery"]["via"], "host");
    assert_eq!(receipt["content"]["destination"], "Family group");
    assert_eq!(questions(app).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_binds_the_open_message_version_only() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, family_message()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let (_, token) = joined(
        &h,
        &main,
        first,
        &token,
        "Send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let edited = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "body": "Dinner at 7?" }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    let changed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(
        changed.error && changed.code() == "NOT_IN_CONTEXT",
        "the owner changed it after sending"
    );
    let send = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(send.error && send.code() == "DRAFT_SETTLED");
    assert_eq!(journaled(app, &main, id).await["status"], "pending");
}
