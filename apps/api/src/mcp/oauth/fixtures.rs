use rmcp::transport::auth::{AuthorizationMetadata, OAuthTokenResponse};
use serde_json::json;

use super::{protocol::Discovery, store::Credential, store::now};
use crate::credentials::SecretKey;

pub(super) fn key() -> SecretKey {
    SecretKey::parse("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()
}

pub(super) fn metadata(issuer: &str, endpoints: &str) -> AuthorizationMetadata {
    serde_json::from_value(json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{endpoints}/authorize"),
        "token_endpoint": format!("{endpoints}/token"),
        "registration_endpoint": format!("{endpoints}/register"),
        "code_challenge_methods_supported": ["S256"],
        "authorization_response_iss_parameter_supported": true,
    }))
    .unwrap()
}

pub(super) fn discovery() -> Discovery {
    Discovery {
        resource: "https://mcp.example.com/mcp".into(),
        metadata: metadata("https://auth.example.com", "https://auth.example.com"),
        scopes: vec!["read:issues".into()],
    }
}

pub(super) fn token(access: &str, refresh: Option<&str>, ttl: u64) -> OAuthTokenResponse {
    serde_json::from_value(json!({
        "access_token": access,
        "refresh_token": refresh,
        "token_type": "bearer",
        "expires_in": ttl,
    }))
    .unwrap()
}

pub(super) fn credential() -> Credential {
    let discovery = discovery();
    Credential {
        resource: discovery.resource,
        metadata: discovery.metadata,
        client_id: "public-client".into(),
        client_secret: None,
        token: token("private-access-token", Some("private-refresh-token"), 3600),
        scopes: discovery.scopes,
        received_at: now(),
    }
}
