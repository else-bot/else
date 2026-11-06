use super::fixtures::{credential, discovery, key};
use super::*;

fn pending(previous: Option<Credential>) -> Pending {
    Pending {
        id: Uuid::new_v4(),
        discovery: discovery(),
        client_id: "public-client".into(),
        redirect_uri: "http://127.0.0.1:12345/callback".into(),
        state_hash: Sha256::digest(b"expected-state").into(),
        verifier: Zeroizing::new("private-pkce-verifier".into()),
        expires: Instant::now() + AUTH_LIFETIME,
        previous,
    }
}

fn callback(id: Uuid, state: &str) -> Complete {
    Complete {
        request_id: id,
        state: state.into(),
        code: None,
        error: Some("access_denied".into()),
        iss: Some("https://auth.example.com".into()),
    }
}

#[tokio::test]
async fn github_signin_identifies_the_missing_server_registration() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(Store::new(
        dir.path().into(),
        Uuid::new_v4(),
        Some(std::sync::Arc::new(key())),
    ))
    .await
    .unwrap();
    let mut github = discovery();
    github.resource = "https://api.githubcopilot.com/mcp/".into();
    github.issuer = "https://github.com/login/oauth".into();
    github.authorization_endpoint = "https://github.com/login/oauth/authorize".into();
    github.token_endpoint = "https://github.com/login/oauth/access_token".into();
    github.registration_endpoint = None;
    session.state = State::Required(github);
    let error = session
        .authorize(
            &Transport::Http {
                url: "https://api.githubcopilot.com/mcp/".into(),
                headers: Default::default(),
            },
            "http://127.0.0.1:49177/mcp/callback".into(),
        )
        .await
        .err()
        .expect("GitHub needs a registered app");
    assert!(error.to_string().contains("ELSE_GITHUB_CLIENT_ID"), "{error}");
    assert!(error.to_string().contains("ELSE_GITHUB_CLIENT_SECRET"), "{error}");
    assert_eq!(session.status().status, "disconnected");
}

#[tokio::test]
async fn wrong_state_and_issuer_do_not_consume_an_active_flow_and_denial_is_one_shot() {
    let dir = tempfile::tempdir().unwrap();
    let pending = pending(None);
    let id = pending.id;
    let bot = Uuid::new_v4();
    let key = key();
    let mut session = Session {
        store: Store::new(dir.path().into(), bot, Some(std::sync::Arc::new(key))),
        state: State::Authorizing(pending),
        connection: None,
    };
    assert!(session.complete(callback(id, "wrong-state")).await.is_err());
    assert_eq!(session.status().status, "authorizing");
    let mut wrong_issuer = callback(id, "expected-state");
    wrong_issuer.iss = Some("https://imposter.example.com".into());
    assert!(session.complete(wrong_issuer).await.is_err());
    assert_eq!(session.status().status, "authorizing");
    assert!(
        session
            .complete(callback(id, "expected-state"))
            .await
            .is_err()
    );
    assert_eq!(session.status().status, "disconnected");
    assert!(
        session
            .complete(callback(id, "expected-state"))
            .await
            .is_err()
    );
}

#[test]
fn cancellation_and_expiration_restore_an_existing_account_without_exposing_credentials() {
    let flow = pending(Some(credential()));
    let id = flow.id;
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session {
        store: Store::new(
            dir.path().into(),
            Uuid::new_v4(),
            Some(std::sync::Arc::new(key())),
        ),
        state: State::Authorizing(flow),
        connection: None,
    };
    session.cancel(Uuid::new_v4());
    assert_eq!(session.status().status, "authorizing");
    session.cancel(id);
    assert_eq!(session.status().status, "connected");
    let safe = serde_json::to_string(&session.status()).unwrap();
    assert!(!safe.contains("private-access-token"));
    assert!(!safe.contains("private-refresh-token"));
    session.state = State::Authorizing(pending(Some(credential())));
    assert_eq!(session.cancel_pending().status, "connected");
    session.state = State::Authorizing(pending(None));
    assert_eq!(session.cancel_pending().status, "disconnected");
    let mut pending = pending(None);
    pending.expires = Instant::now() - Duration::from_secs(1);
    session.state = State::Authorizing(pending);
    assert_eq!(session.status().status, "disconnected");
}

#[tokio::test]
async fn a_failed_credential_write_does_not_publish_refreshed_tokens() {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let blocked_path = dir.path().join("not-a-directory");
    std::fs::write(&blocked_path, "block persistence").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let router = Router::new().route("/token", post(|| async {
        Json(json!({"access_token":"new-access-token","refresh_token":"rotated-refresh-token","token_type":"Bearer","expires_in":3600}))
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut previous = credential();
    previous.discovery.token_endpoint = endpoint;
    previous.expires_at = Some(now() - 1);
    let mut session = Session {
        store: Store::new(
            blocked_path,
            Uuid::new_v4(),
            Some(std::sync::Arc::new(key())),
        ),
        state: State::Authorized(previous),
        connection: None,
    };
    assert!(session.refresh(true).await.is_err());
    let State::Authorized(current) = &session.state else {
        panic!("existing credentials retained");
    };
    assert_eq!(current.access_token, "private-access-token");
    assert_eq!(
        current.refresh_token.as_deref(),
        Some("private-refresh-token")
    );
    assert!(current.expires_at.unwrap() < now());
    server.abort();
}

#[tokio::test]
async fn failed_reconnect_exchange_or_storage_keeps_the_existing_account() {
    use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::post};
    use serde_json::json;
    for reject_exchange in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = if reject_exchange {
            dir.path().join("fixture")
        } else {
            dir.path().join("blocked")
        };
        if reject_exchange {
            std::fs::create_dir(&path).unwrap();
        } else {
            std::fs::write(&path, "not a directory").unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut flow = pending(Some(credential()));
        flow.discovery.token_endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        flow.discovery.scopes = vec!["new:scope".into()];
        let request = flow.id;
        let router = Router::new().route("/token", post(move || async move {
            if reject_exchange { StatusCode::SERVICE_UNAVAILABLE.into_response() }
            else { Json(json!({"access_token":"new-access-token","token_type":"bearer","expires_in":3600})).into_response() }
        }));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut session = Session {
            store: Store::new(path, Uuid::new_v4(), Some(std::sync::Arc::new(key()))),
            state: State::Authorizing(flow),
            connection: None,
        };
        let mut response = callback(request, "expected-state");
        response.error = None;
        response.code = Some("authorization-code".into());
        assert!(session.complete(response).await.is_err());
        let status = session.status();
        assert_eq!(status.status, "connected");
        assert_eq!(status.scopes, ["read:issues"]);
        assert!(
            session
                .complete(callback(request, "expected-state"))
                .await
                .is_err()
        );
        server.abort();
    }
}
