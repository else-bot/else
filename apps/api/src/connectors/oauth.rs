use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use super::types::{Credential, MailError, Provider};
use crate::actor::Actor;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Start {
    pub provider: Provider,
    #[serde(default)]
    pub address: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub redirect_uri: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Complete {
    pub request_id: Uuid,
    pub state: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct Started {
    pub request_id: Uuid,
    pub authorization_url: String,
    pub expires_in: u64,
}

struct Pending {
    request: Start,
    state_hash: [u8; 32],
    verifier: Zeroizing<String>,
    expires: Instant,
}
impl Drop for Start {
    fn drop(&mut self) {
        self.client_secret.zeroize();
    }
}

#[derive(Default)]
pub struct OAuth {
    pending: Actor<HashMap<Uuid, Pending>>,
}

fn nonce() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS entropy");
    URL_SAFE_NO_PAD.encode(bytes)
}

impl OAuth {
    pub async fn cancel(&self, request_id: Uuid) {
        self.pending
            .call(move |map| {
                map.remove(&request_id);
            })
            .await;
    }

    pub async fn start(&self, request: Start) -> Result<Started, MailError> {
        if !matches!(request.provider, Provider::Gmail | Provider::Microsoft) {
            return Err(MailError::new("oauth_wizard_unavailable"));
        }
        if request.provider != Provider::Gmail || !request.address.is_empty() {
            request
                .provider
                .config(request.address.clone())?
                .validate()?;
        }
        let redirect = reqwest::Url::parse(&request.redirect_uri)
            .map_err(|_| MailError::new("invalid_redirect"))?;
        let loopback = matches!(
            redirect.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        );
        if !(redirect.scheme() == "https" || (redirect.scheme() == "http" && loopback))
            || redirect.fragment().is_some()
            || !redirect.username().is_empty()
            || redirect.password().is_some()
            || request.client_id.is_empty()
            || request.client_id.len() > 1024
            || request
                .client_secret
                .as_ref()
                .is_some_and(|s| s.len() > 4096)
        {
            return Err(MailError::new("invalid_oauth_client"));
        }
        let (authorize, _, scopes) = request.provider.oauth_endpoints()?;
        let request_id = Uuid::new_v4();
        let state = nonce();
        let verifier = Zeroizing::new(nonce());
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut url = reqwest::Url::parse(authorize).expect("fixed provider URL");
        url.query_pairs_mut().extend_pairs([
            ("client_id", request.client_id.as_str()),
            ("redirect_uri", request.redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", scopes),
            ("state", state.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("access_type", "offline"),
            ("prompt", "consent"),
        ]);
        if !request.address.is_empty() {
            url.query_pairs_mut()
                .append_pair("login_hint", &request.address);
        }
        let pending = Pending {
            request,
            state_hash: Sha256::digest(state.as_bytes()).into(),
            verifier,
            expires: Instant::now() + Duration::from_secs(600),
        };
        self.pending
            .call(move |map| {
                map.retain(|_, p| p.expires > Instant::now());
                if map.len() >= 32 {
                    return Err(MailError::new("oauth_setup_limit"));
                }
                map.insert(request_id, pending);
                Ok(())
            })
            .await?;
        Ok(Started {
            request_id,
            authorization_url: url.into(),
            expires_in: 600,
        })
    }

    pub async fn complete(
        &self,
        request: Complete,
    ) -> Result<(Provider, String, Credential), MailError> {
        if request.code.is_empty() || request.code.len() > 8192 || request.state.len() > 128 {
            return Err(MailError::new("invalid_oauth_callback"));
        }
        let pending = self
            .pending
            .call(move |map| map.remove(&request.request_id))
            .await
            .ok_or_else(|| MailError::new("oauth_setup_expired"))?;
        use subtle::ConstantTimeEq;
        let hash: [u8; 32] = Sha256::digest(request.state.as_bytes()).into();
        if pending.expires <= Instant::now() || !bool::from(hash.ct_eq(&pending.state_hash)) {
            return Err(MailError::new("invalid_oauth_state"));
        }
        let mut fields = vec![
            ("grant_type", "authorization_code".to_owned()),
            ("code", request.code),
            ("redirect_uri", pending.request.redirect_uri.clone()),
            ("client_id", pending.request.client_id.clone()),
            ("code_verifier", pending.verifier.to_string()),
        ];
        if let Some(secret) = &pending.request.client_secret {
            fields.push(("client_secret", secret.clone()));
        }
        let result = token(pending.request.provider, &fields).await;
        for (_, value) in &mut fields {
            value.zeroize();
        }
        let token = result?;
        if pending.request.provider == Provider::Gmail
            && token.scope.as_deref().is_some_and(|scope| {
                !scope
                    .split_whitespace()
                    .any(|s| s == "https://mail.google.com/")
            })
        {
            return Err(MailError::new("oauth_mail_access_required"));
        }
        let address = if pending.request.provider == Provider::Gmail {
            gmail_address(
                "https://gmail.googleapis.com/gmail/v1/users/me/profile",
                &token.access_token,
            )
            .await?
        } else {
            pending.request.address.clone()
        };
        let refresh_token = token
            .refresh_token
            .ok_or_else(|| MailError::new("oauth_refresh_token_required"))?;
        let credential = Credential::OAuth2 {
            client_id: pending.request.client_id.clone(),
            client_secret: pending.request.client_secret.clone(),
            refresh_token,
            access_token: Some(token.access_token),
            expires_at: Some(chrono::Utc::now().timestamp() + token.expires_in.clamp(60, 86_400)),
        };
        Ok((pending.request.provider, address, credential))
    }
}

async fn gmail_address(endpoint: &str, access_token: &str) -> Result<String, MailError> {
    #[derive(Deserialize)]
    struct Profile {
        #[serde(rename = "emailAddress")]
        address: String,
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| MailError::new("oauth_unavailable"))?;
    let mut response = client
        .get(endpoint)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| MailError::new("oauth_unavailable"))?;
    if !response.status().is_success() {
        return Err(MailError::new("oauth_reauthentication_required"));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| MailError::new("oauth_unavailable"))?
    {
        if bytes.len() + chunk.len() > 16_384 {
            return Err(MailError::new("oauth_response_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let profile: Profile =
        serde_json::from_slice(&bytes).map_err(|_| MailError::new("invalid_oauth_response"))?;
    Provider::Gmail
        .config(profile.address.clone())?
        .validate()?;
    Ok(profile.address)
}

#[derive(Deserialize)]
struct Token {
    access_token: String,
    refresh_token: Option<String>,
    scope: Option<String>,
    #[serde(default = "hour")]
    expires_in: i64,
}
fn hour() -> i64 {
    3600
}

async fn token(provider: Provider, fields: &[(&str, String)]) -> Result<Token, MailError> {
    let (_, endpoint, _) = provider.oauth_endpoints()?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| MailError::new("oauth_unavailable"))?;
    let body: Vec<_> = fields
        .iter()
        .filter(|(name, _)| {
            provider != Provider::Yahoo || !matches!(*name, "client_id" | "client_secret")
        })
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let mut request = client.post(endpoint).form(&body);
    if provider == Provider::Yahoo {
        let id = fields
            .iter()
            .find(|(k, _)| *k == "client_id")
            .map(|(_, v)| v.as_str())
            .unwrap_or_default();
        let secret = fields
            .iter()
            .find(|(k, _)| *k == "client_secret")
            .map(|(_, v)| v.as_str());
        request = request.basic_auth(id, secret);
    }
    let mut response = request
        .send()
        .await
        .map_err(|_| MailError::new("oauth_unavailable"))?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        || response.status().is_server_error()
    {
        return Err(MailError::new("oauth_unavailable"));
    }
    if !response.status().is_success() {
        return Err(MailError::new("oauth_reauthentication_required"));
    }
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| MailError::new("oauth_unavailable"))?
    {
        if bytes.len() + chunk.len() > 65_536 {
            return Err(MailError::new("oauth_response_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let token: Token =
        serde_json::from_slice(&bytes).map_err(|_| MailError::new("invalid_oauth_response"))?;
    if token.access_token.is_empty()
        || token.access_token.len() > 16_384
        || token.refresh_token.as_ref().is_some_and(|s| s.len() > 8192)
    {
        return Err(MailError::new("invalid_oauth_response"));
    }
    Ok(token)
}

pub async fn refresh(provider: Provider, credential: &mut Credential) -> Result<bool, MailError> {
    let Credential::OAuth2 {
        client_id,
        client_secret,
        refresh_token,
        access_token,
        expires_at,
    } = credential
    else {
        return Ok(false);
    };
    if access_token.is_some()
        && expires_at.is_some_and(|at| at > chrono::Utc::now().timestamp() + 120)
    {
        return Ok(false);
    }
    let mut fields = vec![
        ("grant_type", "refresh_token".into()),
        ("client_id", client_id.clone()),
        ("refresh_token", refresh_token.clone()),
    ];
    if let Some(secret) = client_secret {
        fields.push(("client_secret", secret.clone()));
    }
    let result = token(provider, &fields).await;
    for (_, value) in &mut fields {
        value.zeroize();
    }
    let token = result?;
    access_token.zeroize();
    *access_token = Some(token.access_token);
    if let Some(rotated) = token.refresh_token {
        refresh_token.zeroize();
        *refresh_token = rotated;
    }
    *expires_at = Some(chrono::Utc::now().timestamp() + token.expires_in.clamp(60, 86_400));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Start {
        Start {
            provider: Provider::Gmail,
            address: "owner@example.com".into(),
            client_id: "registered-client".into(),
            client_secret: Some("private-client-secret".into()),
            redirect_uri: "http://127.0.0.1:7879/callback".into(),
        }
    }

    #[tokio::test]
    async fn private_setup_uses_pkce_and_single_use_bound_state() {
        let oauth = OAuth::default();
        let started = oauth.start(request()).await.unwrap();
        let url = reqwest::Url::parse(&started.authorization_url).unwrap();
        let query: HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["code_challenge_method"], "S256");
        assert!(!query["code_challenge"].is_empty());
        assert!(!started.authorization_url.contains("private-client-secret"));
        assert!(!started.authorization_url.contains("code_verifier"));
        let wrong = Complete {
            request_id: started.request_id,
            state: "different-state".into(),
            code: "code".into(),
        };
        assert_eq!(
            oauth.complete(wrong).await.unwrap_err().code,
            "invalid_oauth_state"
        );
        let replay = Complete {
            request_id: started.request_id,
            state: query["state"].to_string(),
            code: "code".into(),
        };
        assert_eq!(
            oauth.complete(replay).await.unwrap_err().code,
            "oauth_setup_expired"
        );
    }

    #[tokio::test]
    async fn callback_requires_https_or_loopback_and_setup_is_bounded() {
        let oauth = OAuth::default();
        let mut unsafe_redirect = request();
        unsafe_redirect.redirect_uri = "http://example.com/callback".into();
        assert_eq!(
            oauth.start(unsafe_redirect).await.unwrap_err().code,
            "invalid_oauth_client"
        );
        for _ in 0..32 {
            oauth.start(request()).await.unwrap();
        }
        assert_eq!(
            oauth.start(request()).await.unwrap_err().code,
            "oauth_setup_limit"
        );
    }

    #[tokio::test]
    async fn gmail_can_choose_its_account_and_cancel_without_exhausting_setups() {
        let oauth = OAuth::default();
        let mut start = request();
        start.address.clear();
        let started = oauth.start(start).await.unwrap();
        let url = reqwest::Url::parse(&started.authorization_url).unwrap();
        assert!(!url.query_pairs().any(|(key, _)| key == "login_hint"));
        oauth.cancel(started.request_id).await;
        oauth.cancel(started.request_id).await;
        let state = url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .unwrap()
            .1
            .into_owned();
        assert_eq!(
            oauth
                .complete(Complete {
                    request_id: started.request_id,
                    state,
                    code: "unused".into()
                })
                .await
                .unwrap_err()
                .code,
            "oauth_setup_expired"
        );
        for _ in 0..40 {
            let started = oauth.start(request()).await.unwrap();
            oauth.cancel(started.request_id).await;
        }
    }

    #[tokio::test]
    async fn gmail_address_comes_from_the_authorized_mailbox() {
        use axum::{Json, Router, http::HeaderMap, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/profile", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/profile", get(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer test-mailbox-token");
                Json(serde_json::json!({"emailAddress":"chosen@example.com","messagesTotal":42}))
            }))).await.unwrap();
        });
        assert_eq!(
            gmail_address(&endpoint, "test-mailbox-token")
                .await
                .unwrap(),
            "chosen@example.com"
        );
        server.abort();
    }
}
