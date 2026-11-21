//! A Gmail inbox whose OAuth grant Google no longer accepts, as when a
//! refresh token expires, and the card that signs the owner in again.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_gmail_inbox_google_signed_out_reopens_the_sign_in_card() {
    let inbox = Arc::new(QuietInbox::default());
    let h = recorder::harness(common::TestApp::builder().mail(inbox.clone())).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h
        .running(&bot, &format!("{GATE} Search my Gmail inbox"))
        .await;
    let signed_in = |refresh_token: &str| Credential::OAuth2 {
        client_id: "desktop".into(),
        client_secret: None,
        refresh_token: refresh_token.into(),
        access_token: Some("private-access".into()),
        expires_at: Some(chrono::Utc::now().timestamp() + 3600),
    };
    let account = store::create(
        &app.db.pool,
        &app.state.secret_key,
        &Provider::Gmail.config("owner@gmail.com".into()).unwrap(),
        &signed_in("expired-refresh"),
    )
    .await
    .unwrap();
    app.state.connectors.changed(account.id).await;
    health_becomes(app, account.id, Health::Connected).await;
    let generation = store::account(&app.db.pool, account.id)
        .await
        .unwrap()
        .unwrap()
        .generation;
    let health = |code: &str| {
        let health = Health::from(code.to_owned());
        async move {
            store::set_health(&app.db.pool, account.id, generation, &health)
                .await
                .unwrap()
        }
    };
    assert!(health("oauth_reauthentication_required").await);

    let parked = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"gmail","address":"owner@gmail.com"}),
    )
    .await;
    assert_eq!(
        parked.result["status"], "pending_owner_auth",
        "an inbox Google signed out is not connected: {}",
        parked.body
    );
    let question = parked.result["question_id"].as_str().unwrap().to_owned();
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
    let context = card["context"].as_str().unwrap();
    assert!(
        context.contains("Sign in with Google as owner@gmail.com again."),
        "{context}"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let path = format!("/v1/bots/{bot}/questions/{question}/answer");
    let approve = || app.owner("POST", &path, Some(json!({"choice":"approve"})));
    assert_eq!(
        approve().await.body["error"]["code"],
        "inbox_not_connected",
        "the card waits for a new sign-in"
    );
    assert!(!health("oauth_unavailable").await);
    assert_eq!(
        approve().await.body["error"]["code"],
        "inbox_not_connected",
        "a network error does not hide the expired sign-in"
    );

    let start = app
        .owner(
            "POST",
            "/v1/connectors/email/oauth/start",
            Some(json!({
                "provider":"gmail", "address":"owner@gmail.com", "client_id":"desktop",
                "redirect_uri":"http://127.0.0.1:45678/google/callback"
            })),
        )
        .await;
    assert_eq!(
        start.status,
        StatusCode::OK,
        "the owner can sign in again: {}",
        start.body
    );

    let replaced = app
        .owner(
            "PUT",
            &format!("/v1/connectors/email/accounts/{}/credentials", account.id),
            Some(
                json!({"kind":"oauth2","client_id":"desktop","refresh_token":"new-refresh",
                "access_token":"private-access",
                "expires_at":chrono::Utc::now().timestamp() + 3600}),
            ),
        )
        .await;
    assert_eq!(replaced.status, StatusCode::OK, "{}", replaced.body);
    let answered = approve().await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    health_becomes(app, account.id, Health::Connected).await;
}

#[tokio::test]
async fn signing_in_again_takes_no_account_slot() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let app = &h.app;
    let key = &app.state.secret_key;
    let (config, credential) = gmail("owner@gmail.com");
    let expired = store::create(&app.db.pool, key, &config, &credential)
        .await
        .unwrap();
    for n in 1..store::MAX_ACCOUNTS {
        let mut config = super::config();
        config.address = format!("bot{n}@example.com");
        config.username = config.address.clone();
        store::create(
            &app.db.pool,
            key,
            &config,
            &Credential::Password {
                password: "private-app-password".into(),
            },
        )
        .await
        .unwrap();
    }
    let start = |address: &str| {
        app.owner(
            "POST",
            "/v1/connectors/email/oauth/start",
            Some(json!({
                "provider":"gmail", "address":address, "client_id":"desktop",
                "redirect_uri":"http://127.0.0.1:45678/google/callback"
            })),
        )
    };
    assert_eq!(
        start("new@gmail.com").await.body["error"]["code"],
        "connector_limit"
    );
    let health = Health::from("oauth_reauthentication_required".to_owned());
    assert!(
        store::set_health(&app.db.pool, expired.id, expired.generation, &health)
            .await
            .unwrap()
    );
    let renew = start("owner@gmail.com").await;
    assert_eq!(renew.status, StatusCode::OK, "{}", renew.body);
    assert_eq!(
        start("new@gmail.com").await.body["error"]["code"],
        "connector_limit",
        "a new address still needs a free slot"
    );
}
