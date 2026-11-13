use super::fixtures::{credential, discovery, key, metadata};
use super::*;

/// A sign-in in progress against `discovery`, with the state its callback must echo.
async fn pending(previous: Option<Credential>, discovery: Discovery) -> (Box<Pending>, String) {
    let (flow, state) = protocol::test_flow(discovery).await;
    let pending = Pending {
        id: Uuid::new_v4(),
        flow,
        expires: Instant::now() + AUTH_LIFETIME,
        previous: previous.map(Box::new),
    };
    (Box::new(pending), state)
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

fn session(store: Store, state: State) -> Session {
    Session {
        store,
        state,
        connection: None,
        client: None,
    }
}

fn store(path: std::path::PathBuf) -> Store {
    Store::for_bot(path, Uuid::new_v4(), Some(std::sync::Arc::new(key())))
}

/// A local token endpoint for `discovery`, answering with `reply`.
async fn token_server(
    discovery: &mut Discovery,
    reply: axum::response::Response,
) -> tokio::task::JoinHandle<()> {
    use axum::{Router, routing::post};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    discovery.resource = format!("{base}/mcp");
    discovery.metadata = metadata("https://auth.example.com", &base);
    let (parts, body) = reply.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let router = Router::new().route(
        "/token",
        post(move || {
            let (parts, body) = (parts.clone(), body.clone());
            async move { axum::response::Response::from_parts(parts, axum::body::Body::from(body)) }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() })
}

#[tokio::test]
async fn github_signin_identifies_the_missing_server_registration() {
    let dir = tempfile::tempdir().unwrap();
    let mut session = Session::open(store(dir.path().into()), None).await.unwrap();
    let mut github = discovery();
    github.resource = "https://api.githubcopilot.com/mcp/".into();
    github.metadata = metadata(
        "https://github.com/login/oauth",
        "https://github.com/login/oauth",
    );
    session.state = State::Required(github);
    let error = session
        .authorize(
            &Transport::Http {
                secrets: Vec::new(),
                url: "https://api.githubcopilot.com/mcp/".into(),
                headers: Default::default(),
            },
            "http://127.0.0.1:49177/mcp/callback".into(),
        )
        .await
        .err()
        .expect("GitHub needs a registered app");
    assert!(
        error.to_string().contains("ELSE_GITHUB_CLIENT_ID"),
        "{error}"
    );
    assert!(
        error.to_string().contains("ELSE_GITHUB_CLIENT_SECRET"),
        "{error}"
    );
    assert_eq!(session.status().status, "disconnected");
}

#[tokio::test]
async fn wrong_state_and_issuer_do_not_consume_an_active_flow_and_denial_is_one_shot() {
    let dir = tempfile::tempdir().unwrap();
    let (pending, state) = pending(None, discovery()).await;
    let id = pending.id;
    let mut session = session(store(dir.path().into()), State::Authorizing(pending));
    assert!(session.complete(callback(id, "wrong-state")).await.is_err());
    assert_eq!(session.status().status, "authorizing");
    let mut wrong_issuer = callback(id, &state);
    wrong_issuer.error = None;
    wrong_issuer.code = Some("authorization-code".into());
    wrong_issuer.iss = Some("https://imposter.example.com".into());
    let error = session.complete(wrong_issuer).await.unwrap_err();
    assert!(error.to_string().contains("issuer"), "{error}");
    assert_eq!(session.status().status, "authorizing");
    for iss in [Some("https://imposter.example.com"), None] {
        let mut forged_denial = callback(id, &state);
        forged_denial.iss = iss.map(Into::into);
        assert!(session.complete(forged_denial).await.is_err());
        assert_eq!(session.status().status, "authorizing");
    }
    assert!(session.complete(callback(id, &state)).await.is_err());
    assert_eq!(session.status().status, "disconnected");
    assert!(session.complete(callback(id, &state)).await.is_err());
}

#[tokio::test]
async fn cancellation_and_expiration_restore_an_existing_account_without_exposing_credentials() {
    let (flow, _) = pending(Some(credential()), discovery()).await;
    let id = flow.id;
    let dir = tempfile::tempdir().unwrap();
    let mut session = session(store(dir.path().into()), State::Authorizing(flow));
    session.cancel(Uuid::new_v4());
    assert_eq!(session.status().status, "authorizing");
    session.cancel(id);
    assert_eq!(session.status().status, "connected");
    let safe = serde_json::to_string(&session.status()).unwrap();
    assert!(!safe.contains("private-access-token"));
    assert!(!safe.contains("private-refresh-token"));
    session.state = State::Authorizing(pending(Some(credential()), discovery()).await.0);
    assert_eq!(session.cancel_pending().status, "connected");
    session.state = State::Authorizing(pending(None, discovery()).await.0);
    assert_eq!(session.cancel_pending().status, "disconnected");
    let (mut flow, _) = pending(None, discovery()).await;
    flow.expires = Instant::now() - Duration::from_secs(1);
    session.state = State::Authorizing(flow);
    assert_eq!(session.status().status, "disconnected");
}

#[tokio::test]
async fn a_failed_credential_write_does_not_publish_refreshed_tokens() {
    use axum::{Json, response::IntoResponse};
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let blocked_path = dir.path().join("not-a-directory");
    std::fs::write(&blocked_path, "block persistence").unwrap();
    let mut discovery = discovery();
    let server = token_server(
        &mut discovery,
        Json(json!({"access_token":"new-access-token","refresh_token":"rotated-refresh-token","token_type":"Bearer","expires_in":3600})).into_response(),
    )
    .await;
    let mut previous = credential();
    previous.resource = discovery.resource;
    previous.metadata = discovery.metadata;
    previous.received_at = now() - 7200;
    let mut session = session(store(blocked_path), State::Authorized(Box::new(previous)));
    assert!(session.refresh(true).await.is_err());
    let State::Authorized(current) = &session.state else {
        panic!("existing credentials retained");
    };
    assert_eq!(
        current.token.access_token().secret(),
        "private-access-token"
    );
    assert_eq!(
        current.token.refresh_token().unwrap().secret(),
        "private-refresh-token"
    );
    assert!(current.expires_at().unwrap() < now());
    server.abort();
}

#[tokio::test]
async fn failed_reconnect_exchange_or_storage_keeps_the_existing_account() {
    use axum::{Json, http::StatusCode, response::IntoResponse};
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
        let mut discovery = discovery();
        discovery.scopes = vec!["new:scope".into()];
        let server = token_server(
            &mut discovery,
            if reject_exchange {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            } else {
                Json(json!({"access_token":"new-access-token","token_type":"bearer","expires_in":3600})).into_response()
            },
        )
        .await;
        let (flow, state) = pending(Some(credential()), discovery).await;
        let request = flow.id;
        let mut session = session(store(path), State::Authorizing(flow));
        let mut response = callback(request, &state);
        response.error = None;
        response.code = Some("authorization-code".into());
        assert!(session.complete(response).await.is_err());
        let status = session.status();
        assert_eq!(status.status, "connected");
        assert_eq!(status.scopes, ["read:issues"]);
        assert!(session.complete(callback(request, &state)).await.is_err());
        server.abort();
    }
}

fn github_session(credential: Credential, dir: &std::path::Path) -> Session {
    let mut session = session(store(dir.into()), State::Authorized(Box::new(credential)));
    session.client = Some(std::sync::Arc::new(
        RegisteredClient::github("registered-id".into(), "registered-secret".into()).unwrap(),
    ));
    session
}

fn github_credential(scopes: &[&str]) -> Credential {
    let mut credential = credential();
    credential.resource = "https://api.githubcopilot.com/mcp/".into();
    credential.metadata = metadata(
        "https://github.com/login/oauth",
        "https://github.com/login/oauth",
    );
    credential.scopes = scopes.iter().map(|s| (*s).to_owned()).collect();
    credential.client_id = "registered-id".into();
    credential
}

#[test]
fn git_gets_only_a_github_token_with_repo_scope() {
    let dir = tempfile::tempdir().unwrap();

    let github = github_session(github_credential(&["repo,read:org"]), dir.path());
    let token = github.github_token().unwrap();
    assert_eq!(token.as_str(), "private-access-token");

    let narrow = github_session(github_credential(&["read:org"]), dir.path());
    let error = narrow.github_token().unwrap_err();
    assert!(error.to_string().contains("repo scope"), "{error}");

    let mut other_app = github_credential(&["repo"]);
    other_app.client_id = "previous-id".into();
    let other_app = github_session(other_app, dir.path());
    assert!(other_app.github_token().is_err());

    let mut other_provider = github_session(credential(), dir.path());
    other_provider.client = None;
    let error = other_provider.github_token().unwrap_err();
    assert!(error.to_string().contains("no GitHub sign-in"), "{error}");
}

#[tokio::test]
async fn a_sign_in_saved_before_the_rmcp_client_still_refreshes() {
    use axum::{Form, Json, Router, routing::post};
    use serde_json::json;
    use std::collections::HashMap;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (sent, mut received) = tokio::sync::mpsc::channel(1);
    let router = Router::new().route(
        "/token",
        post(move |Form(fields): Form<HashMap<String, String>>| {
            let sent = sent.clone();
            async move {
                sent.send(fields).await.unwrap();
                Json(json!({"access_token":"fresh-access","token_type":"Bearer","expires_in":3600}))
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let key = std::sync::Arc::new(key());
    let legacy = json!({
        "discovery": {
            "resource": format!("{base}/mcp"),
            "issuer": base,
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "registration_endpoint": null,
            "scopes": ["read:issues"],
            "require_iss": true,
        },
        "client_id": "legacy-client",
        "access_token": "legacy-access",
        "refresh_token": "legacy-refresh",
        "expires_at": chrono::Utc::now().timestamp() - 10,
    });
    store::save(dir.path(), Uuid::nil(), &key, &legacy).unwrap();
    let store = Store::new(dir.path().into(), Some(key));
    let mut session = Session::open(store.clone(), None).await.unwrap();
    assert_eq!(session.status().status, "connected");
    assert_eq!(session.status().provider.as_deref(), Some(base.as_str()));
    session.refresh(false).await.unwrap();
    let refresh = received.recv().await.unwrap();
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["refresh_token"], "legacy-refresh");
    assert_eq!(refresh["client_id"], "legacy-client");
    assert_eq!(refresh["resource"], format!("{base}/mcp"));
    let saved = store.load().await.unwrap().unwrap();
    assert_eq!(saved.token.access_token().secret(), "fresh-access");
    assert_eq!(
        saved.token.refresh_token().unwrap().secret(),
        "legacy-refresh"
    );
    assert!(saved.expires_at().unwrap() > now() + 3000);
    server.abort();
}

#[tokio::test]
async fn sign_in_and_refresh_use_rmcps_resource_and_a_registration_secret() {
    use axum::{Form, Json, Router, routing::get, routing::post};
    use serde_json::json;
    use std::collections::HashMap;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let resource_document =
        json!({"resource": format!("{base}/"), "authorization_servers": [base]});
    let server_document = json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_post"],
    });
    let (sent, mut received) = tokio::sync::mpsc::channel(2);
    let router = Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(move || async move { Json(resource_document) }),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(move || async move { Json(server_document) }),
        )
        .route(
            "/register",
            post(|| async { Json(json!({"client_id":"dynamic-client","client_secret":"dynamic-secret","redirect_uris":[]})) }),
        )
        .route(
            "/token",
            post(move |Form(fields): Form<HashMap<String, String>>| {
                let sent = sent.clone();
                async move {
                    sent.send(fields).await.unwrap();
                    Json(json!({"access_token":"access","refresh_token":"refresh","token_type":"Bearer","expires_in":3600}))
                }
            }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let store = store(dir.path().into());
    let mut session = Session::open(store.clone(), None).await.unwrap();
    let url = format!("{base}/mcp");
    let challenge =
        format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource""#);
    session.state = State::Required(protocol::discover(&url, &challenge).await.unwrap());
    let started = session
        .authorize(
            &Transport::Http {
                secrets: Vec::new(),
                url,
                headers: Default::default(),
            },
            "http://127.0.0.1:49177/mcp/callback".into(),
        )
        .await
        .unwrap();
    let query: HashMap<_, _> = reqwest::Url::parse(&started.authorization_url)
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(query["resource"], format!("{base}/"));
    let mut response = callback(started.request_id, &query["state"]);
    response.error = None;
    response.code = Some("authorization-code".into());
    response.iss = Some(base.clone());
    session.complete(response).await.unwrap();
    let mut reopened = Session::open(store, None).await.unwrap();
    reopened.refresh(true).await.unwrap();
    for grant in ["authorization_code", "refresh_token"] {
        let fields = received.recv().await.unwrap();
        assert_eq!(fields["grant_type"], grant);
        assert_eq!(fields["resource"], format!("{base}/"));
        assert_eq!(fields["client_id"], "dynamic-client");
        assert_eq!(fields["client_secret"], "dynamic-secret");
    }
    server.abort();
}
