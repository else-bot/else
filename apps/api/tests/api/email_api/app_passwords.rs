//! iCloud inbox cards and the app-specific passwords they save, including a

//! password the server later rejects.

use super::*;

/// Parks a running turn on an inbox card and returns its question ID.
async fn parked_card(app: &TestApp, token: &str, args: Value) -> (String, Value) {
    let parked = tool(app, token, "mail_connect", args).await;
    assert_eq!(
        parked.result["status"], "pending_owner_auth",
        "{}",
        parked.body
    );
    let question = parked.result["question_id"].as_str().unwrap().to_owned();
    let bot = app.main_bot().await;
    let card = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["questions"].as_array().unwrap().clone())
        .find(|q| q["id"] == question)
        .unwrap();
    (format!("/v1/bots/{bot}/questions/{question}/answer"), card)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_icloud_card_takes_an_app_password_and_reopens_in_the_same_turn_when_apple_rejects_it() {
    let inbox = Arc::new(QuietInbox::default());
    let h = recorder::harness_with(4, Some(inbox.clone())).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let approve = |path: String| async move {
        app.owner("POST", &path, Some(json!({"choice":"approve"})))
            .await
    };
    let (turn, token) = h
        .running(
            &bot,
            &format!("{GATE_EVERY_ATTEMPT} Connect my iCloud inbox"),
        )
        .await;
    let (path, card) = parked_card(
        app,
        &token,
        json!({"provider":"icloud","address":" Owner@iCloud.com "}),
    )
    .await;
    assert_eq!(
        card["action"]["email_connection"],
        json!({"provider":"icloud","address":"owner@icloud.com"})
    );
    let context = card["context"].as_str().unwrap();
    assert!(context.contains("app-specific password"), "{context}");
    h.release(turn).await;
    app.wait_turn_for(&turn.to_string(), &["waiting"], common::SETTLE)
        .await;
    assert_eq!(
        approve(path.clone()).await.body["error"]["code"],
        "inbox_not_connected"
    );
    store::create(
        &app.db.pool,
        &app.state.secret_key,
        &Provider::Icloud.config("other@icloud.com".into()).unwrap(),
        &Credential::Password {
            password: "other-app-password".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        approve(path.clone()).await.body["error"]["code"],
        "inbox_not_connected",
        "another iCloud inbox does not settle this card"
    );

    let config = Provider::Icloud.config("owner@icloud.com".into()).unwrap();
    assert_eq!(
        (
            config.imap.host.as_str(),
            config.imap.port,
            config.imap.start_tls
        ),
        ("imap.mail.me.com", 993, false)
    );
    assert_eq!(
        (
            config.smtp.host.as_str(),
            config.smtp.port,
            config.smtp.start_tls
        ),
        ("smtp.mail.me.com", 587, true)
    );
    let connect = |password: &str| {
        app.owner(
            "POST",
            "/v1/connectors/email/accounts",
            Some(json!({"config":config,"credential":{"kind":"password","password":password}})),
        )
    };
    let rejected = connect(REVOKED).await;
    assert_eq!(
        rejected.status,
        StatusCode::BAD_REQUEST,
        "{}",
        rejected.body
    );
    assert_eq!(rejected.body["error"]["code"], "mail_authentication_failed");
    assert!(
        rejected.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not your Apple Account password"),
        "{}",
        rejected.body
    );
    assert_eq!(
        approve(path.clone()).await.body["error"]["code"],
        "inbox_not_connected",
        "a rejected password connects nothing"
    );
    let created = connect("abcd-efgh-ijkl-mnop").await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    let account: Uuid = created.body["account"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let answered = approve(path).await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let token = h.continued(turn, &token).await;
    let known = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"icloud","address":"owner@icloud.com"}),
    )
    .await;
    assert_eq!(
        known.result["account_id"],
        account.to_string(),
        "{}",
        known.body
    );

    // The other inbox's watcher may have signalled ready first; wait for this
    // one's own connected write, or it would overwrite the rejection below.
    health_becomes(app, account, Health::Connected).await;
    let generation = store::account(&app.db.pool, account)
        .await
        .unwrap()
        .unwrap()
        .generation;
    let health = |health: Health| async move {
        store::set_health(&app.db.pool, account, generation, &health)
            .await
            .unwrap()
    };
    assert!(health(Health::CredentialRejected).await);
    let (path, card) = parked_card(
        app,
        &token,
        json!({"provider":"icloud","address":"owner@icloud.com"}),
    )
    .await;
    let context = card["context"].as_str().unwrap();
    assert!(
        context.contains("Apple rejected the saved password"),
        "{context}"
    );
    h.release(turn).await;
    app.wait_turn_for(&turn.to_string(), &["waiting"], common::SETTLE)
        .await;
    assert_eq!(
        approve(path.clone()).await.body["error"]["code"],
        "inbox_not_connected",
        "an inbox Apple rejects is not connected"
    );
    assert!(!health(Health::Failing("mail_transport_failed".into())).await);
    assert_eq!(
        approve(path.clone()).await.body["error"]["code"],
        "inbox_not_connected",
        "a network error does not clear a rejected password"
    );
    let credentials = format!("/v1/connectors/email/accounts/{account}/credentials");
    let replace = |password: &str| {
        app.owner(
            "PUT",
            &credentials,
            Some(json!({"kind":"password","password":password})),
        )
    };
    assert_eq!(replace(REVOKED).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(replace("qrst-uvwx-yzab-cdef").await.status, StatusCode::OK);
    let answered = approve(path).await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_inbox_password_reopens_the_card_until_a_sign_in_succeeds() {
    let inbox = Arc::new(QuietInbox::default());
    let h = recorder::harness_with(4, Some(inbox.clone())).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (_, token) = h.running(&bot, GATE).await;
    let account = store::create(
        &app.db.pool,
        &app.state.secret_key,
        &Provider::Icloud.config("owner@icloud.com".into()).unwrap(),
        &Credential::Password {
            password: "abcd-efgh-ijkl-mnop".into(),
        },
    )
    .await
    .unwrap();
    store::set_permissions(
        &app.db.pool,
        account.id,
        bot.parse().unwrap(),
        &permissions(),
    )
    .await
    .unwrap();
    let connect = || {
        tool(
            app,
            &token,
            "mail_connect",
            json!({"provider":"icloud","address":"owner@icloud.com"}),
        )
    };
    let folders = || {
        tool(
            app,
            &token,
            "mail_action",
            json!({"account_id":account.id,"command":{"operation":"folders"}}),
        )
    };
    assert!(!folders().await.error);
    tokio::time::timeout(common::SETTLE, inbox.ready.notified())
        .await
        .expect("the inbox is watched");
    health_becomes(app, account.id, Health::Connected).await;

    // The watch session signed in before the revocation and keeps waiting,
    // as an IDLE session does, so only the failed command shows it.
    inbox.revoked.store(true, Ordering::SeqCst);
    let failed = folders().await;
    assert!(failed.error, "{}", failed.body);
    assert_eq!(
        failed.code(),
        "mail_authentication_failed",
        "{}",
        failed.body
    );
    assert!(
        failed.body.to_string().contains("call mail_connect"),
        "{}",
        failed.body
    );
    let reopened = connect().await;
    assert_eq!(
        reopened.result["status"], "pending_owner_auth",
        "{}",
        reopened.body
    );

    inbox.revoked.store(false, Ordering::SeqCst);
    app.state.connectors.changed(account.id).await;
    health_becomes(app, account.id, Health::Connected).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scan_that_signed_in_before_revocation_cannot_hide_the_rejection() {
    let inbox = Arc::new(QuietInbox::default());
    let h = recorder::harness_with(4, Some(inbox.clone())).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (_, token) = h.running(&bot, GATE).await;
    let account = store::create(
        &app.db.pool,
        &app.state.secret_key,
        &Provider::Icloud.config("owner@icloud.com".into()).unwrap(),
        &Credential::Password {
            password: "abcd-efgh-ijkl-mnop".into(),
        },
    )
    .await
    .unwrap();
    store::set_permissions(
        &app.db.pool,
        account.id,
        bot.parse().unwrap(),
        &permissions(),
    )
    .await
    .unwrap();
    inbox.hold_scan.store(true, Ordering::SeqCst);
    app.state.connectors.changed(account.id).await;
    tokio::time::timeout(common::SETTLE, inbox.scan_held.notified())
        .await
        .expect("the watch signs in and scans");

    inbox.revoked.store(true, Ordering::SeqCst);
    let failed = tool(
        app,
        &token,
        "mail_action",
        json!({"account_id":account.id,"command":{"operation":"folders"}}),
    )
    .await;
    assert_eq!(
        failed.code(),
        "mail_authentication_failed",
        "{}",
        failed.body
    );
    let reopened = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"icloud","address":"owner@icloud.com"}),
    )
    .await;
    assert_eq!(
        reopened.result["status"], "pending_owner_auth",
        "{}",
        reopened.body
    );
    assert_eq!(
        store::account(&app.db.pool, account.id)
            .await
            .unwrap()
            .unwrap()
            .health,
        Health::CredentialRejected
    );
}
