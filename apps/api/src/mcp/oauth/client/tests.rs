use super::*;
use crate::mcp::Transport;
use crate::mcp::oauth::{Complete, Session, State, Store, fixtures};
use axum::{Form, Json, Router, http::HeaderMap, routing::post};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use uuid::Uuid;

fn github() -> Discovery {
    Discovery {
        resource: "https://api.githubcopilot.com/mcp/".into(),
        issuer: "https://github.com/login/oauth".into(),
        authorization_endpoint: "https://github.com/login/oauth/authorize".into(),
        token_endpoint: "https://github.com/login/oauth/access_token".into(),
        registration_endpoint: None,
        scopes: vec!["repo".into()],
        require_iss: true,
    }
}

#[test]
fn registered_credentials_are_bound_to_trusted_github_endpoints() {
    let client = RegisteredClient::github("registered-id".into(), "private-secret".into()).unwrap();
    let valid = github();
    client.validate(&valid).unwrap();
    for field in ["resource", "issuer", "authorization", "token"] {
        let mut changed = valid.clone();
        match field {
            "resource" => {
                changed.resource = "https://api.githubcopilot.com.attacker.test/mcp/".into()
            }
            "issuer" => changed.issuer = "https://attacker.test/login/oauth".into(),
            "authorization" => {
                changed.authorization_endpoint = "https://attacker.test/authorize".into()
            }
            "token" => changed.token_endpoint = "https://attacker.test/token".into(),
            _ => unreachable!(),
        }
        assert!(client.validate(&changed).is_err(), "{field}");
    }
    let mut insecure = valid.clone();
    insecure.resource = "http://api.githubcopilot.com/mcp/".into();
    assert!(client.validate(&insecure).is_err());
    let debug = format!("{client:?}");
    assert!(!debug.contains("private-secret"));
    assert!(!debug.contains("registered-id"));
}

#[tokio::test]
async fn compiled_registration_starts_signin_without_runtime_credentials() {
    use crate::config::{Config, tests::MapSource};
    let config = Config::load(&MapSource::new(&[("HOME", "/h")])).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Store::for_bot(
        dir.path().into(),
        Uuid::new_v4(),
        Some(Arc::new(fixtures::key())),
    );
    let mut session = Session::open(store, config.github_oauth.clone())
        .await
        .unwrap();
    session.state = State::Required(github());
    let started = session
        .authorize(
            &Transport::Http {
                secrets: Vec::new(),
                url: github().resource,
                headers: Default::default(),
            },
            "http://127.0.0.1:49177/mcp/callback".into(),
        )
        .await;
    if let Some(client) = config.github_oauth {
        let started = started.unwrap();
        let url = reqwest::Url::parse(&started.authorization_url).unwrap();
        let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], client.client_id);
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(!started.authorization_url.contains(client.secret.as_str()));
    } else {
        assert!(matches!(started, Err(Error::Invalid(_))));
    }
}

#[tokio::test]
async fn registered_signin_exchanges_and_refreshes_with_a_secret_without_registration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let mut discovery = github();
    discovery.resource = format!("{base}/mcp");
    discovery.issuer = base.clone();
    discovery.authorization_endpoint = format!("{base}/authorize");
    discovery.token_endpoint = format!("{base}/token");
    let registration = Arc::new(RegisteredClient {
        resource_origin: base,
        issuer: discovery.issuer.clone(),
        authorization_endpoint: discovery.authorization_endpoint.clone(),
        token_endpoint: discovery.token_endpoint.clone(),
        client_id: "registered-id".into(),
        secret: Zeroizing::new("private-secret".into()),
    });
    let (sent, mut received) = tokio::sync::mpsc::channel(4);
    let revoked = Arc::new(AtomicBool::new(false));
    let server_revoked = revoked.clone();
    let router = Router::new().route("/token", post(move |headers: HeaderMap, Form(fields): Form<HashMap<String, String>>| {
        let sent = sent.clone();
        let revoked = server_revoked.load(Ordering::Relaxed);
        async move {
            assert_eq!(headers["accept"], "application/json");
            assert_eq!(fields["client_id"], "registered-id");
            assert_eq!(fields["client_secret"], "private-secret");
            sent.send(fields).await.unwrap();
            if revoked {
                return Json(json!({"error":"bad_refresh_token"}));
            }
            Json(json!({"access_token":"private-access", "refresh_token":"private-refresh", "token_type":"bearer", "expires_in":3600}))
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let bot = Uuid::new_v4();
    let store = Store::for_bot(dir.path().into(), bot, Some(Arc::new(fixtures::key())));
    let mut session = Session::open(store.clone(), Some(registration.clone()))
        .await
        .unwrap();
    session.state = State::Required(discovery.clone());
    let started = session
        .authorize(
            &Transport::Http {
                secrets: Vec::new(),
                url: discovery.resource.clone(),
                headers: Default::default(),
            },
            "http://127.0.0.1:49177/mcp/callback".into(),
        )
        .await
        .unwrap();
    let url = reqwest::Url::parse(&started.authorization_url).unwrap();
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query["client_id"], "registered-id");
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(!started.authorization_url.contains("private-secret"));
    session
        .complete(Complete {
            request_id: started.request_id,
            state: query["state"].clone(),
            code: Some("private-code".into()),
            error: None,
            iss: Some(discovery.issuer.clone()),
        })
        .await
        .unwrap();
    let exchange = received.recv().await.unwrap();
    use base64::Engine;
    use sha2::Digest;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(exchange["code_verifier"].as_bytes()));
    assert_eq!(query["code_challenge"], challenge);
    assert_eq!(exchange["redirect_uri"], query["redirect_uri"]);
    assert_eq!(exchange["resource"], discovery.resource);
    assert_eq!(session.status().status, "connected");
    drop(session);
    let mut reopened = Session::open(store, Some(registration)).await.unwrap();
    reopened.refresh(true).await.unwrap();
    let refresh = received.recv().await.unwrap();
    assert_eq!(refresh["grant_type"], "refresh_token");
    assert_eq!(refresh["refresh_token"], "private-refresh");
    let safe = serde_json::to_string(&reopened.status()).unwrap();
    assert!(!safe.contains("private-secret"));
    assert!(!safe.contains("private-access"));
    assert!(!safe.contains("private-refresh"));
    let stored = std::fs::read_to_string(dir.path().join(format!("oauth-{bot}.json"))).unwrap();
    assert!(!stored.contains("private-secret"));
    assert!(!stored.contains("private-access"));
    revoked.store(true, Ordering::Relaxed);
    assert!(matches!(
        reopened.refresh(true).await,
        Err(Error::AuthRequired(_))
    ));
    assert_eq!(reopened.status().status, "reauth_required");
    assert!(!dir.path().join(format!("oauth-{bot}.json")).exists());
    server.abort();
}
