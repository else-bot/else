use super::*;
use axum::{Json, Router, routing::get};
use serde_json::json;

#[tokio::test]
async fn successful_http_responses_with_oauth_errors_require_new_consent_or_server_setup() {
    for error in [
        "bad_refresh_token",
        "bad_verification_code",
        "invalid_grant",
        "incorrect_client_credentials",
        "unknown_error",
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let router = Router::new().route(
            "/token",
            axum::routing::post(move || async move {
                Json(json!({"error":error, "error_description":"private-provider-data"}))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let result = token(&endpoint, &[], None)
            .await
            .err()
            .expect("OAuth error");
        if matches!(
            error,
            "bad_refresh_token" | "bad_verification_code" | "invalid_grant"
        ) {
            assert!(matches!(result, Error::OAuthRevoked), "{result}");
        } else if error == "incorrect_client_credentials" {
            assert!(
                result.to_string().contains("client ID or secret"),
                "{result}"
            );
        }
        assert!(!result.to_string().contains("private-provider-data"));
        server.abort();
    }
}

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
