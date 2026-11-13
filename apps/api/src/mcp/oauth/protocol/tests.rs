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
