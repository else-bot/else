use super::*;
use axum::{Json, Router, routing::get};
use serde_json::json;

#[tokio::test]
async fn discovery_supports_oidc_path_insertion_and_challenge_scope_precedence() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let issuer = format!("{base}/tenant");
    let resource = format!("{base}/mcp");
    let resource_document = json!({"resource":resource,"authorization_servers":[issuer],"scopes_supported":["metadata:read"]});
    let server_document = json!({"issuer":issuer,"authorization_endpoint":format!("{base}/authorize"),"token_endpoint":format!("{base}/token"),"registration_endpoint":format!("{base}/register"),"code_challenge_methods_supported":["S256"]});
    let router = Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(move || async move { Json(resource_document) }),
        )
        .route(
            "/.well-known/openid-configuration/tenant",
            get(move || async move { Json(server_document) }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let found = discover(&resource, "Bearer scope=challenge:read")
        .await
        .unwrap();
    assert_eq!(found.metadata.issuer, Some(issuer));
    assert_eq!(found.scopes, ["challenge:read"]);
    server.abort();
}

#[tokio::test]
async fn discovery_requests_every_scope_of_a_large_provider_like_posthog() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let resource = format!("{base}/mcp");
    let scopes: Vec<String> = Vec::from_fn(157, |index| format!("resource_{index}:read"));
    assert!(scopes.join(" ").len() > 2048);
    let resource_document =
        json!({"resource":resource,"authorization_servers":[base],"scopes_supported":scopes});
    let server_document = json!({"issuer":base,"authorization_endpoint":format!("{base}/oauth/authorize/"),"token_endpoint":format!("{base}/oauth/token/"),"registration_endpoint":format!("{base}/oauth/register/"),"code_challenge_methods_supported":["S256"]});
    let router = Router::new()
        .route(
            "/.well-known/oauth-protected-resource/mcp",
            get(move || async move { Json(resource_document) }),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(move || async move { Json(server_document) }),
        );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let found = discover(
        &resource,
        &format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#),
    )
    .await
    .unwrap();
    assert_eq!(found.scopes, scopes);
    server.abort();
}

/// This machine's address on its private network, if it has one.
fn private_address() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("10.255.255.255:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    match ip {
        std::net::IpAddr::V4(v4) if v4.is_private() => Some(ip),
        _ => None,
    }
}

#[tokio::test]
async fn discovery_reaches_a_separate_authorization_server_on_the_same_private_network() {
    let Some(ip) = private_address() else {
        eprintln!("skipping: this machine has no private IPv4 address");
        return;
    };
    let resource_listener = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
    let issuer_listener = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
    let base = format!("http://{}", resource_listener.local_addr().unwrap());
    let issuer = format!("http://{}", issuer_listener.local_addr().unwrap());
    let resource = format!("{base}/mcp");
    let resource_document = json!({"resource":resource,"authorization_servers":[issuer],"scopes_supported":["orchestration:read"]});
    let server_document = json!({"issuer":issuer,"authorization_endpoint":format!("{issuer}/authorize"),"token_endpoint":format!("{issuer}/token"),"registration_endpoint":format!("{issuer}/register"),"code_challenge_methods_supported":["S256"]});
    let resource_router = Router::new().route(
        "/.well-known/oauth-protected-resource/mcp",
        get(move || async move { Json(resource_document) }),
    );
    let issuer_router = Router::new().route(
        "/.well-known/oauth-authorization-server",
        get(move || async move { Json(server_document) }),
    );
    let servers = [
        tokio::spawn(async move {
            axum::serve(resource_listener, resource_router)
                .await
                .unwrap()
        }),
        tokio::spawn(async move { axum::serve(issuer_listener, issuer_router).await.unwrap() }),
    ];
    let found = discover(
        &resource,
        &format!(r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#),
    )
    .await;
    servers.iter().for_each(tokio::task::JoinHandle::abort);
    let found = found.unwrap();
    assert_eq!(found.metadata.issuer, Some(issuer));
    assert_eq!(found.scopes, ["orchestration:read"]);
}
