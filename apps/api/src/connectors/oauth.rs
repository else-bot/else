use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet,
    EndpointSet, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, RefreshToken, RequestTokenError,
    Scope, TokenResponse, TokenUrl,
    basic::{BasicClient, BasicErrorResponse, BasicTokenResponse},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroize;

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
    state: CsrfToken,
    verifier: PkceCodeVerifier,
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

type Client = BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

fn client(provider: Provider, id: &str, secret: Option<&str>) -> Result<Client, MailError> {
    let (authorize, token, _) = provider.oauth_endpoints()?;
    let client = BasicClient::new(ClientId::new(id.to_owned()))
        .set_auth_uri(AuthUrl::new(authorize.into()).expect("fixed provider URL"))
        .set_token_uri(TokenUrl::new(token.into()).expect("fixed provider URL"))
        .set_auth_type(if provider == Provider::Yahoo {
            AuthType::BasicAuth
        } else {
            AuthType::RequestBody
        });
    Ok(match secret {
        Some(secret) => client.set_client_secret(ClientSecret::new(secret.to_owned())),
        None => client,
    })
}

fn redirect(uri: &str) -> Result<RedirectUrl, MailError> {
    RedirectUrl::new(uri.to_owned()).map_err(|_| MailError::new("invalid_redirect"))
}

/// Sends the token request without following redirects, and reports a
/// throttled or failing provider as unavailable so the account is not marked
/// for reauthentication during an outage.
async fn http(request: oauth2::HttpRequest) -> Result<oauth2::HttpResponse, std::io::Error> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(std::io::Error::other)?;
    let response = client
        .execute(request.try_into().map_err(std::io::Error::other)?)
        .await
        .map_err(std::io::Error::other)?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Err(std::io::Error::other(format!(
            "token endpoint answered {status}"
        )));
    }
    let headers = response.headers().clone();
    let body = response.bytes().await.map_err(std::io::Error::other)?;
    let mut reply = oauth2::HttpResponse::new(body.to_vec());
    *reply.status_mut() = status;
    *reply.headers_mut() = headers;
    Ok(reply)
}

fn token_error(error: RequestTokenError<std::io::Error, BasicErrorResponse>) -> MailError {
    MailError::new(match error {
        RequestTokenError::ServerResponse(_) => "oauth_reauthentication_required",
        RequestTokenError::Request(_) => "oauth_unavailable",
        RequestTokenError::Parse(..) | RequestTokenError::Other(_) => "invalid_oauth_response",
    })
}

fn expires_at(token: &BasicTokenResponse) -> i64 {
    let seconds = token.expires_in().map_or(3600, |ttl| ttl.as_secs());
    chrono::Utc::now().timestamp() + seconds.clamp(60, 86_400) as i64
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
        let redirect_url = reqwest::Url::parse(&request.redirect_uri)
            .map_err(|_| MailError::new("invalid_redirect"))?;
        let loopback = matches!(
            redirect_url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]")
        );
        if !(redirect_url.scheme() == "https" || (redirect_url.scheme() == "http" && loopback))
            || redirect_url.fragment().is_some()
            || !redirect_url.username().is_empty()
            || redirect_url.password().is_some()
            || request.client_id.is_empty()
            || request.client_id.len() > 1024
            || request
                .client_secret
                .as_ref()
                .is_some_and(|s| s.len() > 4096)
        {
            return Err(MailError::new("invalid_oauth_client"));
        }
        let (_, _, scopes) = request.provider.oauth_endpoints()?;
        let client = client(
            request.provider,
            &request.client_id,
            request.client_secret.as_deref(),
        )?
        .set_redirect_uri(redirect(&request.redirect_uri)?);
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let mut authorization = client
            .authorize_url(CsrfToken::new_random)
            .add_scopes(scopes.split(' ').map(|scope| Scope::new(scope.into())))
            .set_pkce_challenge(challenge)
            .add_extra_param("access_type", "offline")
            .add_extra_param("prompt", "consent");
        if !request.address.is_empty() {
            authorization = authorization.add_extra_param("login_hint", request.address.as_str());
        }
        let (url, state) = authorization.url();
        let request_id = Uuid::new_v4();
        let pending = Pending {
            request,
            state,
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
        if pending.expires <= Instant::now() || pending.state != CsrfToken::new(request.state) {
            return Err(MailError::new("invalid_oauth_state"));
        }
        let Pending {
            request: setup,
            verifier,
            ..
        } = pending;
        let token = client(
            setup.provider,
            &setup.client_id,
            setup.client_secret.as_deref(),
        )?
        .set_redirect_uri(redirect(&setup.redirect_uri)?)
        .exchange_code(AuthorizationCode::new(request.code))
        .set_pkce_verifier(verifier)
        .request_async(&http)
        .await
        .map_err(token_error)?;
        if setup.provider == Provider::Gmail
            && token.scopes().is_some_and(|scopes| {
                !scopes
                    .iter()
                    .any(|scope| scope.as_str() == "https://mail.google.com/")
            })
        {
            return Err(MailError::new("oauth_mail_access_required"));
        }
        let access_token = token.access_token().secret();
        let address = if setup.provider == Provider::Gmail {
            gmail_address(
                "https://gmail.googleapis.com/gmail/v1/users/me/profile",
                access_token,
            )
            .await?
        } else {
            setup.address.clone()
        };
        let refresh_token = token
            .refresh_token()
            .ok_or_else(|| MailError::new("oauth_refresh_token_required"))?;
        let credential = Credential::OAuth2 {
            client_id: setup.client_id.clone(),
            client_secret: setup.client_secret.clone(),
            refresh_token: refresh_token.secret().clone(),
            access_token: Some(access_token.clone()),
            expires_at: Some(expires_at(&token)),
        };
        Ok((setup.provider, address, credential))
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
    let response = client
        .get(endpoint)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| MailError::new("oauth_unavailable"))?;
    if !response.status().is_success() {
        return Err(MailError::new("oauth_reauthentication_required"));
    }
    let profile: Profile = response
        .json()
        .await
        .map_err(|_| MailError::new("invalid_oauth_response"))?;
    Provider::Gmail
        .config(profile.address.clone())?
        .validate()?;
    Ok(profile.address)
}

pub async fn refresh(provider: Provider, credential: &mut Credential) -> Result<bool, MailError> {
    let Credential::OAuth2 {
        client_id,
        client_secret,
        refresh_token,
        access_token,
        expires_at: expiry,
    } = credential
    else {
        return Ok(false);
    };
    if access_token.is_some() && expiry.is_some_and(|at| at > chrono::Utc::now().timestamp() + 120)
    {
        return Ok(false);
    }
    let token = client(provider, client_id, client_secret.as_deref())?
        .exchange_refresh_token(&RefreshToken::new(refresh_token.clone()))
        .request_async(&http)
        .await
        .map_err(token_error)?;
    access_token.zeroize();
    *access_token = Some(token.access_token().secret().clone());
    if let Some(rotated) = token.refresh_token() {
        refresh_token.zeroize();
        *refresh_token = rotated.secret().clone();
    }
    *expiry = Some(expires_at(&token));
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
    async fn yahoo_authenticates_with_basic_auth_and_others_send_the_secret_in_the_body() {
        for provider in [Provider::Yahoo, Provider::Gmail] {
            let sent = std::sync::Mutex::new(None);
            let token = client(provider, "registered-client", Some("private-client-secret"))
                .unwrap()
                .exchange_refresh_token(&RefreshToken::new("private-refresh".into()))
                .request_async(&|request: oauth2::HttpRequest| {
                    *sent.lock().unwrap() = Some(request);
                    async {
                        let mut response = oauth2::HttpResponse::new(
                            br#"{"access_token":"fresh","token_type":"bearer","expires_in":10}"#
                                .to_vec(),
                        );
                        response.headers_mut().insert(
                            reqwest::header::CONTENT_TYPE,
                            "application/json".parse().unwrap(),
                        );
                        Ok::<_, std::io::Error>(response)
                    }
                })
                .await
                .unwrap();
            assert_eq!(token.access_token().secret(), "fresh");
            assert!((60..=61).contains(&(expires_at(&token) - chrono::Utc::now().timestamp())));
            let request = sent.into_inner().unwrap().unwrap();
            let body = String::from_utf8(request.body().clone()).unwrap();
            assert!(body.contains("refresh_token=private-refresh"));
            let basic = request.headers().get(reqwest::header::AUTHORIZATION);
            if provider == Provider::Yahoo {
                assert!(basic.unwrap().to_str().unwrap().starts_with("Basic "));
                assert!(!body.contains("client_secret"));
            } else {
                assert!(basic.is_none());
                assert!(body.contains("client_secret=private-client-secret"));
            }
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
