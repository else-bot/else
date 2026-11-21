//! Connecting an account: the catalog, OAuth sign-in, and the owner's
//! credential routes.

use super::*;

#[tokio::test]
async fn the_connector_catalog_describes_email_sign_in_and_permissions() {
    let app = TestApp::installed().await;
    let catalog = app.owner("GET", "/v1/connectors", None).await;
    assert_eq!(catalog.status, StatusCode::OK);
    let email = &catalog.body["connectors"][0];
    assert_eq!(email["id"], "email");
    assert_eq!(email["version"], 1);
    assert_eq!(email["credentials"], "owner_only_write_only");
    assert_eq!(email["notifications"], "imap_idle");
    assert_eq!(
        email["permissions"],
        json!(["read", "write", "send", "wake"])
    );
    assert_eq!(
        email["providers"][0],
        json!({
            "provider": "gmail",
            "preset": {
                "provider": "gmail",
                "address": "you@example.com",
                "username": "you@example.com",
                "imap": {"host": "imap.gmail.com", "port": 993, "start_tls": false},
                "smtp": {"host": "smtp.gmail.com", "port": 465, "start_tls": false},
                "archive_folder": "[Gmail]/All Mail",
                "trash_folder": "[Gmail]/Trash",
                "drafts_folder": "[Gmail]/Drafts",
                "sent_folder": "[Gmail]/Sent Mail"
            },
            "authentication": ["oauth2"],
            "oauth_wizard": true
        })
    );
    assert_eq!(
        email["providers"][4],
        json!({"provider": "imap", "preset": null, "authentication": ["app_password"], "oauth_wizard": false})
    );
}

#[tokio::test]
async fn microsoft_desktop_sign_in_uses_public_client_pkce_and_outlook_scopes() {
    let app = TestApp::installed().await;
    let client_id = Uuid::new_v4().to_string();
    let started = app
        .owner(
            "POST",
            "/v1/connectors/email/oauth/start",
            Some(json!({
                "provider":"microsoft", "address":"owner@example.com", "client_id":client_id,
                "redirect_uri":"http://localhost:12345/"
            })),
        )
        .await;
    assert_eq!(started.status, StatusCode::OK, "{}", started.body);
    assert_eq!(started.body["expires_in"], 600);
    let url = reqwest::Url::parse(started.body["authorization_url"].as_str().unwrap()).unwrap();
    assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
    assert_eq!(url.path(), "/common/oauth2/v2.0/authorize");
    let params: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(params["client_id"], client_id);
    assert_eq!(params["login_hint"], "owner@example.com");
    assert_eq!(params["redirect_uri"], "http://localhost:12345/");
    assert_eq!(params["code_challenge_method"], "S256");
    assert!(!params.contains_key("client_secret"));
    let scopes: Vec<_> = params["scope"].split_whitespace().collect();
    for scope in [
        "offline_access",
        "https://outlook.office.com/IMAP.AccessAsUser.All",
        "https://outlook.office.com/SMTP.Send",
    ] {
        assert!(scopes.contains(&scope));
    }
}

#[tokio::test]
async fn desktop_oauth_can_choose_google_account_and_cancel_owner_only() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let bot = h.app.main_bot().await;
    let (_, task_token) = h.running(&bot, GATE).await;
    let start = h.app.owner("POST", "/v1/connectors/email/oauth/start", Some(json!({
        "provider":"gmail", "client_id":"desktop-client", "client_secret":"desktop-client-configuration",
        "redirect_uri":"http://127.0.0.1:45678/google/callback"
    }))).await;
    assert_eq!(start.status, StatusCode::OK, "{}", start.body);
    assert!(
        !start
            .body
            .to_string()
            .contains("desktop-client-configuration")
    );
    let path = format!(
        "/v1/connectors/email/oauth/{}",
        start.body["request_id"].as_str().unwrap()
    );
    assert_eq!(
        h.app
            .call("DELETE", &path, None, Some(&task_token))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for _ in 0..2 {
        assert_eq!(
            h.app.owner("DELETE", &path, None).await.status,
            StatusCode::OK
        );
    }
    let url = reqwest::Url::parse(start.body["authorization_url"].as_str().unwrap()).unwrap();
    let state = url
        .query_pairs()
        .find(|(key, _)| key == "state")
        .unwrap()
        .1
        .into_owned();
    let complete = h
        .app
        .owner(
            "POST",
            "/v1/connectors/email/oauth/complete",
            Some(json!({"request_id":start.body["request_id"],"state":state,"code":"unused"})),
        )
        .await;
    assert_eq!(complete.status, StatusCode::BAD_REQUEST);
    assert_eq!(complete.body["error"]["code"], "oauth_setup_expired");
}

#[tokio::test]
async fn owner_can_connect_pause_replace_credentials_and_disconnect_privately() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let app = &h.app;
    let config = config();
    let rejected = app
        .owner(
            "POST",
            "/v1/connectors/email/accounts",
            Some(json!({"config":config,"credential":{"kind":"password","password":REVOKED}})),
        )
        .await;
    assert_eq!(
        rejected.status,
        StatusCode::BAD_REQUEST,
        "{}",
        rejected.body
    );
    assert_eq!(rejected.body["error"]["code"], "mail_authentication_failed");
    assert!(
        app.owner("GET", "/v1/connectors/email/accounts", None)
            .await
            .body["accounts"]
            .as_array()
            .unwrap()
            .is_empty(),
        "a rejected password saves no account"
    );
    let created = app.owner("POST","/v1/connectors/email/accounts",Some(json!({"config":config,"credential":{"kind":"password","password":"private-app-password"}}))).await;
    assert_eq!(created.status, StatusCode::OK, "{}", created.body);
    assert!(!created.body.to_string().contains("private-app-password"));
    let duplicate = app.owner("POST", "/v1/connectors/email/accounts", Some(json!({"config":config,"credential":{"kind":"password","password":"private-app-password"}}))).await;
    assert_eq!(duplicate.status, StatusCode::CONFLICT);
    assert_eq!(duplicate.body["error"]["code"], "mail_account_exists");
    let path = format!(
        "/v1/connectors/email/accounts/{}",
        created.body["account"]["id"].as_str().unwrap()
    );
    assert_eq!(
        app.owner("PATCH", &path, Some(json!({"active":false})))
            .await
            .status,
        StatusCode::OK
    );
    let replaced = app
        .owner(
            "PUT",
            &format!("{path}/credentials"),
            Some(json!({"kind":"password","password":REVOKED})),
        )
        .await;
    assert_eq!(
        replaced.status,
        StatusCode::BAD_REQUEST,
        "{}",
        replaced.body
    );
    assert_eq!(
        app.owner(
            "PUT",
            &format!("{path}/credentials"),
            Some(json!({"kind":"password","password":"replacement-app-password"}))
        )
        .await
        .status,
        StatusCode::OK
    );
    let read = app.owner("GET", &path, None).await;
    assert_eq!(read.body["account"]["state"], "paused");
    assert!(read.body["grants"].as_array().unwrap().is_empty());
    assert!(!read.body.to_string().contains("replacement-app-password"));
    let mut config = read.body["account"]["config"].clone();
    config["archive_folder"] = json!("Archives");
    let saved = app
        .owner("PUT", &format!("{path}/config"), Some(config.clone()))
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        saved.body["account"]["config"]["archive_folder"],
        "Archives"
    );
    let again = app
        .owner("PUT", &format!("{path}/config"), Some(config.clone()))
        .await;
    assert_eq!(
        again.body["account"]["generation"],
        saved.body["account"]["generation"]
    );
    config["imap"]["host"] = json!("another.example.com");
    assert_eq!(
        app.owner("PUT", &format!("{path}/config"), Some(config))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        app.owner("DELETE", &path, None).await.status,
        StatusCode::OK
    );
    assert_eq!(
        app.owner("GET", &path, None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn owner_routes_never_return_credentials_and_task_tokens_cannot_manage_accounts() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let bot = h.app.main_bot().await;
    let (_, token) = h.running(&bot, GATE).await;
    let account = account(&h.app).await;
    let path = format!("/v1/connectors/email/accounts/{}", account.id);
    let listed = h
        .app
        .owner("GET", "/v1/connectors/email/accounts", None)
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert!(!listed.body.to_string().contains("private-app-password"));
    assert!(!listed.body.to_string().contains("credential"));
    assert_eq!(
        h.app.call("GET", &path, None, Some(&token)).await.status,
        StatusCode::FORBIDDEN
    );
    let default_access = tool(&h.app, &token, "mail_accounts", json!({})).await;
    assert_eq!(
        default_access.result["accounts"][0]["id"],
        json!(account.id)
    );
    assert_eq!(
        default_access.result["accounts"][0]["permissions"],
        json!({
            "read":true,"write":true,"send":true,"wake":false
        })
    );
    for _ in 0..2 {
        let saved = h
            .app
            .owner(
                "PUT",
                &format!("{path}/bots/{bot}"),
                Some(serde_json::to_value(permissions()).unwrap()),
            )
            .await;
        assert_eq!(saved.status, StatusCode::OK);
    }
    let visible = tool(&h.app, &token, "mail_accounts", json!({})).await;
    assert!(!visible.error, "{}", visible.body);
    assert_eq!(visible.result["accounts"][0]["id"], json!(account.id));
    for _ in 0..2 {
        let revoked = h
            .app
            .owner("DELETE", &format!("{path}/bots/{bot}"), None)
            .await;
        assert_eq!(revoked.status, StatusCode::OK);
    }
    assert!(
        tool(&h.app, &token, "mail_accounts", json!({}))
            .await
            .result["accounts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
