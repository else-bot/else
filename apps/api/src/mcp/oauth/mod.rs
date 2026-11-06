mod protocol;
mod store;

use std::path::Path;
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use super::{Error, Transport, transport};
use protocol::{
    Discovery, authorization_endpoint, discover, loopback, register, safe_url, token,
    validate_scopes,
};
use store::Credential;
pub(super) use store::Store;

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
    client_id: String,
    redirect_uri: String,
    state_hash: [u8; 32],
    verifier: Zeroizing<String>,
    expires: Instant,
    previous: Option<Credential>,
}

enum State {
    Disconnected,
    Required(Discovery),
    Authorizing(Pending),
    Authorized(Credential),
    ReauthRequired(Discovery),
}

pub(super) struct Session {
    store: Store,
    state: State,
    connection: Option<transport::Connection>,
}

impl Session {
    pub(super) async fn open(store: Store) -> Result<Self, Error> {
        let state = store
            .load()
            .await?
            .map_or(State::Disconnected, State::Authorized);
        Ok(Self {
            store,
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
        let (status, discovery) = match &self.state {
            State::Disconnected => (
                if self.connection.is_some() {
                    "connected"
                } else {
                    "disconnected"
                },
                None,
            ),
            State::Required(discovery) => ("disconnected", Some(discovery)),
            State::Authorizing(pending) => ("authorizing", Some(&pending.discovery)),
            State::Authorized(credential) => {
                let expired = credential.expires_at.is_some_and(|at| at <= now());
                (
                    if expired && credential.refresh_token.is_none() {
                        "reauth_required"
                    } else {
                        "connected"
                    },
                    Some(&credential.discovery),
                )
            }
            State::ReauthRequired(discovery) => ("reauth_required", Some(discovery)),
        };
        ConnectionStatus {
            status,
            provider: discovery.map(|d| d.issuer.clone()),
            scopes: discovery.map_or_else(Vec::new, |d| d.scopes.clone()),
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
                        State::Authorized(credential) => &credential.discovery.scopes,
                        State::ReauthRequired(discovery) => &discovery.scopes,
                        _ => &[],
                    };
                    for scope in previous_scopes {
                        if !discovery.scopes.contains(scope) {
                            discovery.scopes.push(scope.clone());
                        }
                    }
                    validate_scopes(&discovery.scopes)?;
                    self.store.remove().await?;
                    self.state = if was_authorized {
                        State::ReauthRequired(discovery)
                    } else {
                        State::Required(discovery)
                    };
                    return Err(Error::AuthRequired(Box::new(self.status())));
                }
                Err(error @ (Error::Connection | Error::Timeout)) => {
                    self.close().await;
                    return Err(error);
                }
                other => return other,
            }
        }
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
                    format!("Bearer {}", credential.access_token),
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
        let redirect = safe_url(&redirect_uri)?;
        if redirect.scheme() != "http" || !loopback(&redirect) || redirect.query().is_some() {
            return Err(Error::Invalid(
                "MCP sign-in needs a local loopback callback URL.",
            ));
        }
        self.status();
        if matches!(self.state, State::Authorizing(_)) {
            return Err(Error::Invalid("This MCP sign-in is already in progress."));
        }
        let discovery = match &self.state {
            State::Required(d) | State::ReauthRequired(d) => d.clone(),
            State::Authorized(c) => c.discovery.clone(),
            State::Authorizing(p) => p.discovery.clone(),
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
        let client_id = register(&discovery, &redirect_uri).await?;
        let id = Uuid::new_v4();
        let state = nonce();
        let verifier = Zeroizing::new(nonce());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = authorization_endpoint(&discovery.authorization_endpoint)?;
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("resource", discovery.resource.as_str()),
        ]);
        if !discovery.scopes.is_empty() {
            url.query_pairs_mut()
                .append_pair("scope", &discovery.scopes.join(" "));
        }
        self.close().await;
        let previous = match std::mem::replace(&mut self.state, State::Disconnected) {
            State::Authorized(credential) => Some(credential),
            _ => None,
        };
        let provider = Some(discovery.issuer.clone());
        let scopes = discovery.scopes.clone();
        self.state = State::Authorizing(Pending {
            id,
            discovery,
            client_id,
            redirect_uri,
            state_hash: Sha256::digest(state.as_bytes()).into(),
            verifier,
            expires: Instant::now() + AUTH_LIFETIME,
            previous,
        });
        Ok(Started {
            request_id: id,
            authorization_url: url.into(),
            expires_in: AUTH_LIFETIME.as_secs(),
            provider,
            scopes,
        })
    }

    pub(super) async fn complete(&mut self, callback: Complete) -> Result<(), Error> {
        self.store.require_key()?;
        let State::Authorizing(pending) = &self.state else {
            return Err(Error::Invalid(
                "This MCP sign-in has expired or was already completed.",
            ));
        };
        let state_hash: [u8; 32] = Sha256::digest(callback.state.as_bytes()).into();
        if callback.request_id != pending.id
            || callback.state.len() > 128
            || !bool::from(state_hash.ct_eq(&pending.state_hash))
        {
            return Err(Error::Invalid("The MCP sign-in callback state is invalid."));
        }
        if pending.expires <= Instant::now() {
            let id = pending.id;
            self.cancel(id);
            return Err(Error::Invalid("This MCP sign-in has expired."));
        }
        if callback
            .iss
            .as_ref()
            .is_some_and(|issuer| issuer != &pending.discovery.issuer)
            || (pending.discovery.require_iss && callback.iss.is_none())
        {
            return Err(Error::Invalid(
                "The MCP sign-in callback issuer is invalid.",
            ));
        }
        let discovery = pending.discovery.clone();
        let previous = std::mem::replace(&mut self.state, State::Required(discovery));
        let State::Authorizing(mut pending) = previous else {
            unreachable!()
        };
        if let Some(previous) = pending.previous.take() {
            self.state = State::Authorized(previous);
        }
        if callback.error.is_some() {
            return Err(Error::Invalid("MCP sign-in was denied or cancelled."));
        }
        let code = callback
            .code
            .filter(|code| !code.is_empty() && code.len() <= 8192)
            .ok_or(Error::Invalid(
                "The MCP sign-in callback did not include a valid code.",
            ))?;
        let mut fields = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", code),
            ("client_id", pending.client_id.clone()),
            ("redirect_uri", pending.redirect_uri.clone()),
            ("code_verifier", pending.verifier.to_string()),
            ("resource", pending.discovery.resource.clone()),
        ];
        let result = token(&pending.discovery.token_endpoint, &fields).await;
        for (_, value) in &mut fields {
            value.zeroize();
        }
        let response = result?;
        let credential = Credential {
            discovery: pending.discovery,
            client_id: pending.client_id,
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            expires_at: response.expires_in.map(expiration),
        };
        self.store.save(&credential).await?;
        self.state = State::Authorized(credential);
        Ok(())
    }

    async fn refresh(&mut self, force: bool) -> Result<(), Error> {
        let State::Authorized(credential) = &self.state else {
            return Ok(());
        };
        if !force
            && credential
                .expires_at
                .is_none_or(|expires| expires > now() + 30)
        {
            return Ok(());
        }
        let Some(refresh_token) = &credential.refresh_token else {
            let discovery = credential.discovery.clone();
            self.close().await;
            self.store.remove().await?;
            self.state = State::ReauthRequired(discovery);
            return Err(Error::AuthRequired(Box::new(self.status())));
        };
        let mut fields = vec![
            ("grant_type", "refresh_token".to_owned()),
            ("refresh_token", refresh_token.clone()),
            ("client_id", credential.client_id.clone()),
            ("resource", credential.discovery.resource.clone()),
        ];
        let result = token(&credential.discovery.token_endpoint, &fields).await;
        for (_, value) in &mut fields {
            value.zeroize();
        }
        let response = match result {
            Ok(response) => response,
            Err(Error::OAuthRevoked) => {
                let discovery = credential.discovery.clone();
                self.close().await;
                self.store.remove().await?;
                self.state = State::ReauthRequired(discovery);
                return Err(Error::AuthRequired(Box::new(self.status())));
            }
            Err(error) => return Err(error),
        };
        let refreshed = Credential {
            discovery: credential.discovery.clone(),
            client_id: credential.client_id.clone(),
            access_token: response.access_token,
            refresh_token: response
                .refresh_token
                .or_else(|| credential.refresh_token.clone()),
            expires_at: response.expires_in.map(expiration),
        };
        self.store.save(&refreshed).await?;
        self.state = State::Authorized(refreshed);
        self.close().await;
        Ok(())
    }

    pub(super) async fn disconnect(&mut self) -> Result<(), Error> {
        self.close().await;
        self.store.remove().await?;
        self.state = State::Disconnected;
        Ok(())
    }

    pub(super) fn cancel(&mut self, request_id: Uuid) {
        if let State::Authorizing(pending) = &self.state
            && pending.id == request_id
        {
            let discovery = pending.discovery.clone();
            let State::Authorizing(pending) =
                std::mem::replace(&mut self.state, State::Required(discovery))
            else {
                unreachable!()
            };
            if let Some(credential) = pending.previous {
                self.state = State::Authorized(credential);
            }
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
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn expiration(seconds: u64) -> i64 {
    now().saturating_add(seconds.min(365 * 86_400) as i64)
}

fn nonce() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS entropy");
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod fixtures;
