mod client;
mod protocol;
mod store;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oauth2::TokenResponse;
use rmcp::transport::auth::{
    AuthError, AuthorizationMetadata, AuthorizationRequest, AuthorizationSession, CredentialStore,
    InMemoryCredentialStore, InMemoryStateStore, OAuthClientConfig, StateStore,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{Error, Transport, transport};
pub use client::RegisteredClient;
use protocol::{Discovery, auth_error, discover, exchange};
pub(super) use store::Store;
use store::{Credential, now};

const AUTH_LIFETIME: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, Serialize)]
pub struct ConnectionStatus {
    pub status: &'static str,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Serialize)]
pub struct Started {
    pub request_id: Uuid,
    pub authorization_url: String,
    pub expires_in: u64,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Complete {
    pub request_id: Uuid,
    pub state: String,
    pub code: Option<String>,
    pub error: Option<String>,
    pub iss: Option<String>,
}

struct Pending {
    id: Uuid,
    discovery: Discovery,
    resource: String,
    client_secret: Option<String>,
    session: AuthorizationSession,
    tokens: InMemoryCredentialStore,
    states: InMemoryStateStore,
    expires: Instant,
    previous: Option<Box<Credential>>,
}

enum State {
    Disconnected,
    Required(Discovery),
    Authorizing(Box<Pending>),
    Authorized(Box<Credential>),
    ReauthRequired(Discovery),
}

pub(super) struct Session {
    store: Store,
    client: Option<Arc<RegisteredClient>>,
    state: State,
    connection: Option<transport::Connection>,
}

impl Session {
    pub(super) async fn open(
        store: Store,
        client: Option<Arc<RegisteredClient>>,
    ) -> Result<Self, Error> {
        let state = store
            .load()
            .await?
            .map_or(State::Disconnected, |credential| {
                State::Authorized(Box::new(credential))
            });
        Ok(Self {
            store,
            client,
            state,
            connection: None,
        })
    }

    pub(super) fn status(&mut self) -> ConnectionStatus {
        if let State::Authorizing(pending) = &self.state
            && pending.expires <= Instant::now()
        {
            let id = pending.id;
            self.cancel(id);
        }
        let (status, metadata, scopes): (_, Option<&AuthorizationMetadata>, &[String]) =
            match &self.state {
                State::Disconnected => (
                    if self.connection.is_some() {
                        "connected"
                    } else {
                        "disconnected"
                    },
                    None,
                    &[],
                ),
                State::Required(discovery) => {
                    ("disconnected", Some(&discovery.metadata), &discovery.scopes)
                }
                State::Authorizing(pending) => (
                    "authorizing",
                    Some(&pending.discovery.metadata),
                    &pending.discovery.scopes,
                ),
                State::Authorized(credential) => {
                    let expired = credential.expires_at().is_some_and(|at| at <= now());
                    (
                        if expired && credential.token.refresh_token().is_none() {
                            "reauth_required"
                        } else {
                            "connected"
                        },
                        Some(&credential.metadata),
                        &credential.scopes,
                    )
                }
                State::ReauthRequired(discovery) => (
                    "reauth_required",
                    Some(&discovery.metadata),
                    &discovery.scopes,
                ),
            };
        ConnectionStatus {
            status,
            provider: metadata.and_then(|metadata| metadata.issuer.clone()),
            scopes: scopes.to_vec(),
        }
    }

    pub(super) async fn request(
        &mut self,
        config: &Transport,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        if !transport::METHODS.contains(&method.as_str()) || !params.is_object() {
            return Err(Error::Invalid(
                "Use a supported MCP method with object params.",
            ));
        }
        self.status();
        if matches!(
            self.state,
            State::Required(_) | State::Authorizing(_) | State::ReauthRequired(_)
        ) {
            return Err(Error::AuthRequired(Box::new(self.status())));
        }
        self.refresh(false).await?;
        let mut retried = false;
        loop {
            let result = match self.ensure_connection(config).await {
                Ok(()) => {
                    self.connection
                        .as_mut()
                        .expect("connected MCP")
                        .request(method.clone(), params.clone())
                        .await
                }
                Err(error) => Err(error),
            };
            match result {
                Err(Error::AuthChallenge(challenge)) => {
                    self.close().await;
                    if !retried
                        && matches!(self.state, State::Authorized(_))
                        && method != "tools/call"
                    {
                        retried = true;
                        match self.refresh(true).await {
                            Ok(()) | Err(Error::AuthRequired(_)) => {}
                            Err(error) => return Err(error),
                        }
                        if matches!(self.state, State::Authorized(_)) {
                            continue;
                        }
                    }
                    let Transport::Http { url, .. } = config else {
                        return Err(Error::Connection);
                    };
                    let mut discovery = discover(url, &challenge).await?;
                    let was_authorized =
                        matches!(self.state, State::Authorized(_) | State::ReauthRequired(_));
                    let previous_scopes: &[String] = match &self.state {
                        State::Authorized(credential) => &credential.scopes,
                        State::ReauthRequired(discovery) => &discovery.scopes,
                        _ => &[],
                    };
                    for scope in previous_scopes {
                        if !discovery.scopes.contains(scope) {
                            discovery.scopes.push(scope.clone());
                        }
                    }
                    self.store.remove().await?;
                    self.state = if was_authorized {
                        State::ReauthRequired(discovery)
                    } else {
                        State::Required(discovery)
                    };
                    return Err(Error::AuthRequired(Box::new(self.status())));
                }
                Err(error @ (Error::Connection | Error::Http(_) | Error::Timeout)) => {
                    self.close().await;
                    return Err(error);
                }
                other => return other,
            }
        }
    }

    /// The access token of a GitHub sign-in made through the server's
    /// registered GitHub app, for authenticating git over HTTPS. A read first
    /// refreshes the token if needed, proves it still works, and turns a
    /// revoked one into `AuthRequired`.
    pub(super) async fn git_token(
        &mut self,
        config: &Transport,
    ) -> Result<Zeroizing<String>, Error> {
        self.request(config, "tools/list".into(), serde_json::json!({}))
            .await?;
        self.github_token()
    }

    /// Any other provider's token is refused, so it can never reach github.com.
    fn github_token(&self) -> Result<Zeroizing<String>, Error> {
        let not_github = Error::Invalid("This MCP has no GitHub sign-in. Use the GitHub MCP's id.");
        let (State::Authorized(credential), Some(client)) = (&self.state, self.client.as_deref())
        else {
            return Err(not_github);
        };
        if !client.applies(&credential.metadata) || client.client_id != credential.client_id {
            return Err(not_github);
        }
        // GitHub reports the granted scopes as one comma-separated value.
        if !credential
            .scopes
            .iter()
            .flat_map(|scope| scope.split(','))
            .any(|scope| scope == "repo")
        {
            return Err(Error::Invalid(
                "This GitHub sign-in lacks the repo scope git needs. The owner must disconnect it and sign in again.",
            ));
        }
        Ok(Zeroizing::new(
            credential.token.access_token().secret().clone(),
        ))
    }

    async fn ensure_connection(&mut self, config: &Transport) -> Result<(), Error> {
        if self.connection.is_none() {
            let mut config = config.clone();
            if let State::Authorized(credential) = &self.state
                && let Transport::Http { headers, .. } = &mut config
            {
                headers.retain(|name, _| !name.eq_ignore_ascii_case("authorization"));
                headers.insert(
                    "Authorization".into(),
                    format!("Bearer {}", credential.token.access_token().secret()),
                );
            }
            self.connection = Some(
                transport::connect(
                    &config,
                    self.store.path(),
                    self.store
                        .path()
                        .parent()
                        .and_then(Path::parent)
                        .expect("MCP data directory"),
                )
                .await?,
            );
        }
        Ok(())
    }

    pub(super) async fn authorize(
        &mut self,
        config: &Transport,
        redirect_uri: String,
    ) -> Result<Started, Error> {
        self.store.require_key()?;
        let redirect = reqwest::Url::parse(&redirect_uri).ok().filter(|url| {
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        });
        if redirect.is_none() {
            return Err(Error::Invalid(
                "MCP sign-in needs a local loopback callback URL.",
            ));
        }
        self.status();
        let mut discovery = match &self.state {
            State::Authorizing(_) => {
                return Err(Error::Invalid("This MCP sign-in is already in progress."));
            }
            State::Required(discovery) | State::ReauthRequired(discovery) => discovery.clone(),
            State::Authorized(credential) => Discovery {
                resource: credential.resource.clone(),
                challenge: None,
                metadata: credential.metadata.clone(),
                scopes: credential.scopes.clone(),
            },
            State::Disconnected => {
                let Transport::Http { url, .. } = config else {
                    return Err(Error::Invalid("This MCP does not use HTTP OAuth."));
                };
                let path = self.store.path();
                let data_dir = path
                    .parent()
                    .and_then(Path::parent)
                    .expect("MCP data directory");
                match transport::connect(config, path, data_dir).await {
                    Err(Error::AuthChallenge(challenge)) => discover(url, &challenge).await?,
                    Ok(mut connection) => {
                        connection.shutdown().await;
                        return Err(Error::Invalid(
                            "This MCP is already accessible without OAuth sign-in.",
                        ));
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        let registered = self.registered_client(&discovery.metadata)?;
        let mut exchange = exchange(&discovery.resource, registered.is_some())
            .await
            .map_err(auth_error)?;
        // Discovering on the client that signs in lets rmcp choose the
        // resource the tokens are for.
        discovery.metadata = match registered {
            Some(client) => client.metadata(&discovery.resource)?,
            None => {
                exchange
                    .manager
                    .resolve_metadata_from_challenge(discovery.challenge.as_deref())
                    .await
                    .map_err(auth_error)?
                    .metadata
            }
        };
        exchange.manager.set_metadata(discovery.metadata.clone());
        let (client_id, client_secret) = match registered {
            Some(client) => (client.client_id.clone(), None),
            None => {
                let scopes: Vec<&str> = discovery.scopes.iter().map(String::as_str).collect();
                let client = exchange
                    .manager
                    .register_client("else.bot", &redirect_uri, &scopes)
                    .await
                    .map_err(auth_error)?;
                (client.client_id, client.client_secret)
            }
        };
        let mut request = AuthorizationRequest::new(redirect_uri)
            .with_preregistered_client(client_id)
            .with_scopes(discovery.scopes.clone());
        if let Some(secret) = registered
            .map(|client| client.secret.as_str())
            .or(client_secret.as_deref())
        {
            request = request.with_client_secret(secret);
        }
        let session = AuthorizationSession::new(exchange.manager, request)
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
        let started = Started {
            request_id: Uuid::new_v4(),
            authorization_url: session.get_authorization_url().to_owned(),
            expires_in: AUTH_LIFETIME.as_secs(),
            provider: discovery.metadata.issuer.clone(),
            scopes: discovery.scopes.clone(),
        };
        self.close().await;
        let previous = match std::mem::replace(&mut self.state, State::Disconnected) {
            State::Authorized(credential) => Some(credential),
            _ => None,
        };
        self.state = State::Authorizing(Box::new(Pending {
            id: started.request_id,
            discovery,
            resource,
            client_secret,
            session,
            tokens: exchange.tokens,
            states: exchange.states,
            expires: Instant::now() + AUTH_LIFETIME,
            previous,
        }));
        Ok(started)
    }

    pub(super) async fn complete(&mut self, callback: Complete) -> Result<(), Error> {
        self.store.require_key()?;
        let State::Authorizing(pending) = &self.state else {
            return Err(Error::Invalid(
                "This MCP sign-in has expired or was already completed.",
            ));
        };
        if callback.request_id != pending.id
            || !matches!(pending.states.load(&callback.state).await, Ok(Some(_)))
        {
            return Err(Error::Invalid("The MCP sign-in callback state is invalid."));
        }
        if pending.expires <= Instant::now() {
            let id = pending.id;
            self.cancel(id);
            return Err(Error::Invalid("This MCP sign-in has expired."));
        }
        if !issuer_matches(&pending.discovery.metadata, callback.iss.as_deref()) {
            return Err(Error::Invalid(
                "The MCP sign-in callback issuer is invalid.",
            ));
        }
        let credential = match (callback.error, callback.code) {
            (Some(_), _) => Err(Error::Invalid("MCP sign-in was denied or cancelled.")),
            (None, None) => Err(Error::Invalid(
                "The MCP sign-in callback did not include a valid code.",
            )),
            (None, Some(code)) => match pending
                .session
                .handle_callback_with_issuer(&code, &callback.state, callback.iss.as_deref())
                .await
            {
                Err(error) => Err(auth_error(error)),
                Ok(_) => match pending.tokens.load().await {
                    Ok(Some(stored)) => Credential::from_stored(
                        pending.resource.clone(),
                        pending.discovery.metadata.clone(),
                        pending.client_secret.clone(),
                        stored,
                    )
                    .ok_or(Error::Connection),
                    _ => Err(Error::Connection),
                },
            },
        };
        self.end_authorizing();
        let credential = credential?;
        self.store.save(&credential).await?;
        self.state = State::Authorized(Box::new(credential));
        Ok(())
    }

    async fn refresh(&mut self, force: bool) -> Result<(), Error> {
        let State::Authorized(credential) = &self.state else {
            return Ok(());
        };
        if !force
            && credential
                .expires_at()
                .is_none_or(|expires| expires > now() + 30)
        {
            return Ok(());
        }
        match self.refreshed(credential).await {
            Ok(refreshed) => {
                self.store.save(&refreshed).await?;
                self.state = State::Authorized(Box::new(refreshed));
                self.close().await;
                Ok(())
            }
            Err(AuthError::AuthorizationRequired | AuthError::TokenRefreshRejected(_)) => {
                let discovery = Discovery {
                    resource: credential.resource.clone(),
                    challenge: None,
                    metadata: credential.metadata.clone(),
                    scopes: credential.scopes.clone(),
                };
                self.close().await;
                self.store.remove().await?;
                self.state = State::ReauthRequired(discovery);
                Err(Error::AuthRequired(Box::new(self.status())))
            }
            Err(error) => Err(auth_error(error)),
        }
    }

    async fn refreshed(&self, credential: &Credential) -> Result<Credential, AuthError> {
        let registered = self.client.as_deref().filter(|client| {
            client.applies(&credential.metadata) && client.client_id == credential.client_id
        });
        let mut exchange = exchange(&credential.resource, registered.is_some()).await?;
        exchange.manager.set_metadata(credential.metadata.clone());
        let mut config =
            OAuthClientConfig::new(credential.client_id.clone(), credential.resource.clone());
        if let Some(secret) = registered
            .map(|client| client.secret.as_str())
            .or(credential.client_secret.as_deref())
        {
            config = config.with_client_secret(secret);
        }
        exchange.manager.configure_client(config)?;
        exchange.tokens.save(credential.stored()).await?;
        exchange.manager.refresh_token().await?;
        exchange
            .tokens
            .load()
            .await?
            .and_then(|stored| {
                Credential::from_stored(
                    credential.resource.clone(),
                    credential.metadata.clone(),
                    credential.client_secret.clone(),
                    stored,
                )
            })
            .ok_or(AuthError::AuthorizationRequired)
    }

    fn registered_client(
        &self,
        metadata: &AuthorizationMetadata,
    ) -> Result<Option<&RegisteredClient>, Error> {
        if let Some(client) = self
            .client
            .as_deref()
            .filter(|client| client.applies(metadata))
        {
            return Ok(Some(client));
        }
        if metadata.issuer.as_deref() == Some("https://github.com/login/oauth") {
            return Err(Error::Invalid(
                "GitHub sign-in needs an OAuth app. Set ELSE_GITHUB_CLIENT_ID and ELSE_GITHUB_CLIENT_SECRET on the API server, then restart it and try again.",
            ));
        }
        Ok(None)
    }

    pub(super) async fn disconnect(&mut self) -> Result<(), Error> {
        self.close().await;
        self.store.remove().await?;
        self.state = State::Disconnected;
        Ok(())
    }

    pub(super) fn cancel(&mut self, request_id: Uuid) {
        if matches!(&self.state, State::Authorizing(pending) if pending.id == request_id) {
            self.end_authorizing();
        }
    }

    /// Leave a sign-in, restoring the account it would have replaced.
    fn end_authorizing(&mut self) {
        if let State::Authorizing(pending) = std::mem::replace(&mut self.state, State::Disconnected)
        {
            self.state = match pending.previous {
                Some(credential) => State::Authorized(credential),
                None => State::Required(pending.discovery),
            };
        }
    }

    pub(super) fn cancel_pending(&mut self) -> ConnectionStatus {
        if let State::Authorizing(pending) = &self.state {
            let id = pending.id;
            self.cancel(id);
        }
        self.status()
    }

    pub(super) async fn close(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            connection.shutdown().await;
        }
    }
}

/// RFC 9207: a callback must name the provider the sign-in started with,
/// and must name one when the provider says it always does.
fn issuer_matches(metadata: &AuthorizationMetadata, received: Option<&str>) -> bool {
    let required = metadata
        .additional_fields
        .get("authorization_response_iss_parameter_supported")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    match (metadata.issuer.as_deref(), received) {
        (Some(expected), Some(received)) => expected == received,
        (None, Some(_)) => false,
        (_, None) => !required,
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod fixtures;
