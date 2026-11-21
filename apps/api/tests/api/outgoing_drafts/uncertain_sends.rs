//! Sends that fail or end uncertain, and retries that must never send twice.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn an_unreported_send_is_uncertain_and_an_unused_approval_fails() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, lunch()).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    let token = h.continued(turn, &token).await;
    tool(app, &token, "computer_run", send_args(id)).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "uncertain", "{receipt}");
    assert!(
        receipt["result_detail"]
            .as_str()
            .unwrap()
            .contains("before the bot reported")
    );

    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut other = lunch();
    other["to"] = json!(["carol@example.com"]);
    let id = draft(app, &token, other).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "failed", "{receipt}");
    assert!(
        receipt["result_detail"]
            .as_str()
            .unwrap()
            .contains("Nothing was sent")
    );

    let sent = send_with_draft(
        app,
        &main,
        &format!("{GATE} Try sending it again."),
        json!({ "id": id, "version": 1 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (retry, token) = h
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
    let failed = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": id, "status": "failed", "detail": "No SMTP credentials stored." }),
    )
    .await;
    assert_eq!(failed.result["status"], "failed", "{}", failed.body);
    let card = shown(app, &main, retry).await[0].clone();
    assert_eq!(card["status"], "failed");
    assert_eq!(card["result_detail"], "No SMTP credentials stored.");
    let discarded = owner_action(app, &main, id, "discard", json!({})).await;
    assert_eq!(
        discarded.body["draft"]["status"], "discarded",
        "{}",
        discarded.body
    );
}

/// Starts a request whose owner message approves the open draft `id`, and
/// confirms that approval.
async fn approve_again(h: &Harness, bot: &str, id: Uuid) -> (Uuid, String) {
    let app = &h.app;
    let sent = send_with_draft(
        app,
        bot,
        &format!("{GATE} Send it again."),
        json!({ "id": id, "version": 1 }),
    )
    .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let (turn, token) = h
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
    (turn, token)
}

/// A connected reply, approved on its card and claimed by a running request.
async fn approved_connected_reply(h: &Harness, bot: &str, account: Uuid) -> (Uuid, Uuid, String) {
    let app = &h.app;
    let (turn, token) = h.running(bot, GATE_EVERY_ATTEMPT).await;
    let id = draft(app, &token, connected_reply(account)).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    owner_action(app, bot, id, "approve", json!({ "version": 1 })).await;
    (id, turn, h.continued(turn, &token).await)
}

async fn action_statuses(app: &TestApp, account: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT status FROM connector_actions WHERE account_id = ?1 ORDER BY created_at, rowid",
    )
    .bind(account)
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_that_stops_before_the_message_is_written_fails_with_its_reason() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Envelope));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;

    let refused = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(refused["status"], "failed", "{refused}");
    assert_eq!(
        refused["detail"],
        "Nothing was sent: the mail server closed the connection while naming the sender and \
         recipients.",
    );
    assert_eq!(refused["sends"][0]["phase"], "envelope", "{refused}");
    assert_eq!(action_statuses(app, account).await, ["failed"]);
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    mail.fail_at(None);
    let (_, token) = approve_again(&h, &main, id).await;
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "sent", "{sent}");
    assert!(
        mail.searches().is_empty(),
        "nothing was sent, so Sent is not searched"
    );
    let sends = mail.sends();
    assert_eq!(sends.len(), 2);
    let message_id = format!("draft-{}@owner.example", id.simple());
    assert_eq!(sends[0].message_id.as_deref(), Some(message_id.as_str()));
    assert_eq!(
        sends[1].message_id, sends[0].message_id,
        "a retry keeps its Message-ID"
    );
    assert_eq!(sent["sends"].as_array().unwrap().len(), 2, "{sent}");
    assert_eq!(action_statuses(app, account).await, ["failed", "succeeded"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncertain_send_found_in_sent_is_never_sent_again() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;

    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    assert!(
        lost["detail"].as_str().unwrap().starts_with(
            "It may have been sent: the mail server closed the connection while waiting for the \
             server's reply after the whole message was sent."
        ),
        "{lost}"
    );
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    mail.fail_at(None);
    mail.search_fails
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (second, token) = approve_again(&h, &main, id).await;
    let unsearched = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(unsearched["status"], "uncertain", "{unsearched}");
    assert!(
        unsearched["detail"]
            .as_str()
            .unwrap()
            .starts_with("Its Sent folder was not searched: the mail server timed out."),
        "{unsearched}"
    );
    assert_eq!(mail.sends().len(), 1, "a failed search sends nothing");
    h.release(second).await;
    app.wait_turn(second, &["completed"]).await;

    mail.search_fails
        .store(false, std::sync::atomic::Ordering::SeqCst);
    mail.in_sent
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (_, token) = approve_again(&h, &main, id).await;
    let found = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(found["status"], "sent", "{found}");
    assert!(
        found["detail"]
            .as_str()
            .unwrap()
            .contains("already holds it"),
        "{found}"
    );
    let message_id = format!("draft-{}@owner.example", id.simple());
    assert_eq!(mail.searches(), [message_id.clone(), message_id]);
    assert_eq!(mail.sends().len(), 1, "found in Sent, so never sent again");
    assert_eq!(action_statuses(app, account).await, ["succeeded"]);
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "sent");
    assert_eq!(
        receipt["delivery"]["action_id"], found["sends"][0]["action_id"],
        "the card names the send that went out"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncertain_send_missing_from_sent_waits_for_a_new_approval() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    mail.fail_at(None);
    let (second, token) = approve_again(&h, &main, id).await;
    let absent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(absent["status"], "uncertain", "{absent}");
    assert!(
        absent["detail"]
            .as_str()
            .unwrap()
            .starts_with("Its Sent folder does not hold it"),
        "{absent}"
    );
    let again = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(
        again["status"], "uncertain",
        "the same approval never sends it"
    );
    let reconfirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert!(reconfirmed.error);
    assert_eq!(
        reconfirmed.result, "DRAFT_SETTLED: the draft is uncertain",
        "the owner message that led to the search cannot approve it again"
    );
    let earlier: Uuid = absent["sends"][0]["action_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let stale = outgoing_drafts::record_sent_check(
        &app.db.pool,
        id,
        Uuid::new_v4(),
        &[earlier],
        Ok(outgoing_drafts::SentCheck::Found),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(stale, DraftError::Settled(_)),
        "a search for another claim is refused: {stale:?}"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "uncertain", "and changes nothing");
    assert_eq!(action_statuses(app, account).await, ["uncertain"]);
    assert_eq!(mail.sends().len(), 1);
    assert_eq!(mail.searches().len(), 1);
    h.release(second).await;
    app.wait_turn(second, &["completed"]).await;

    let (_, token) = approve_again(&h, &main, id).await;
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "sent", "{sent}");
    assert_eq!(
        mail.searches().len(),
        1,
        "the owner approved after the search"
    );
    let sends = mail.sends();
    assert_eq!(sends.len(), 2);
    assert_eq!(
        sends[1].message_id, sends[0].message_id,
        "a duplicate is recognisable"
    );
    let attempts = sent["sends"].as_array().unwrap();
    assert_eq!(attempts.len(), 2, "{sent}");
    assert_eq!(
        (
            &attempts[0]["status"],
            &attempts[0]["phase"],
            &attempts[0]["sent_check"]
        ),
        (&json!("uncertain"), &json!("reply"), &json!("absent"))
    );
    assert_eq!(attempts[1]["status"], "succeeded");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsent_retry_keeps_the_earlier_send_uncertain() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    let (second, _) = approve_again(&h, &main, id).await;
    h.release(second).await;
    app.wait_turn(second, &["completed"]).await;
    let receipt = journaled(app, &main, id).await;
    assert_eq!(receipt["status"], "uncertain", "{receipt}");
    assert_eq!(
        receipt["delivery"]["action_id"], lost["sends"][0]["action_id"],
        "the card names the send that may have gone out"
    );

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
    let mut revision = connected_reply(account);
    revision["replaces"] = json!(id);
    let revised = tool(app, &token, "draft_email", revision).await;
    assert!(revised.error);
    assert_eq!(revised.result, "DRAFT_SETTLED: the draft is uncertain");
    assert_eq!(mail.sends().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncertain_send_without_a_message_id_is_never_sent_again() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    sqlx::query("UPDATE draft_mail_sends SET message_id = NULL")
        .execute(&app.db.pool)
        .await
        .unwrap();

    mail.fail_at(None);
    for _ in 0..2 {
        let (turn, token) = approve_again(&h, &main, id).await;
        let blocked = tool(app, &token, "send_draft", json!({ "draft_id": id }))
            .await
            .result;
        assert_eq!(blocked["status"], "uncertain", "{blocked}");
        assert!(
            blocked["detail"].as_str().unwrap().starts_with(
                "Its Sent folder was not searched: an earlier send carried no Message-ID"
            ),
            "{blocked}"
        );
        h.release(turn).await;
        app.wait_turn(turn, &["completed"]).await;
    }
    assert!(mail.searches().is_empty());
    assert_eq!(mail.sends().len(), 1, "no approval sends it again");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_file_on_a_retry_keeps_an_earlier_send_uncertain() {
    let root = tempfile::tempdir().unwrap();
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone()).projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    project(root.path(), &main, &[("report.pdf", PDF)]);
    let account = connect(app, &main, "main@owner.example", true).await;
    let (first, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut args = connected_reply(account);
    args["attachments"] = json!([{ "path": "report.pdf" }]);
    let id = draft(app, &token, args).await;
    h.release(first).await;
    app.wait_turn(first, &["waiting"]).await;
    let file_id = files_of(&shown(app, &main, first).await[0])[0]["id"].clone();
    let seen = json!({ "id": id, "version": 1, "attachments": [file_id] });
    let mut approval = seen.clone();
    approval.as_object_mut().unwrap().remove("id");
    owner_action(app, &main, id, "approve", approval).await;
    let token = h.continued(first, &token).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    let earlier = lost["sends"][0]["action_id"].clone();
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    let again = send_with_draft(app, &main, &format!("{GATE} Send it again."), seen).await;
    assert_eq!(again.status, StatusCode::ACCEPTED, "{}", again.body);
    let (_, token) = h
        .started(again.body["turn"]["id"].as_str().unwrap().parse().unwrap())
        .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    sqlx::query("UPDATE attachments SET data = zeroblob(size) WHERE id = ?1")
        .bind(file_id.as_str().unwrap().parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let commands = mail.commands.lock().unwrap().len();
    let retried = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(retried.result["status"], "uncertain", "{}", retried.body);
    assert_eq!(
        retried.result["detail"],
        "the attachment report.pdf changed or is missing since the owner approved it; nothing \
         was sent An earlier send of it may have gone out."
    );
    assert_eq!(
        mail.commands.lock().unwrap().len(),
        commands,
        "nothing was sent"
    );
    let delivery: String = sqlx::query_scalar("SELECT delivery FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&delivery).unwrap()["action_id"],
        earlier,
        "the draft names the send that may have gone out"
    );
}

/// A mail connector too busy to queue the send.
struct Busy;

#[async_trait::async_trait]
impl send::Connected for Busy {
    async fn send_mail(&self, _: Uuid, _: Uuid, _: MailCommand) -> Result<(), MailError> {
        Err(MailError::new(MailErrorCode::MailAccountBusy))
    }

    async fn find_sent(&self, _: Uuid, _: String) -> Result<bool, MailError> {
        unreachable!("no earlier send to search for")
    }

    async fn send_social(
        &self,
        _: Uuid,
        _: Uuid,
        _: else_api::app::outgoing::Platform,
        _: else_api::socials::types::SendMessage,
    ) -> Result<(), else_api::socials::types::Error> {
        unreachable!("these drafts are emails")
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_mail_send_fails_and_a_queued_copy_never_goes_out() {
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let bot: Uuid = main.parse().unwrap();
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, turn, _) = approved_connected_reply(&h, &main, account).await;

    let refused = send::send_connected(&app.db.pool, turn, id, MAIL, &Busy)
        .await
        .unwrap();
    assert_eq!(refused.status.as_str(), "failed");
    let Some(Delivery::Mail { action_id, .. }) = refused.delivery.as_deref().cloned() else {
        panic!("a connected email: {refused:?}");
    };
    let Content::Email(email) = &refused.content.0 else {
        panic!("an email: {refused:?}");
    };
    let queued = app
        .state
        .connectors
        .execute(
            account,
            bot,
            turn,
            Some(action_id),
            email.send_command(Vec::new(), Some(email.message_id(id))),
        )
        .await
        .unwrap_err();
    assert_eq!(queued.code, MailErrorCode::MailActionConflict, "{queued:?}");
    assert!(mail.sends().is_empty(), "the queued copy never went out");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_send_its_stopped_request_failed_is_never_admitted_after_the_request_resumes() {
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let bot: Uuid = main.parse().unwrap();
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, turn, _) = approved_connected_reply(&h, &main, account).await;
    let Claim::Fresh(claimed) = outgoing_drafts::claim_connected(&app.db.pool, turn, id, MAIL)
        .await
        .unwrap()
    else {
        panic!("the first call claims the email");
    };
    for status in [TurnStatus::Cancelling, TurnStatus::Running] {
        common::seed::force_status(&app.db.pool, turn, status).await;
    }
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "failed", "the stop failed the unsent email");
    let Payload::Mail(email) = claimed.payload else {
        panic!("an email");
    };
    let queued = app
        .state
        .connectors
        .execute(
            account,
            bot,
            turn,
            Some(claimed.action_id),
            email.send_command(Vec::new(), Some(email.message_id(id))),
        )
        .await
        .unwrap_err();
    assert_eq!(queued.code, MailErrorCode::MailActionConflict, "{queued:?}");
    assert!(mail.sends().is_empty(), "the queued copy never went out");
}

/// The request's first call claims the email, and a second call resumes the
/// claim and stops before reading the outcome.
async fn stalled_retry(app: &TestApp, turn: Uuid, id: Uuid) -> Uuid {
    let first = outgoing_drafts::claim_connected(&app.db.pool, turn, id, MAIL)
        .await
        .unwrap();
    assert!(matches!(first, Claim::Fresh(_)), "{first:?}");
    match outgoing_drafts::claim_connected(&app.db.pool, turn, id, MAIL)
        .await
        .unwrap()
    {
        Claim::Resumed(claimed) => claimed.action_id,
        other => panic!("the retry resumes the claim: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resumed_mail_retry_returns_the_receipt_its_claim_settled() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Envelope));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, turn, token) = approved_connected_reply(&h, &main, account).await;
    let claim = stalled_retry(app, turn, id).await;
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "failed", "{sent}");

    let next = outgoing_drafts::next_mail_send(&app.db.pool, id, claim, "draft@owner.example")
        .await
        .unwrap();
    let MailSend::Settled(draft) = next else {
        panic!("the claim's receipt: {next:?}");
    };
    assert_eq!(draft.status.as_str(), "failed");
    assert_eq!(
        json!(draft.delivery.unwrap().0)["action_id"],
        sent["sends"][0]["action_id"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_retry_never_reads_or_changes_a_later_claim() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Envelope));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let claim = stalled_retry(app, first, id).await;
    let failed = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(failed["status"], "failed", "{failed}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;
    mail.fail_at(None);
    let (_, token) = approve_again(&h, &main, id).await;
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "sent", "{sent}");

    let next = outgoing_drafts::next_mail_send(&app.db.pool, id, claim, "draft@owner.example")
        .await
        .unwrap_err();
    assert!(matches!(next, DraftError::Settled(_)), "{next:?}");
    let after =
        outgoing_drafts::after_mail_send(&app.db.pool, id, claim, Some("MAIL_ACCOUNT_BUSY"))
            .await
            .unwrap_err();
    assert!(matches!(after, DraftError::Settled(_)), "{after:?}");
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_after_the_earlier_send_succeeded_sends_nothing() {
    let mail = Arc::new(FakeMail::default());
    mail.fail_at(Some(SendPhase::Reply));
    let h = harness(TestApp::builder().mail(mail.clone())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let account = connect(app, &main, "main@owner.example", true).await;
    let (id, first, token) = approved_connected_reply(&h, &main, account).await;
    let lost = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(lost["status"], "uncertain", "{lost}");
    h.release(first).await;
    app.wait_turn(first, &["completed"]).await;

    mail.fail_at(None);
    let (_, token) = approve_again(&h, &main, id).await;
    sqlx::query("UPDATE connector_actions SET status = 'succeeded' WHERE account_id = ?1")
        .bind(account)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id }))
        .await
        .result;
    assert_eq!(sent["status"], "sent", "{sent}");
    assert_eq!(
        sent["detail"],
        "An earlier send of it went out. It was not sent again."
    );
    assert_eq!(mail.sends().len(), 1);
    assert!(mail.searches().is_empty());
}
