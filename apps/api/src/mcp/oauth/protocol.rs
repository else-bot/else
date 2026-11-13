use std::sync::Arc;

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, InMemoryCredentialStore,
    InMemoryStateStore, OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRequest,
    default_oauth_http_client,
};

use super::Error;

#[derive(Clone)]
pub(super) struct Discovery {
    pub(super) resource: String,
    pub(super) challenge: Option<String>,
    pub(super) metadata: AuthorizationMetadata,
    pub(super) scopes: Vec<String>,
}

pub(super) async fn discover(resource: &str, challenge: &str) -> Result<Discovery, Error> {
    let challenge = Some(challenge.to_owned()).filter(|value| !value.is_empty());
    let mut manager = AuthorizationManager::new(resource)
        .await
        .map_err(auth_error)?;
    let metadata = manager
        .resolve_metadata_from_challenge(challenge.as_deref())
        .await
        .map_err(auth_error)?
        .metadata;
    manager.set_metadata(metadata.clone());
    let scopes = manager.select_initial_scopes(&[]).await;
    Ok(Discovery {
        resource: resource.into(),
        challenge,
        metadata,
        scopes,
    })
}

/// rmcp's client for one sign-in or refresh. Its stores hold only that
/// exchange's PKCE state and tokens; the session persists the result.
pub(super) struct Exchange {
    pub(super) manager: AuthorizationManager,
    pub(super) tokens: InMemoryCredentialStore,
    pub(super) states: InMemoryStateStore,
}

/// `github` routes the token requests of the registered GitHub app, whose
/// endpoints are fixed, through [`GitHubErrors`].
pub(super) async fn exchange(resource: &str, github: bool) -> Result<Exchange, AuthError> {
    let mut manager = if github {
        AuthorizationManager::new_with_oauth_http_client(
            resource,
            Arc::new(GitHubErrors(Box::new(default_oauth_http_client()?))),
        )
        .await?
    } else {
        AuthorizationManager::new(resource).await?
    };
    let tokens = InMemoryCredentialStore::new();
    let states = InMemoryStateStore::new();
    manager.set_credential_store(tokens.clone());
    manager.set_state_store(states.clone());
    Ok(Exchange {
        manager,
        tokens,
        states,
    })
}

/// GitHub rejects a revoked refresh token with HTTP 200 and
/// `bad_refresh_token`. rmcp asks for a new sign-in only on an RFC 6749
/// `invalid_grant` error, so this rewrites GitHub's reply into one.
struct GitHubErrors(Box<dyn OAuthHttpClient>);

impl OAuthHttpClient for GitHubErrors {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            let mut response = self.0.execute(request).await?;
            let revoked = serde_json::from_slice::<serde_json::Value>(response.body())
                .is_ok_and(|body| body["error"] == "bad_refresh_token");
            if revoked {
                *response.status_mut() = reqwest::StatusCode::BAD_REQUEST;
                *response.body_mut() = br#"{"error":"invalid_grant"}"#.to_vec();
            }
            Ok(response)
        })
    }
}

/// Provider error text can echo request fields, so only the kind is logged.
pub(super) fn auth_error(error: AuthError) -> Error {
    let (kind, mapped) = match error {
        AuthError::HttpError(_)
        | AuthError::DiscoveryRequestFailed { .. }
        | AuthError::TokenRefreshFailed(_)
        | AuthError::CredentialStoreError(_) => ("unreachable", Error::Connection),
        AuthError::RegistrationFailed(_) => (
            "registration",
            Error::Invalid(
                "The OAuth provider did not register else.bot as a client. It may need a pre-registered OAuth app.",
            ),
        ),
        AuthError::TokenExchangeFailed(_) => (
            "token_exchange",
            Error::Invalid("The OAuth provider rejected the sign-in. Try signing in again."),
        ),
        AuthError::PkceUnsupported => (
            "pkce",
            Error::Invalid("The OAuth provider does not support PKCE S256."),
        ),
        _ => (
            "metadata",
            Error::Invalid("The MCP's OAuth metadata is invalid or unavailable."),
        ),
    };
    tracing::warn!(kind, "MCP OAuth request failed");
    mapped
}

#[cfg(test)]
mod tests;
