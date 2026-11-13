use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::Url;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::Zeroizing;

use super::Error;

const MAX_BODY: usize = 64 * 1024;

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Discovery {
    pub(super) resource: String,
    pub(super) issuer: String,
    pub(super) authorization_endpoint: String,
    pub(super) token_endpoint: String,
    pub(super) registration_endpoint: Option<String>,
    pub(super) scopes: Vec<String>,
    pub(super) require_iss: bool,
}

pub(super) async fn register(discovery: &Discovery, redirect_uri: &str) -> Result<String, Error> {
    let registration = discovery.registration_endpoint.as_deref().ok_or(Error::Invalid(
        "This provider needs a pre-registered OAuth client; dynamic registration is unavailable.",
    ))?;
    let response = client()?
        .post(registration)
        .json(&serde_json::json!({
            "client_name": "else.bot",
            "redirect_uris": [redirect_uri],
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
    Ok(registered.client_id)
}

pub(super) fn loopback(url: &Url) -> bool {
    matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
}

pub(super) fn safe_url(value: &str) -> Result<Url, Error> {
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

pub(super) fn authorization_endpoint(value: &str) -> Result<Url, Error> {
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
        .user_agent(crate::mcp::USER_AGENT)
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

pub(super) async fn discover(resource: &str, challenge: &str) -> Result<Discovery, Error> {
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

/// Providers such as PostHog advertise over 150 scopes, all requested when the
/// challenge names none.
const MAX_SCOPES: usize = 512;
const MAX_SCOPE_PARAMETER: usize = 8192;

pub(super) fn validate_scopes(scopes: &[String]) -> Result<(), Error> {
    if scopes.len() > MAX_SCOPES
        || scopes.join(" ").len() > MAX_SCOPE_PARAMETER
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
            quoted.toggle()
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
pub(super) struct TokenResponse {
    pub(super) access_token: String,
    pub(super) token_type: String,
    pub(super) refresh_token: Option<String>,
    pub(super) expires_in: Option<u64>,
}

pub(super) async fn token(
    endpoint: &str,
    fields: &[(&str, String)],
    client_secret: Option<&str>,
) -> Result<TokenResponse, Error> {
    let fields: Vec<_> = fields
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .chain(client_secret.map(|secret| ("client_secret", secret)))
        .collect();
    let response = client()?
        .post(endpoint)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&fields)
        .send()
        .await
        .map_err(|_| Error::Connection)?;
    if matches!(
        response.status(),
        reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::UNAUTHORIZED
    ) {
        return Err(Error::OAuthRevoked);
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum TokenResult {
        Error { error: String },
        Token(TokenResponse),
    }
    let token = match json::<TokenResult>(response).await? {
        TokenResult::Token(token) => token,
        TokenResult::Error { error } => {
            return Err(match error.as_str() {
                "invalid_grant" | "bad_refresh_token" | "bad_verification_code" => {
                    Error::OAuthRevoked
                }
                "invalid_client" | "incorrect_client_credentials" => Error::Invalid(
                    "The OAuth app client ID or secret is incorrect. Update the API server's OAuth app configuration.",
                ),
                _ => Error::Invalid("The OAuth provider rejected the token request."),
            });
        }
    };
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

#[cfg(test)]
mod tests;
