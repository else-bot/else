use super::{now, protocol::Discovery, store::Credential};
use crate::credentials::SecretKey;

pub(super) fn key() -> SecretKey {
    SecretKey::parse("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()
}

pub(super) fn discovery() -> Discovery {
    Discovery {
        resource: "https://mcp.example.com/mcp".into(),
        issuer: "https://auth.example.com".into(),
        authorization_endpoint: "https://auth.example.com/authorize".into(),
        token_endpoint: "https://auth.example.com/token".into(),
        registration_endpoint: Some("https://auth.example.com/register".into()),
        scopes: vec!["read:issues".into()],
        require_iss: true,
    }
}

pub(super) fn credential() -> Credential {
    Credential {
        discovery: discovery(),
        client_id: "public-client".into(),
        access_token: "private-access-token".into(),
        refresh_token: Some("private-refresh-token".into()),
        expires_at: Some(now() + 3600),
    }
}
