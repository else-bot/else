use std::sync::Arc;

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, AuthorizationRequest,
    AuthorizationSession, CredentialStore, InMemoryCredentialStore, InMemoryStateStore,
    OAuthClientConfig, OAuthHttpClient, OAuthHttpClientFuture, OAuthHttpRequest, StateStore,
    default_oauth_http_client,
};

use super::store::{Credential, now};
use super::{Error, RegisteredClient};

#[derive(Clone)]
pub(super) struct Discovery {
    pub(super) resource: String,
    pub(super) challenge: Option<String>,
    pub(super) metadata: AuthorizationMetadata,
    pub(super) scopes: Vec<String>,
}

impl From<&Credential> for Discovery {
    fn from(credential: &Credential) -> Self {
        Self {
            resource: credential.resource.clone(),
            challenge: None,
            metadata: credential.metadata.clone(),
            scopes: credential.scopes.clone(),
        }
    }
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

/// A sign-in waiting for its callback. rmcp's in-memory stores hold only this
/// sign-in's PKCE state and tokens; the session persists the result.
pub(super) struct Flow {
    pub(super) discovery: Discovery,
    pub(super) session: AuthorizationSession,
    pub(super) states: InMemoryStateStore,
    pub(super) tokens: InMemoryCredentialStore,
    pub(super) resource: String,
    pub(super) client_secret: Option<String>,
}

pub(super) async fn begin(
    mut discovery: Discovery,
    registered: Option<&RegisteredClient>,
    redirect_uri: String,
) -> Result<Flow, Error> {
    let (mut manager, tokens, states) = manager(&discovery.resource, registered)
        .await
        .map_err(auth_error)?;
    // Discovering on the client that signs in lets rmcp choose the resource
    // the tokens are for.
    discovery.metadata = match registered {
        Some(client) => client.metadata(&discovery.resource)?,
        None => {
            manager
                .resolve_metadata_from_challenge(discovery.challenge.as_deref())
                .await
                .map_err(auth_error)?
                .metadata
        }
    };
    manager.set_metadata(discovery.metadata.clone());
    let mut request =
        AuthorizationRequest::new(redirect_uri.clone()).with_scopes(discovery.scopes.clone());
    let client_secret = match registered {
        Some(client) => {
            request = request
                .with_preregistered_client(client.client_id.clone())
                .with_client_secret(client.secret.as_str());
            None
        }
        None => {
            let scopes: Vec<&str> = discovery.scopes.iter().map(String::as_str).collect();
            let client = manager
                .register_client("else.bot", &redirect_uri, &scopes)
                .await
                .map_err(auth_error)?;
            request = request.with_preregistered_client(client.client_id);
            if let Some(secret) = &client.client_secret {
                request = request.with_client_secret(secret.as_str());
            }
            client.client_secret
        }
    };
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, error)| auth_error(error))?;
    // rmcp keeps that resource private; its authorization URL carries it.
    let resource = reqwest::Url::parse(session.get_authorization_url())
        .ok()
        .and_then(|url| {
            url.query_pairs()
                .find(|(name, _)| name == "resource")
                .map(|(_, value)| value.into_owned())
        })
        .unwrap_or_else(|| discovery.resource.clone());
    Ok(Flow {
        discovery,
        session,
        states,
        tokens,
        resource,
        client_secret,
    })
}

impl Flow {
    pub(super) fn authorization_url(&self) -> &str {
        self.session.get_authorization_url()
    }

    pub(super) async fn expects(&self, state: &str) -> bool {
        matches!(self.states.load(state).await, Ok(Some(_)))
    }

    pub(super) async fn finish(
        &self,
        code: &str,
        state: &str,
        issuer: Option<&str>,
    ) -> Result<Credential, Error> {
        self.session
            .handle_callback_with_issuer(code, state, issuer)
            .await
            .map_err(auth_error)?;
        let stored = self
            .tokens
            .load()
            .await
            .ok()
            .flatten()
            .ok_or(Error::Connection)?;
        Ok(Credential {
            resource: self.resource.clone(),
            metadata: self.discovery.metadata.clone(),
            client_id: stored.client_id,
            client_secret: self.client_secret.clone(),
            token: stored.token_response.ok_or(Error::Connection)?,
            scopes: stored.granted_scopes,
            received_at: stored.token_received_at.unwrap_or_else(now),
        })
    }
}

/// A flow for a client registered as `public-client`, started without
/// discovery or registration. Returns the state its callback must echo.
#[cfg(test)]
pub(super) async fn test_flow(discovery: Discovery) -> (Flow, String) {
    let (mut manager, tokens, states) = manager(&discovery.resource, None).await.unwrap();
    manager.set_metadata(discovery.metadata.clone());
    let request = AuthorizationRequest::new("http://127.0.0.1:12345/callback")
        .with_preregistered_client("public-client")
        .with_scopes(discovery.scopes.clone());
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, error)| error)
        .unwrap();
    let state = reqwest::Url::parse(session.get_authorization_url())
        .unwrap()
        .query_pairs()
        .find(|(name, _)| name == "state")
        .unwrap()
        .1
        .into_owned();
    let flow = Flow {
        resource: discovery.resource.clone(),
        discovery,
        session,
        states,
        tokens,
        client_secret: None,
    };
    (flow, state)
}

/// `registered` is the app that issued `credential`, if any.
pub(super) async fn refresh(
    credential: &Credential,
    registered: Option<&RegisteredClient>,
) -> Result<Credential, AuthError> {
    let (mut manager, tokens, _) = manager(&credential.resource, registered).await?;
    manager.set_metadata(credential.metadata.clone());
    let mut config =
        OAuthClientConfig::new(credential.client_id.clone(), credential.resource.clone());
    if let Some(secret) = registered
        .map(|client| client.secret.as_str())
        .or(credential.client_secret.as_deref())
    {
        config = config.with_client_secret(secret);
    }
    manager.configure_client(config)?;
    tokens.save(credential.stored()).await?;
    manager.refresh_token().await?;
    let stored = tokens
        .load()
        .await?
        .ok_or(AuthError::AuthorizationRequired)?;
    Ok(Credential {
        token: stored
            .token_response
            .ok_or(AuthError::AuthorizationRequired)?,
        scopes: stored.granted_scopes,
        received_at: stored.token_received_at.unwrap_or_else(now),
        ..credential.clone()
    })
}

/// A registered app's token requests go through [`GitHubErrors`]; its
/// endpoints are fixed, so they need no address filtering.
async fn manager(
    resource: &str,
    registered: Option<&RegisteredClient>,
) -> Result<
    (
        AuthorizationManager,
        InMemoryCredentialStore,
        InMemoryStateStore,
    ),
    AuthError,
> {
    let mut manager = match registered {
        Some(_) => {
            AuthorizationManager::new_with_oauth_http_client(
                resource,
                Arc::new(GitHubErrors(Box::new(default_oauth_http_client()?))),
            )
            .await?
        }
        None => AuthorizationManager::new(resource).await?,
    };
    let tokens = InMemoryCredentialStore::new();
    let states = InMemoryStateStore::new();
    manager.set_credential_store(tokens.clone());
    manager.set_state_store(states.clone());
    Ok((manager, tokens, states))
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
