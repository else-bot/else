use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Url;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use super::{Error, Transport, transport};
use crate::credentials::SecretKey;

const MAX_BODY: usize = 64 * 1024;
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

#[derive(Clone, Serialize, Deserialize)]
struct Discovery {
    resource: String,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes: Vec<String>,
    require_iss: bool,
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

#[derive(Serialize, Deserialize)]
struct Credential {
    discovery: Discovery,
    client_id: String,
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<i64>,
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

#[derive(Default)]
enum State {
    #[default]
    Disconnected,
    Required(Discovery),
    Authorizing(Pending),
    Authorized(Credential),
    ReauthRequired(Discovery),
}

#[derive(Default)]
pub(super) struct Session {
    loaded: bool,
    state: State,
    connection: Option<transport::Connection>,
}

impl Session {
    pub(super) async fn load(
        &mut self,
        path: &Path,
        bot: Uuid,
        key: Option<&SecretKey>,
    ) -> Result<(), Error> {
        if !self.loaded {
            let stored = load(path, bot, key)?;
            self.state = stored.map_or(State::Disconnected, State::Authorized);
            self.loaded = true;
        }
        Ok(())
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
        path: &Path,
        bot: Uuid,
        key: Option<&SecretKey>,
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
        self.refresh(path, bot, key, false).await?;
        let mut retried = false;
        loop {
            let result = match self.ensure_connection(config, path).await {
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
                        match self.refresh(path, bot, key, true).await {
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
                    let previous_scopes = match &self.state {
                        State::Authorized(credential) => &credential.discovery.scopes,
                        State::ReauthRequired(discovery) => &discovery.scopes,
                        _ => &Vec::new(),
                    };
                    for scope in previous_scopes {
                        if !discovery.scopes.contains(scope) {
                            discovery.scopes.push(scope.clone());
                        }
                    }
                    validate_scopes(&discovery.scopes)?;
                    remove(path, bot)?;
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

    async fn ensure_connection(&mut self, config: &Transport, path: &Path) -> Result<(), Error> {
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
                    path,
                    path.parent()
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
        path: &Path,
        redirect_uri: String,
        key: Option<&SecretKey>,
    ) -> Result<Started, Error> {
        if key.is_none() {
            return Err(Error::Invalid(
                "MCP sign-in needs the installation's SECRET_KEY.",
            ));
        }
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
        let registration = discovery.registration_endpoint.as_deref().ok_or(Error::Invalid(
            "This provider needs a pre-registered OAuth client; dynamic registration is unavailable.",
        ))?;
        let response = client()?
            .post(registration)
            .json(&serde_json::json!({
                "client_name": "else.bot",
                "redirect_uris": [&redirect_uri],
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none"
            }))
            .send()
            .await
            .map_err(|_| Error::Connection)?;
        #[derive(Deserialize)]
        struct Registration {
            client_id: String,
            token_endpoint_auth_method: Option<String>,
        }
        let registered: Registration = json(response).await?;
        if registered.client_id.is_empty()
            || registered.client_id.len() > 2048
            || registered
                .token_endpoint_auth_method
                .as_deref()
                .is_some_and(|method| method != "none")
        {
            return Err(Error::Invalid(
                "The provider did not register a public OAuth client.",
            ));
        }
        let id = Uuid::new_v4();
        let state = nonce();
        let verifier = Zeroizing::new(nonce());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = authorization_endpoint(&discovery.authorization_endpoint)?;
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", registered.client_id.as_str()),
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
            client_id: registered.client_id,
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

    pub(super) async fn complete(
        &mut self,
        path: &Path,
        bot: Uuid,
        key: Option<&SecretKey>,
        callback: Complete,
    ) -> Result<(), Error> {
        let key = key.ok_or(Error::Invalid(
            "MCP sign-in needs the installation's SECRET_KEY.",
        ))?;
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
        let State::Authorizing(pending) = previous else {
            unreachable!()
        };
        if callback.error.is_some() {
            self.state = pending
                .previous
                .map_or(State::Required(pending.discovery), State::Authorized);
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
        save(path, bot, key, &credential)?;
        self.state = State::Authorized(credential);
        Ok(())
    }

    async fn refresh(
        &mut self,
        path: &Path,
        bot: Uuid,
        key: Option<&SecretKey>,
        force: bool,
    ) -> Result<(), Error> {
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
            remove(path, bot)?;
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
                remove(path, bot)?;
                self.state = State::ReauthRequired(discovery);
                return Err(Error::AuthRequired(Box::new(self.status())));
            }
            Err(error) => return Err(error),
        };
        let State::Authorized(credential) = &mut self.state else {
            unreachable!()
        };
        credential.access_token.zeroize();
        credential.access_token = response.access_token;
        if let Some(refresh_token) = response.refresh_token {
            credential.refresh_token.zeroize();
            credential.refresh_token = Some(refresh_token);
        }
        credential.expires_at = response.expires_in.map(expiration);
        save(
            path,
            bot,
            key.ok_or(Error::Invalid("MCP credentials need SECRET_KEY."))?,
            credential,
        )?;
        self.close().await;
        Ok(())
    }

    pub(super) async fn disconnect(&mut self, path: &Path, bot: Uuid) -> Result<(), Error> {
        self.close().await;
        remove(path, bot)?;
        self.state = State::Disconnected;
        self.loaded = true;
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

fn loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
}

fn safe_url(value: &str) -> Result<Url, Error> {
    let url = Url::parse(value)
        .map_err(|_| Error::Invalid("The OAuth provider returned an invalid URL."))?;
    if value.len() > 8192
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !(url.scheme() == "https" || (url.scheme() == "http" && loopback(&url)))
    {
        return Err(Error::Invalid(
            "OAuth endpoints must use HTTPS, except literal loopback HTTP for local servers.",
        ));
    }
    Ok(url)
}

fn endpoint(value: &str, resource: &Url) -> Result<(), Error> {
    let url = safe_url(value)?;
    if resource.scheme() == "https" && url.scheme() != "https" {
        return Err(Error::Invalid(
            "An HTTPS MCP cannot send OAuth requests to HTTP endpoints.",
        ));
    }
    Ok(())
}

fn authorization_endpoint(value: &str) -> Result<Url, Error> {
    let url = safe_url(value)?;
    const RESERVED: &[&str] = &[
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
    ];
    if url.query_pairs().any(|(name, _)| {
        RESERVED
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
    }) {
        return Err(Error::Invalid(
            "The OAuth authorization endpoint contains reserved request parameters.",
        ));
    }
    Ok(url)
}

fn client() -> Result<reqwest::Client, Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| Error::Connection)
}

async fn json<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T, Error> {
    if !response.status().is_success() {
        return Err(Error::Connection);
    }
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BODY as u64)
    {
        return Err(Error::Invalid(
            "The OAuth provider response exceeds 64 KiB.",
        ));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Connection)? {
        if bytes.len() + chunk.len() > MAX_BODY {
            return Err(Error::Invalid(
                "The OAuth provider response exceeds 64 KiB.",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::Invalid("The OAuth provider returned an invalid response."))
}

#[derive(Deserialize)]
struct ResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Deserialize)]
struct ServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
}

async fn discover(resource: &str, challenge: &str) -> Result<Discovery, Error> {
    let resource_url = safe_url(resource)?;
    let http = client()?;
    let parameters = challenge_parameters(challenge)?;
    let explicit = parameters.get("resource_metadata");
    let mut candidates = Vec::new();
    if let Some(metadata) = explicit {
        endpoint(metadata, &resource_url)?;
        candidates.push(metadata.clone());
    } else {
        let mut url = resource_url.clone();
        url.set_query(None);
        url.set_path(&format!(
            "/.well-known/oauth-protected-resource{}",
            resource_url.path().trim_end_matches('/')
        ));
        candidates.push(url.to_string());
        url.set_path("/.well-known/oauth-protected-resource");
        if !candidates.contains(&url.to_string()) {
            candidates.push(url.to_string());
        }
    }
    let mut protected = None;
    for candidate in candidates {
        let response = http
            .get(&candidate)
            .send()
            .await
            .map_err(|_| Error::Connection)?;
        if matches!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
        ) && explicit.is_none()
        {
            continue;
        }
        let metadata: ResourceMetadata = json(response).await?;
        if safe_url(&metadata.resource)?.as_str().trim_end_matches('/')
            != resource_url.as_str().trim_end_matches('/')
            || metadata.authorization_servers.is_empty()
            || metadata.authorization_servers.len() > 16
        {
            return Err(Error::Invalid(
                "The OAuth resource metadata does not identify this MCP.",
            ));
        }
        protected = Some(metadata);
        break;
    }
    let (issuer, scopes) = if let Some(metadata) = protected {
        (
            metadata.authorization_servers[0].clone(),
            metadata.scopes_supported,
        )
    } else {
        (resource_url.origin().ascii_serialization(), Vec::new())
    };
    endpoint(&issuer, &resource_url)?;
    let issuer_url = safe_url(&issuer)?;
    if issuer_url.query().is_some() {
        return Err(Error::Invalid("Invalid OAuth issuer URL."));
    }
    let suffix = issuer_url.path().trim_end_matches('/');
    let mut oauth_url = issuer_url.clone();
    oauth_url.set_path(&format!("/.well-known/oauth-authorization-server{suffix}"));
    let mut oidc_url = issuer_url.clone();
    oidc_url.set_path(&format!("/.well-known/openid-configuration{suffix}"));
    let mut oidc_append_url = issuer_url.clone();
    oidc_append_url.set_path(&format!("{suffix}/.well-known/openid-configuration"));
    let mut metadata = None;
    for url in [oauth_url, oidc_url, oidc_append_url] {
        let response = http.get(url).send().await.map_err(|_| Error::Connection)?;
        if matches!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
        ) {
            continue;
        }
        metadata = Some(json::<ServerMetadata>(response).await?);
        break;
    }
    let metadata = metadata.ok_or(Error::Invalid(
        "The MCP did not publish OAuth authorization server metadata.",
    ))?;
    if metadata.issuer != issuer
        || !metadata
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
    {
        return Err(Error::Invalid(
            "The OAuth provider issuer or PKCE S256 support is invalid.",
        ));
    }
    endpoint(&metadata.authorization_endpoint, &resource_url)?;
    authorization_endpoint(&metadata.authorization_endpoint)?;
    endpoint(&metadata.token_endpoint, &resource_url)?;
    if let Some(registration) = &metadata.registration_endpoint {
        endpoint(registration, &resource_url)?
    }
    let scopes = parameters.get("scope").map_or(scopes, |scope| {
        scope.split_ascii_whitespace().map(str::to_owned).collect()
    });
    validate_scopes(&scopes)?;
    Ok(Discovery {
        resource: resource_url.into(),
        issuer: metadata.issuer,
        authorization_endpoint: metadata.authorization_endpoint,
        token_endpoint: metadata.token_endpoint,
        registration_endpoint: metadata.registration_endpoint,
        scopes,
        require_iss: metadata.authorization_response_iss_parameter_supported,
    })
}

fn validate_scopes(scopes: &[String]) -> Result<(), Error> {
    if scopes.len() > 64
        || scopes.join(" ").len() > 2048
        || scopes.iter().any(|scope| {
            scope.is_empty()
                || scope.len() > 256
                || !scope
                    .bytes()
                    .all(|byte| matches!(byte, 0x21 | 0x23..=0x5b | 0x5d..=0x7e))
        })
    {
        return Err(Error::Invalid(
            "The OAuth provider returned invalid scopes.",
        ));
    }
    Ok(())
}

fn challenge_parameters(value: &str) -> Result<BTreeMap<String, String>, Error> {
    if value.len() > 8192 {
        return Err(Error::Invalid(
            "The MCP authentication challenge is too large.",
        ));
    }
    let mut parameters = BTreeMap::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, byte) in value.bytes().chain(std::iter::once(b',')).enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' && quoted {
            escaped = true;
            continue;
        }
        if byte == b'"' {
            quoted = !quoted
        }
        if byte == b',' && !quoted {
            let part = value[start..index].trim();
            let part = part
                .split_once(' ')
                .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                .map_or(part, |(_, parameters)| parameters.trim_start());
            if let Some((name, raw)) = part.split_once('=') {
                let raw = raw.trim();
                let decoded = if let Some(raw) = raw.strip_prefix('"') {
                    raw.strip_suffix('"')
                        .ok_or(Error::Invalid("Invalid MCP authentication challenge."))?
                        .replace("\\\"", "\"")
                        .replace("\\\\", "\\")
                } else {
                    raw.to_owned()
                };
                if parameters
                    .insert(name.trim().to_ascii_lowercase(), decoded)
                    .is_some()
                {
                    return Err(Error::Invalid(
                        "Duplicate MCP authentication challenge parameter.",
                    ));
                }
            }
            start = index + 1;
        }
    }
    if quoted || escaped {
        return Err(Error::Invalid("Invalid MCP authentication challenge."));
    }
    Ok(parameters)
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

async fn token(endpoint: &str, fields: &[(&str, String)]) -> Result<TokenResponse, Error> {
    let response = client()?
        .post(endpoint)
        .form(fields)
        .send()
        .await
        .map_err(|_| Error::Connection)?;
    if matches!(
        response.status(),
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNAUTHORIZED
    ) {
        return Err(Error::OAuthRevoked);
    }
    let token: TokenResponse = json(response).await?;
    if !token.token_type.eq_ignore_ascii_case("bearer")
        || token.access_token.is_empty()
        || token.access_token.len() > 16_384
        || token
            .access_token
            .bytes()
            .any(|byte| byte <= 0x20 || byte >= 0x7f)
        || token
            .refresh_token
            .as_ref()
            .is_some_and(|token| token.is_empty() || token.len() > 16_384)
    {
        return Err(Error::Invalid(
            "The OAuth provider returned an invalid bearer token.",
        ));
    }
    Ok(token)
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    salt: String,
    ciphertext: String,
}

fn identity(
    key: &SecretKey,
    salt: &[u8; 32],
    path: &Path,
    bot: Uuid,
) -> Result<age::x25519::Identity, Error> {
    let package = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(Error::Connection)?;
    crate::backup::identity_for_purpose(
        key,
        salt,
        &format!("else.bot/mcp-oauth/v1/{package}/{bot}"),
    )
    .map_err(|_| Error::Invalid("The MCP credentials could not be encrypted or decrypted."))
}

fn filename(bot: Uuid) -> String {
    format!("oauth-{bot}.json")
}

fn save(path: &Path, bot: Uuid, key: &SecretKey, credential: &Credential) -> Result<(), Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut salt = [0u8; 32];
    getrandom::fill(&mut salt).expect("OS entropy");
    let identity = identity(key, &salt, path, bot)?;
    let recipient = identity.to_public();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|_| Error::Connection)?;
    let plain = Zeroizing::new(serde_json::to_vec(credential).map_err(|_| Error::Connection)?);
    let mut ciphertext = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut ciphertext)
        .map_err(|_| Error::Connection)?;
    writer.write_all(&plain)?;
    writer.finish()?;
    let bytes = serde_json::to_vec(&Envelope {
        salt: URL_SAFE_NO_PAD.encode(salt),
        ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
    })
    .map_err(|_| Error::Connection)?;
    if bytes.len() > MAX_BODY {
        return Err(Error::Invalid(
            "The encrypted MCP credentials exceed 64 KiB.",
        ));
    }
    super::require_dir(path)?;
    let staging = path.join(format!(".oauth-{}", Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&staging, path.join(filename(bot)))?;
        std::fs::File::open(path)?.sync_all()?;
        Ok::<_, Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(staging);
    }
    result
}

fn load(path: &Path, bot: Uuid, key: Option<&SecretKey>) -> Result<Option<Credential>, Error> {
    let text = match super::read_file(&path.join(filename(bot))) {
        Ok(text) => text,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let key = key.ok_or(Error::Invalid(
        "MCP credentials need the installation's SECRET_KEY.",
    ))?;
    let envelope: Envelope = serde_json::from_str(&text)
        .map_err(|_| Error::Invalid("Invalid encrypted MCP credentials."))?;
    let salt: [u8; 32] = URL_SAFE_NO_PAD
        .decode(envelope.salt)
        .map_err(|_| Error::Connection)?
        .try_into()
        .map_err(|_| Error::Connection)?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(envelope.ciphertext)
        .map_err(|_| Error::Connection)?;
    let identity = identity(key, &salt, path, bot)?;
    let decryptor = age::Decryptor::new(ciphertext.as_slice()).map_err(|_| Error::Connection)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|_| Error::Invalid("The MCP credentials could not be decrypted."))?;
    let mut plain = Zeroizing::new(Vec::new());
    reader.read_to_end(&mut plain)?;
    let credential = serde_json::from_slice(&plain)
        .map_err(|_| Error::Invalid("Invalid decrypted MCP credentials."))?;
    Ok(Some(credential))
}

fn remove(path: &Path, bot: Uuid) -> Result<(), Error> {
    match std::fs::remove_file(path.join(filename(bot))) {
        Ok(()) => {
            std::fs::File::open(path)?.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn key() -> SecretKey {
        SecretKey::parse("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").unwrap()
    }

    fn discovery() -> Discovery {
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

    fn credential() -> Credential {
        Credential {
            discovery: discovery(),
            client_id: "public-client".into(),
            access_token: "private-access-token".into(),
            refresh_token: Some("private-refresh-token".into()),
            expires_at: Some(now() + 3600),
        }
    }

    fn pending(previous: Option<Credential>) -> Pending {
        Pending {
            id: Uuid::new_v4(),
            discovery: discovery(),
            client_id: "public-client".into(),
            redirect_uri: "http://127.0.0.1:12345/callback".into(),
            state_hash: Sha256::digest(b"expected-state").into(),
            verifier: Zeroizing::new("private-pkce-verifier".into()),
            expires: Instant::now() + AUTH_LIFETIME,
            previous,
        }
    }

    fn callback(id: Uuid, state: &str) -> Complete {
        Complete {
            request_id: id,
            state: state.into(),
            code: None,
            error: Some("access_denied".into()),
            iss: Some("https://auth.example.com".into()),
        }
    }

    #[test]
    fn credentials_are_encrypted_private_and_bound_to_bot_and_package() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("first-package");
        let other = dir.path().join("other-package");
        std::fs::create_dir(&path).unwrap();
        std::fs::create_dir(&other).unwrap();
        let bot = Uuid::new_v4();
        let key = key();
        save(&path, bot, &key, &credential()).unwrap();
        let stored = path.join(filename(bot));
        let bytes = std::fs::read(&stored).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("private-access-token"));
        assert!(!String::from_utf8_lossy(&bytes).contains("private-refresh-token"));
        assert_eq!(
            std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let opened = load(&path, bot, Some(&key)).unwrap().unwrap();
        assert_eq!(opened.access_token, "private-access-token");
        assert_eq!(
            opened.refresh_token.as_deref(),
            Some("private-refresh-token")
        );
        let another_bot = Uuid::new_v4();
        std::fs::copy(&stored, path.join(filename(another_bot))).unwrap();
        assert!(load(&path, another_bot, Some(&key)).is_err());
        std::fs::copy(&stored, other.join(filename(bot))).unwrap();
        assert!(load(&other, bot, Some(&key)).is_err());
        remove(&path, bot).unwrap();
        assert!(load(&path, bot, Some(&key)).unwrap().is_none());
    }

    #[tokio::test]
    async fn wrong_state_and_issuer_do_not_consume_an_active_flow_and_denial_is_one_shot() {
        let dir = tempfile::tempdir().unwrap();
        let pending = pending(None);
        let id = pending.id;
        let bot = Uuid::new_v4();
        let key = key();
        let mut session = Session {
            state: State::Authorizing(pending),
            ..Session::default()
        };
        assert!(
            session
                .complete(dir.path(), bot, Some(&key), callback(id, "wrong-state"))
                .await
                .is_err()
        );
        assert_eq!(session.status().status, "authorizing");
        let mut wrong_issuer = callback(id, "expected-state");
        wrong_issuer.iss = Some("https://imposter.example.com".into());
        assert!(
            session
                .complete(dir.path(), bot, Some(&key), wrong_issuer)
                .await
                .is_err()
        );
        assert_eq!(session.status().status, "authorizing");
        assert!(
            session
                .complete(dir.path(), bot, Some(&key), callback(id, "expected-state"))
                .await
                .is_err()
        );
        assert_eq!(session.status().status, "disconnected");
        assert!(
            session
                .complete(dir.path(), bot, Some(&key), callback(id, "expected-state"))
                .await
                .is_err()
        );
    }

    #[test]
    fn cancellation_and_expiration_restore_an_existing_account_without_exposing_credentials() {
        let flow = pending(Some(credential()));
        let id = flow.id;
        let mut session = Session {
            state: State::Authorizing(flow),
            ..Session::default()
        };
        session.cancel(Uuid::new_v4());
        assert_eq!(session.status().status, "authorizing");
        session.cancel(id);
        assert_eq!(session.status().status, "connected");
        let safe = serde_json::to_string(&session.status()).unwrap();
        assert!(!safe.contains("private-access-token"));
        assert!(!safe.contains("private-refresh-token"));
        let mut pending = pending(None);
        pending.expires = Instant::now() - Duration::from_secs(1);
        session.state = State::Authorizing(pending);
        assert_eq!(session.status().status, "disconnected");
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
        assert!(
            authorization_endpoint("https://oauth.example.com/authorize?%73cope=admin").is_err()
        );
        assert!(
            authorization_endpoint("https://oauth.example.com/authorize?tenant=example").is_ok()
        );
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
}
