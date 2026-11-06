use zeroize::Zeroizing;

use super::{Error, protocol::Discovery};

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

    pub(super) fn applies(&self, discovery: &Discovery) -> bool {
        discovery.issuer == self.issuer
    }

    pub(super) fn validate(&self, discovery: &Discovery) -> Result<(), Error> {
        let resource = super::protocol::safe_url(&discovery.resource)?;
        if !self.applies(discovery)
            || resource.origin().ascii_serialization() != self.resource_origin
            || discovery.authorization_endpoint != self.authorization_endpoint
            || discovery.token_endpoint != self.token_endpoint
        {
            return Err(Error::Invalid(
                "The MCP OAuth metadata does not match the registered app's trusted endpoints.",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
