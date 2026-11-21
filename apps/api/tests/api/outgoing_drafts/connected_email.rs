//! Drafts sent through a connected email account.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_email_sends_the_owners_approved_edit_once_through_its_account() {
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let unpermitted = connect(app, &main, "other@owner.example", false).await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;

    let direct = tool(
        app,
        &token,
        "mail_action",
        json!({ "account_id": account, "action_id": Uuid::new_v4(),
                "command": { "operation": "send", "draft": {
                    "to": ["ada@example.com"], "subject": "Hi", "body": "Unapproved" } } }),
    )
    .await;
    assert!(direct.error);
    assert_eq!(direct.result, "mail_send_needs_approved_draft");
    let reply = tool(
        app,
        &token,
        "mail_action",
        json!({ "account_id": account, "action_id": Uuid::new_v4(),
                "command": { "operation": "reply", "body": "Unapproved", "send": true,
                             "message": { "folder": "INBOX", "uid_validity": 7, "uid": 42 } } }),
    )
    .await;
    assert!(reply.error);
    assert_eq!(reply.result, "mail_send_needs_approved_draft");
    assert!(
        mail.commands.lock().unwrap().is_empty(),
        "nothing reached the server"
    );
    let forbidden = tool(app, &token, "draft_email", connected_reply(unpermitted)).await;
    assert!(
        forbidden.error && forbidden.code() == "MAIL_FORBIDDEN",
        "{}",
        forbidden.body
    );
    let mut spoofed = connected_reply(account);
    spoofed["from"] = json!("ceo@owner.example");
    let refused = tool(app, &token, "draft_email", spoofed).await;
    assert!(
        refused.error && refused.code() == "INVALID_DRAFT",
        "{}",
        refused.body
    );

    let paused = connect(app, &main, "paused@owner.example", true).await;
    let paused_response = app
        .owner(
            "PATCH",
            &format!("/v1/connectors/email/accounts/{paused}"),
            Some(json!({"active":false})),
        )
        .await;
    assert_eq!(paused_response.status, StatusCode::OK);
    let refused = tool(app, &token, "draft_email", connected_reply(paused)).await;
    assert!(
        refused.error && refused.code() == "MAIL_FORBIDDEN",
        "{}",
        refused.body
    );

    let folder = "Projects/\"urgent\"/INBOX";
    let mut args = connected_reply(account);
    args["reply_to"]["folder"] = json!(folder);
    let id = draft(app, &token, args).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[0].clone();
    assert_eq!(
        card["content"]["from"], "main@owner.example",
        "the account's own address"
    );
    assert_eq!(card["content"]["account_id"], account.to_string());
    assert_eq!(card["content"]["reply_to"]["uid"], 42);
    assert!(!card.to_string().contains("private-app-password"));

    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "subject": "Re: Lunch Friday", "body": "Friday at noon. Main" }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains(&format!("account_id=\"{account}\"")),
        "{packet}"
    );
    assert!(packet.contains("send it now with send_draft"), "{packet}");
    assert!(
        !packet.contains(" reply_to=\""),
        "the folder must not break tag attributes: {packet}"
    );
    let reference = packet
        .lines()
        .find_map(|line| line.strip_prefix("Reply to message: "))
        .unwrap();
    let reference: Value = serde_json::from_str(reference).unwrap();
    assert_eq!(reference["folder"], folder);

    for (name, args) in [
        ("computer_run", send_args(id)),
        ("start_draft_send", json!({ "draft_id": id })),
    ] {
        let other = tool(app, &token, name, args).await;
        assert!(
            other.error && other.code() == "CONNECTED_ACCOUNT",
            "{name}: {}",
            other.body
        );
    }
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    let again = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(again.result["status"], "sent", "a repeat reads the receipt");
    let contradicted = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "failed" }),
    )
    .await;
    assert!(contradicted.error && contradicted.code() == "DRAFT_SETTLED");

    let sends = mail.sends();
    assert_eq!(sends.len(), 1, "one transport send");
    assert_eq!(sends[0].to, ["ada@example.com"]);
    assert_eq!(sends[0].cc, ["bob@example.com"]);
    assert_eq!(sends[0].subject, "Re: Lunch Friday");
    assert_eq!(sends[0].body, "Friday at noon. Main");
    assert_eq!(sends[0].reply_to.as_ref().unwrap().folder, folder);
    assert_eq!(
        sends[0].reply_to.as_ref().map(|m| (m.uid_validity, m.uid)),
        Some((7, 42))
    );
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(receipt["delivery"]["via"], "mail");
    assert_eq!(receipt["delivery"]["account_id"], account.to_string());
    let (status, operation, action_turn, action_id): (String, String, Uuid, Uuid) = sqlx::query_as(
        "SELECT status, operation, turn_id, action_id FROM connector_actions
             WHERE account_id = ?1",
    )
    .bind(account)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), operation.as_str(), action_turn),
        ("succeeded", "send", turn)
    );
    assert_eq!(
        receipt["delivery"]["action_id"],
        action_id.to_string(),
        "the card names the mail action receipt"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn uncertain_revoked_and_revised_connected_emails_never_send_twice() {
    let mail = Arc::new(FakeMail::default());
    mail.connection_lost
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, connected_reply(account)).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    let token = h.continued(first, &token).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(lost.result["status"], "uncertain", "{}", lost.body);
    let again = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(again.result["status"], "uncertain");
    assert_eq!(mail.sends().len(), 1, "an uncertain send is never repeated");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    assert_eq!(journaled(app, &main, id).await["status"], "uncertain");

    mail.connection_lost
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let sent = send_with_draft(
        app,
        &main,
        &format!("{GATE} I checked Sent, it is not there. Send it again."),
        json!({ "id": id, "version": 1 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (second, token) = h
        .started(sent.body["turn"]["id"].as_str().unwrap().parse().unwrap())
        .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    permit(app, account, &main, false).await;
    let revoked = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    let revoked = revoked.result;
    assert_eq!(
        revoked["status"], "uncertain",
        "the earlier send stays uncertain: {revoked}"
    );
    assert!(
        revoked["detail"]
            .as_str()
            .unwrap()
            .contains("was not searched: mail_forbidden. Nothing was sent again"),
        "{revoked}"
    );
    assert_eq!(mail.sends().len(), 1, "a revoked account sends nothing");
    h.release(second).await;
    app.wait_turn(second, &["completed"]).await;

    permit(app, account, &main, true).await;
    let sent = send_with_draft(
        app,
        &main,
        &format!("{GATE} Make it shorter."),
        json!({ "id": id, "version": 1 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (_, token) = h
        .started(sent.body["turn"]["id"].as_str().unwrap().parse().unwrap())
        .await;
    let mut unbound = connected_reply(account);
    unbound["account_id"] = Value::Null;
    unbound["reply_to"] = Value::Null;
    unbound["from"] = json!("main@owner.example");
    unbound["replaces"] = json!(id);
    let changed = tool(app, &token, "draft_email", unbound).await;
    assert!(
        changed.error && changed.code() == "CHANNEL_CHANGED",
        "{}",
        changed.body
    );
    let mut revision = connected_reply(account);
    revision["body"] = json!("Friday works.");
    revision["replaces"] = json!(id);
    let revised = tool(app, &token, "draft_email", revision).await;
    assert!(revised.error);
    assert_eq!(
        revised.result, "DRAFT_SETTLED: the draft is uncertain",
        "an email that may have gone out is never revised into a second send"
    );
    assert_eq!(mail.sends().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_send_settles_from_its_mail_receipt_even_after_the_request_ends() {
    use else_api::db::outgoing_drafts::{self, Claim, Claimed, Payload};
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let bot: Uuid = main.parse().unwrap();
    let account = connect(app, &main, "main@owner.example", true).await;
    let generation = store::account(&app.db.pool, account)
        .await
        .unwrap()
        .unwrap()
        .generation;
    let mut outcomes = Vec::new();
    for dispatched in [false, true] {
        let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
        let mut args = connected_reply(account);
        args["body"] = json!(format!("Dispatched: {dispatched}"));
        let id = draft(app, &token, args).await;
        h.release(turn).await;
        app.wait_turn(turn, &["waiting"]).await;
        owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
        h.continued(turn, &token).await;
        let Claim::Fresh(Claimed {
            action_id,
            payload: Payload::Mail(email),
            ..
        }) = outgoing_drafts::claim_connected(&app.db.pool, turn, id, MAIL)
            .await
            .unwrap()
        else {
            panic!("an approved connected email is claimed for its mail send");
        };
        if dispatched {
            let started = store::begin_action(
                &app.db.pool,
                account,
                generation,
                bot,
                turn,
                action_id,
                &email.send_command(Vec::new(), None),
            )
            .await
            .unwrap();
            assert!(matches!(
                started,
                else_api::connector_core::actions::Start::Dispatch
            ));
        }
        h.release(turn).await;
        app.wait_turn(turn, &["completed"]).await;
        let ended = journaled(app, &main, id).await;
        outcomes.push((id, action_id, ended["status"].as_str().unwrap().to_owned()));
        assert!(ended["result_detail"].is_string(), "{ended}");
    }
    assert_eq!(outcomes[0].2, "failed", "never recorded, so never sent");
    assert_eq!(outcomes[1].2, "uncertain", "dispatched without a receipt");
    let (id, action_id, _) = outcomes[1];
    assert_eq!(
        store::ACTIONS
            .finish(
                &app.db.pool,
                account,
                action_id,
                else_api::connector_core::actions::Outcome::Succeeded,
                json!({}),
            )
            .await
            .unwrap()["status"],
        "succeeded"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "sent", "the late receipt settles it");
    assert_eq!(
        journaled(app, &main, id).await["status"],
        "sent",
        "the late receipt reaches the live conversation without a reload"
    );
    assert!(
        mail.sends().is_empty(),
        "the test drove the action receipts itself"
    );
}
