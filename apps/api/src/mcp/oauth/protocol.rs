use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, InMemoryCredentialStore,
    InMemoryStateStore,
};

use super::Error;

/// What a sign-in will ask the provider for.
#[derive(Clone)]
pub(super) struct Discovery {
    pub(super) resource: String,
    pub(super) metadata: AuthorizationMetadata,
    pub(super) scopes: Vec<String>,
}

pub(super) async fn discover(resource: &str, challenge: &str) -> Result<Discovery, Error> {
    let mut manager = AuthorizationManager::new(resource)
        .await
        .map_err(auth_error)?;
    let metadata = manager
        .resolve_metadata_from_challenge(Some(challenge).filter(|value| !value.is_empty()))
        .await
        .map_err(auth_error)?
        .metadata;
    manager.set_metadata(metadata.clone());
    let scopes = manager.select_initial_scopes(&[]).await;
    Ok(Discovery {
        resource: resource.into(),
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

pub(super) async fn exchange(
    resource: &str,
    metadata: AuthorizationMetadata,
) -> Result<Exchange, AuthError> {
    let mut manager = AuthorizationManager::new(resource).await?;
    manager.set_metadata(metadata);
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

pub(super) fn auth_error(error: AuthError) -> Error {
    tracing::warn!(%error, "MCP OAuth request failed");
    match error {
        AuthError::HttpError(_)
        | AuthError::DiscoveryRequestFailed { .. }
        | AuthError::TokenRefreshFailed(_)
        | AuthError::CredentialStoreError(_) => Error::Connection,
        AuthError::RegistrationFailed(_) => Error::Invalid(
            "The OAuth provider did not register else.bot as a client. It may need a pre-registered OAuth app.",
        ),
        AuthError::TokenExchangeFailed(_) => {
            Error::Invalid("The OAuth provider rejected the sign-in. Try signing in again.")
        }
        AuthError::AuthorizationServerMismatch { .. }
        | AuthError::AuthorizationServerMissingIssuer { .. } => {
            Error::Invalid("The MCP sign-in callback issuer is invalid.")
        }
        AuthError::PkceUnsupported => {
            Error::Invalid("The OAuth provider does not support PKCE S256.")
        }
        _ => Error::Invalid("The MCP's OAuth metadata is invalid or unavailable."),
    }
}

#[cfg(test)]
mod tests;
