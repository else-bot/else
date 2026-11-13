use rmcp::transport::auth::AuthorizationMetadata;
use zeroize::Zeroizing;

use super::Error;

/// An OAuth app registered with a provider that offers no dynamic client
/// registration. Its secret only ever reaches the app's own token endpoint.
pub struct RegisteredClient {
    resource_origin: String,
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    pub(super) client_id: String,
    pub(super) secret: Zeroizing<String>,
}

impl std::fmt::Debug for RegisteredClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RegisteredClient(<redacted>)")
    }
}

impl RegisteredClient {
    pub fn github(client_id: String, secret: String) -> Result<Self, &'static str> {
        let secret = Zeroizing::new(secret);
        if [client_id.as_str(), secret.as_str()].iter().any(|value| {
            value.is_empty()
                || value.len() > 2048
                || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        }) {
            return Err(
                "use nonempty OAuth app credentials with at most 2048 printable ASCII characters",
            );
        }
        Ok(Self {
            resource_origin: "https://api.githubcopilot.com".into(),
            issuer: "https://github.com/login/oauth".into(),
            authorization_endpoint: "https://github.com/login/oauth/authorize".into(),
            token_endpoint: "https://github.com/login/oauth/access_token".into(),
            client_id,
            secret,
        })
    }

    pub(super) fn applies(&self, metadata: &AuthorizationMetadata) -> bool {
        metadata.issuer.as_deref() == Some(self.issuer.as_str())
    }

    /// The app's own endpoints, used in place of what the MCP advertised so a
    /// server cannot redirect the secret or the sign-in elsewhere. The app's
    /// tokens are only for the MCP it was registered for.
    pub(super) fn metadata(&self, resource: &str) -> Result<AuthorizationMetadata, Error> {
        let origin = reqwest::Url::parse(resource)
            .map(|url| url.origin().ascii_serialization())
            .map_err(|_| Error::Invalid("Invalid MCP URL."))?;
        if origin != self.resource_origin {
            return Err(Error::Invalid(
                "This MCP names the registered OAuth app's provider but is not the app's MCP.",
            ));
        }
        Ok(serde_json::from_value(serde_json::json!({
            "issuer": self.issuer,
            "authorization_endpoint": self.authorization_endpoint,
            "token_endpoint": self.token_endpoint,
            "code_challenge_methods_supported": ["S256"],
            "token_endpoint_auth_methods_supported": ["client_secret_post"],
            "authorization_response_iss_parameter_supported": true,
        }))
        .expect("registered OAuth metadata"))
    }
}

#[cfg(test)]
mod tests;
