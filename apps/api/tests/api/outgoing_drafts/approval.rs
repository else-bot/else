//! How the owner approves, edits, or discards a draft, and which version an
//! approval binds.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn the_approve_button_sends_the_owners_edited_version_once() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut bad = lunch();
    bad["subject"] = json!("Hi\r\nBcc: everyone@example.com");
    let refused = tool(app, &token, "draft_email", bad).await;
    assert!(
        refused.error && refused.code() == "INVALID_DRAFT",
        "{}",
        refused.body
    );
    let id = draft(app, &token, lunch()).await;
    let parked = tool(app, &token, "memory_bots", json!({})).await;
    assert_eq!(parked.status, StatusCode::CONFLICT, "the attempt must end");

    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[0].clone();
    assert_eq!(card["id"], id.to_string());
    assert_eq!(card["revision"], 1);
    assert_eq!(card["content"]["channel"], "email");
    assert_eq!(card["content"]["from"], "main@bots.example");
    assert_eq!(card["content"]["to"], json!(["ada@example.com"]));
    assert_eq!(card["content"]["cc"], json!(["bob@example.com"]));
    assert_eq!(card["content"]["bcc"], json!([]));
    assert_eq!(card["content"]["subject"], "Lunch on Friday?");
    assert!(card["content"].get("account_id").is_none());
    assert!(card["content"].get("reply_to").is_none());
    assert!(card["content"].get("attachments").is_none());
    assert_eq!(card["status"], "pending");
    assert_eq!(card["version"], 1);
    assert_eq!(card["can_approve"], true);
    for key in ["approved_version", "delivery", "kind", "settled_at"] {
        assert_eq!(card.get(key), Some(&Value::Null), "{key}: {card}");
    }
    assert_eq!(journaled(app, &main, id).await["status"], "pending");

    let edited = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "body": "Hi Ada, lunch Friday?" }),
    )
    .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);
    assert_eq!(edited.body["draft"]["version"], 2);
    let stale = owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "draft_changed");
    let bad_edit = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 2, "subject": "a\nb" }),
    )
    .await;
    assert_eq!(
        bad_edit.status,
        StatusCode::BAD_REQUEST,
        "{}",
        bad_edit.body
    );

    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "subject": "Lunch Friday" }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let shown_draft = &approved.body["draft"];
    assert_eq!(shown_draft["status"], "approved", "approval is not a send");
    assert_eq!(shown_draft["version"], 3);
    assert_eq!(shown_draft["approved_version"], 3);
    assert_eq!(shown_draft["approval_source"], "button");
    assert_eq!(approved.body["requeued"], true);
    let replay = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "subject": "Lunch Friday" }),
    )
    .await;
    assert_eq!(
        replay.status,
        StatusCode::OK,
        "a lost response retries its original edits: {}",
        replay.body
    );
    assert_eq!(replay.body["requeued"], false);
    let changed_replay = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "subject": "Something else" }),
    )
    .await;
    assert_eq!(changed_replay.status, StatusCode::CONFLICT);
    let replay = owner_action(app, &main, id, "approve", json!({ "version": 3 })).await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body["requeued"], false);
    let late_edit = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 3, "body": "changed" }),
    )
    .await;
    assert_eq!(late_edit.body["error"]["code"], "draft_settled");

    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot outgoing drafts]"), "{packet}");
    assert!(
        packet.contains("Subject: Lunch Friday\n\nHi Ada, lunch Friday?"),
        "{packet}"
    );
    assert!(
        packet.contains("The owner approved it on its card"),
        "{packet}"
    );

    let local = tool(
        app,
        &token,
        "computer_run",
        json!({ "command": "true", "effect": "local", "approved_draft": id }),
    )
    .await;
    assert!(
        local.error
            && local
                .result
                .as_str()
                .unwrap()
                .contains("only with effect external")
    );
    let claimed = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(
        claimed.error && claimed.code() == "COMPUTER_UNAVAILABLE",
        "the claim passed the gate and reached the computer: {}",
        claimed.body
    );
    assert_eq!(questions(app).await, 0, "no second approval card");
    let again = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(
        again.error && again.code() == "DRAFT_SETTLED",
        "one send per approval"
    );
    let sent = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent", "detail": "250 OK id=abc" }),
    )
    .await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    let replayed = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent" }),
    )
    .await;
    assert_eq!(replayed.result["status"], "sent", "{}", replayed.body);
    let contradicted = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "failed" }),
    )
    .await;
    assert!(contradicted.error && contradicted.code() == "DRAFT_SETTLED");

    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(receipt["result_detail"], "250 OK id=abc");
    assert_eq!(receipt["delivery"]["via"], "tool");
    assert_eq!(receipt["delivery"]["tool"], "computer_run");
    assert!(receipt["settled_at"].is_string());
    let card = shown(app, &main, turn).await[0].clone();
    assert_eq!(card["status"], "sent");
    assert_eq!(card["can_approve"], false);

    let generic = tool(
        app,
        &token,
        "computer_run",
        json!({ "command": "python3 send_mail.py", "effect": "external" }),
    )
    .await;
    assert_eq!(generic.result["status"], "pending", "{}", generic.body);
    assert_eq!(
        questions(app).await,
        1,
        "other external calls still need their card"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discarding_continues_the_turn_and_nothing_can_send_it() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    let early = else_api::db::outgoing_drafts::claim(
        &app.db.pool,
        turn,
        id,
        &else_api::app::outgoing::Delivery::Host,
    )
    .await;
    assert!(
        matches!(
            early,
            Err(else_api::db::outgoing_drafts::DraftError::Settled(
                else_api::app::outgoing::Status::Pending
            ))
        ),
        "a pending draft cannot be sent: {early:?}"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let discarded = owner_action(app, &main, id, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);
    assert_eq!(discarded.body["draft"]["status"], "discarded");
    assert_eq!(discarded.body["requeued"], true);
    let replay = owner_action(app, &main, id, "discard", json!({})).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body["requeued"], false);
    let approve = owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(
        approve.body["error"]["code"], "draft_settled",
        "{}",
        approve.body
    );

    let token = h.continued(turn, &token).await;
    let send = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(send.error && send.code() == "DRAFT_SETTLED");
    let report = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "sent" }),
    )
    .await;
    assert!(
        report.error && report.code() == "OTHER_REQUEST",
        "a report cannot invent a send"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("The owner discarded it on its card"),
        "{packet}"
    );
    assert_eq!(journaled(app, &main, id).await["status"], "discarded");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_joined_message_carries_the_owners_edits_and_a_revision_replaces_the_card() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let turns_before: i64 = sqlx::query_scalar("SELECT count(*) FROM turns")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let stale = send_with_draft(
        app,
        &main,
        "Shorter please.",
        json!({ "id": id, "version": 2 }),
    )
    .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "draft_changed");
    let turns_after: i64 = sqlx::query_scalar("SELECT count(*) FROM turns")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        turns_before, turns_after,
        "a stale draft refuses the whole message"
    );

    let follow_up = "Make it warmer and mention the new place.";
    let (second, token) = joined(
        &h,
        &main,
        first,
        &token,
        follow_up,
        Some(json!({ "id": id, "version": 1, "body": "Hi Ada, lunch Friday? My treat." })),
    )
    .await;
    assert_eq!(status(app, second).await, "queued");
    let old = shown(app, &main, first).await[0].clone();
    assert_eq!(old["status"], "pending");
    assert_eq!(
        old["version"], 2,
        "the owner's edits were saved with the message"
    );
    assert_eq!(old["content"]["body"], "Hi Ada, lunch Friday? My treat.");
    assert_eq!(old["in_reply"], true, "{old}");
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("in_context=\"true\""), "{packet}");
    assert!(
        packet.contains("Hi Ada, lunch Friday? My treat."),
        "{packet}"
    );
    assert!(packet.contains(follow_up), "{packet}");

    let mut revised = lunch();
    revised["body"] = json!("Hi Ada,\n\nLunch Friday at the new ramen place? My treat.\n\nMain");
    revised["replaces"] = json!(id);
    let drafted = tool(app, &token, "draft_email", revised).await.result;
    assert_eq!(drafted["status"], "pending", "{drafted}");
    assert_eq!(drafted["draft"]["revision"], 2);
    let new_id: Uuid = drafted["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(drafted["draft"]["lineage_id"], old["lineage_id"]);

    let cards = shown(app, &main, first).await;
    let old = card_of(&cards, id);
    assert_eq!(old["status"], "superseded");
    assert_eq!(old["superseded_by"], new_id.to_string());
    assert_eq!(card_of(&cards, new_id)["status"], "pending");
    assert!(shown(app, &main, second).await.is_empty());
    assert_eq!(journaled(app, &main, id).await["status"], "superseded");
    let obsolete = send_with_draft(app, &main, "Send it.", json!({ "id": id, "version": 2 })).await;
    assert_eq!(
        obsolete.body["error"]["code"], "draft_settled",
        "{}",
        obsolete.body
    );
    let obsolete = owner_action(app, &main, id, "edit", json!({ "version": 2, "body": "x" })).await;
    assert_eq!(obsolete.body["error"]["code"], "draft_settled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clear_chat_approval_confirms_exactly_the_open_version() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let (second, token) = joined(
        &h,
        &main,
        first,
        &token,
        "Looks good, send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let early = tool(app, &token, "computer_run", send_args(id)).await;
    assert!(
        early.error && early.code() == "DRAFT_SETTLED",
        "chat words alone send nothing"
    );
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    let replay = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(replay.result["status"], "approved", "{}", replay.body);

    assert!(
        shown(app, &main, second).await.is_empty(),
        "the card stays with the reply the message joined"
    );
    let card = shown(app, &main, first).await[0].clone();
    assert_eq!(card["status"], "approved");
    assert_eq!(card["approval_source"], "message");
    assert_eq!(card["approved_version"], 1);
    let item: String =
        sqlx::query_scalar("SELECT lower(hex(owner_item_id)) FROM turns WHERE id = ?1")
            .bind(second)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(
        card["approved_item_id"].as_str().unwrap().replace('-', ""),
        item
    );

    let started = tool(app, &token, "start_draft_send", json!({ "draft_id": id })).await;
    let started = started.result;
    assert_eq!(started["status"], "sending", "{started}");
    assert_eq!(started["draft"]["content"]["subject"], "Lunch on Friday?");
    let twice = tool(app, &token, "start_draft_send", json!({ "draft_id": id })).await;
    assert!(twice.error && twice.code() == "DRAFT_SETTLED");
    let uncertain = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "uncertain", "detail": "SMTP timeout after DATA" }),
    )
    .await;
    assert_eq!(
        uncertain.result["status"], "uncertain",
        "{}",
        uncertain.body
    );
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "uncertain");
    assert_eq!(receipt["delivery"]["via"], "host");
    assert_eq!(questions(app).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_message_sent_with_the_unchanged_draft_can_approve_it() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;

    let (_, plain) = joined(&h, &main, first, &token, "Send it.", None).await;
    let unbound = tool(
        app,
        &plain,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(
        unbound.error && unbound.code() == "NOT_IN_CONTEXT",
        "a message without the draft"
    );

    let (_, token) = joined(
        &h,
        &main,
        first,
        &plain,
        "Send it.",
        Some(json!({ "id": id, "version": 1 })),
    )
    .await;
    let edited = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "subject": "Lunch?" }),
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
    let unknown = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": Uuid::new_v4() }),
    )
    .await;
    assert!(unknown.error && unknown.code() == "NOT_FOUND");
    assert_eq!(journaled(app, &main, id).await["status"], "pending");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draft_sent_with_a_message_the_reply_has_not_taken_yet_cannot_be_approved() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    let sent = send_with_draft(app, &main, "Send it.", json!({ "id": id, "version": 1 })).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], first.to_string());
    assert_eq!(attempt(app, first).await, 1, "the attempt is still running");
    let early = else_api::db::outgoing_drafts::confirm(&app.db.pool, first, id, None).await;
    assert!(
        matches!(
            early,
            Err(else_api::db::outgoing_drafts::DraftError::NotInContext)
        ),
        "the reply has not read the message yet: {early:?}"
    );

    h.release(first).await;
    let token = h.continued(first, &token).await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
}
