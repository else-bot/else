use super::*;
use axum::{Json, Router, routing::get};
use serde_json::json;

#[test]
fn challenge_fields_preserve_quoted_commas_and_reject_duplicate_parameters() {
    let fields = challenge_parameters(r#"Bearer realm="hello, world", resource_metadata="https://mcp.example.com/meta", scope="read:issues write:issues""#).unwrap();
    assert_eq!(fields["realm"], "hello, world");
    assert_eq!(fields["scope"], "read:issues write:issues");
    for scheme in ["BEARER", "bEaReR", "bearer"] {
        let fields = challenge_parameters(&format!(
            "{scheme} resource_metadata=\"https://mcp.example.com/meta\", scope=read:issues"
        ))
        .unwrap();
        assert_eq!(fields["resource_metadata"], "https://mcp.example.com/meta");
        assert_eq!(fields["scope"], "read:issues");
    }
    assert!(challenge_parameters(r#"Bearer scope="read", scope="write""#).is_err());
    assert!(challenge_parameters(r#"Bearer scope="unterminated"#).is_err());
}

#[test]
fn oauth_urls_reject_credentials_fragments_and_insecure_remote_endpoints() {
    for invalid in [
        "http://oauth.example.com/token",
        "http://localhost:1234/token",
        "https://user:secret@oauth.example.com/token",
        "https://oauth.example.com/token#secret",
        "file:///tmp/token",
    ] {
        assert!(safe_url(invalid).is_err(), "{invalid}");
    }
    assert!(safe_url("http://127.0.0.1:1234/token").is_ok());
    assert!(safe_url("http://[::1]:1234/token").is_ok());
    assert!(
        endpoint(
            "http://127.0.0.1:1234/token",
            &Url::parse("https://mcp.example.com/mcp").unwrap()
        )
        .is_err()
    );
}

#[test]
fn authorization_endpoints_cannot_inject_scopes_or_replace_pkce_and_callback_parameters() {
    for name in [
        "client_id",
        "client_secret",
        "redirect_uri",
        "response_type",
        "scope",
        "state",
        "code_challenge",
        "code_challenge_method",
        "resource",
        "access_token",
        "refresh_token",
    ] {
        let url = format!("https://oauth.example.com/authorize?{name}=injected");
        assert!(authorization_endpoint(&url).is_err(), "{name}");
    }
    assert!(authorization_endpoint("https://oauth.example.com/authorize?%73cope=admin").is_err());
    assert!(authorization_endpoint("https://oauth.example.com/authorize?tenant=example").is_ok());
}

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
    assert_eq!(found.issuer, issuer);
    assert_eq!(found.scopes, ["challenge:read"]);
    server.abort();
}
