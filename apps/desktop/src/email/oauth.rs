use std::{sync::Arc, time::Duration};

use url::Url;

use crate::api::{Client, ClientError, EmailAccount, EmailAuthorization, EmailProvider};
use crate::oauth_callback::{self, Outcome, WaitError};

const GOOGLE_REGISTRATION: Option<(&str, &str)> =
    include!(concat!(env!("OUT_DIR"), "/google_oauth.rs"));

const MICROSOFT_CLIENT_ID: Option<&str> = include!(concat!(env!("OUT_DIR"), "/microsoft_oauth.rs"));

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    Google,
    Microsoft,
}

impl Provider {
    pub fn for_email(provider: EmailProvider) -> Option<Self> {
        match provider {
            EmailProvider::Gmail => Some(Self::Google),
            EmailProvider::Microsoft => Some(Self::Microsoft),
            _ => None,
        }
    }

    fn email(self) -> EmailProvider {
        match self {
            Self::Google => EmailProvider::Gmail,
            Self::Microsoft => EmailProvider::Microsoft,
        }
    }

    pub fn label(self) -> &'static str {
        self.email().label()
    }

    pub fn configured(self) -> bool {
        self.registration().is_some()
    }

    fn registration(self) -> Option<(&'static str, Option<&'static str>)> {
        match self {
            Self::Google => GOOGLE_REGISTRATION.map(|(id, secret)| (id, Some(secret))),
            Self::Microsoft => MICROSOFT_CLIENT_ID.map(|id| (id, None)),
        }
    }

    fn callback(self, port: u16) -> (String, String) {
        match self {
            Self::Google => {
                let path = format!("/google/{}", uuid::Uuid::new_v4().simple());
                (format!("http://127.0.0.1:{port}{path}"), path)
            }
            Self::Microsoft => (format!("http://localhost:{port}/"), "/".into()),
        }
    }

    fn authorize_endpoint(self) -> (&'static str, &'static str) {
        match self {
            Self::Google => ("accounts.google.com", "/o/oauth2/v2/auth"),
            Self::Microsoft => ("login.microsoftonline.com", "/common/oauth2/v2.0/authorize"),
        }
    }
}

pub(crate) struct SignIn {
    pub authorization: EmailAuthorization,
    callback: oauth_callback::Pending,
    client: Arc<Client>,
}

impl Drop for SignIn {
    fn drop(&mut self) {
        self.client
            .clone()
            .cancel_email_authorization(self.authorization.request_id);
    }
}

pub(crate) struct Callback {
    pub state: String,
    pub code: String,
}

impl SignIn {
    pub async fn start(
        client: Arc<Client>,
        provider: Provider,
        address: String,
    ) -> Result<Self, ClientError> {
        let registration = provider.registration().ok_or_else(|| {
            ClientError::Protocol(format!(
                "This build does not include {} sign-in. Use an official else.bot release.",
                provider.label()
            ))
        })?;
        Self::start_registered(client, provider, address, registration).await
    }

    async fn start_registered(
        client: Arc<Client>,
        provider: Provider,
        address: String,
        registration: (&'static str, Option<&'static str>),
    ) -> Result<Self, ClientError> {
        let this = client.clone();
        client
            .run(async move { Self::start_on_runtime(this, provider, address, registration).await })
            .await
    }

    async fn start_on_runtime(
        client: Arc<Client>,
        provider: Provider,
        address: String,
        (client_id, client_secret): (&str, Option<&str>),
    ) -> Result<Self, ClientError> {
        let (listener, callback) = oauth_callback::Listener::bind().await.map_err(|_| {
            ClientError::Protocol("Cannot open the local email sign-in callback.".into())
        })?;
        let (redirect_uri, _) = provider.callback(listener.port());
        let authorization = client
            .clone()
            .authorize_email(
                provider.email(),
                address,
                client_id.into(),
                client_secret.map(str::to_owned),
                redirect_uri.clone(),
            )
            .await?;
        let signin = Self {
            authorization,
            callback,
            client,
        };
        let state = authorization_state(
            provider,
            &signin.authorization.authorization_url,
            client_id,
            &redirect_uri,
        )
        .ok_or_else(|| {
            ClientError::Protocol("The API returned an invalid email sign-in URL.".into())
        })?;
        listener.start(
            &Url::parse(&redirect_uri).expect("local callback URL"),
            state,
            Duration::from_secs(600),
        );
        Ok(signin)
    }

    pub async fn finish(&self, callback: Callback) -> Result<EmailAccount, ClientError> {
        self.client
            .clone()
            .complete_email(self.authorization.request_id, callback.state, callback.code)
            .await
    }

    pub async fn wait(&mut self) -> Result<Callback, String> {
        let callback = self.callback.wait().await.map_err(|error| match error {
            WaitError::Stopped => "Email sign-in stopped. Try again.",
            WaitError::Expired => "Email sign-in expired. Try again.",
        })?;
        match callback.outcome {
            Outcome::Code(code) => Ok(Callback {
                state: callback.state,
                code,
            }),
            Outcome::Error(error) => Err(if error == "access_denied" {
                "Email sign-in was cancelled. You can try again."
            } else {
                "Your email provider could not authorize this account. Try signing in again."
            }
            .into()),
        }
    }
}

fn authorization_state(
    provider: Provider,
    value: &str,
    client_id: &str,
    redirect_uri: &str,
) -> Option<String> {
    let (host, path) = provider.authorize_endpoint();
    let url = Url::parse(value).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some(host)
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != path
        || url.fragment().is_some()
    {
        return None;
    }
    let one = |name: &str| {
        let mut values = url
            .query_pairs()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned());
        let value = values.next()?;
        values.next().is_none().then_some(value)
    };
    if one("client_id")? != client_id
        || one("redirect_uri")? != redirect_uri
        || one("response_type")? != "code"
        || one("code_challenge_method")? != "S256"
    {
        return None;
    }
    one("state").filter(|s| !s.is_empty() && s.len() <= 128)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{HeaderMap, StatusCode},
    };
    use tokio::{net::TcpListener, sync::mpsc};

    const FIXTURE_REGISTRATION: (&str, Option<&str>) =
        ("fixture.apps.googleusercontent.com", Some("fixture-secret"));

    #[test]
    fn microsoft_uses_registered_localhost_callback_and_common_authority() {
        let (redirect, path) = Provider::Microsoft.callback(12345);
        assert_eq!(redirect, "http://localhost:12345/");
        assert_eq!(path, "/");
        let mut url =
            Url::parse("https://login.microsoftonline.com/common/oauth2/v2.0/authorize").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("client_id", "registered"),
            ("redirect_uri", redirect.as_str()),
            ("response_type", "code"),
            ("code_challenge_method", "S256"),
            ("state", "bound-state"),
        ]);
        assert_eq!(
            authorization_state(Provider::Microsoft, url.as_str(), "registered", &redirect)
                .as_deref(),
            Some("bound-state")
        );
        assert!(
            authorization_state(Provider::Google, url.as_str(), "registered", &redirect).is_none()
        );
        url.set_path("/untrusted-tenant/oauth2/v2.0/authorize");
        assert!(
            authorization_state(Provider::Microsoft, url.as_str(), "registered", &redirect)
                .is_none()
        );
    }

    #[tokio::test]
    async fn abandoned_or_rejected_starts_cancel_the_api_request() {
        use crate::config::{Origin, OwnerToken};
        use axum::{
            Json,
            routing::{delete, post},
        };
        use serde_json::{Value, json};
        for abandon in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin =
                Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let gate = Arc::new(tokio::sync::Notify::new());
            let release = gate.clone();
            let (started, mut starts) = mpsc::channel(1);
            let (cancelled, mut cancellations) = mpsc::channel(1);
            let router = Router::new().route("/v1/connectors/email/oauth/start", post(move |Json(body): Json<Value>| {
                let gate = gate.clone();
                let started = started.clone();
                async move {
                    let mut url = Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
                    url.query_pairs_mut().extend_pairs([
                        ("client_id", body["client_id"].as_str().unwrap()), ("redirect_uri", body["redirect_uri"].as_str().unwrap()),
                        ("state", "bound-state"), ("response_type", "code"), ("code_challenge_method", "S256"),
                    ]);
                    if !abandon { url.set_host(Some("attacker.example")).unwrap(); }
                    started.send(()).await.unwrap();
                    gate.notified().await;
                    Json(json!({"request_id":"11111111-1111-4111-8111-111111111111", "authorization_url":url.as_str()}))
                }
            })).route("/v1/connectors/email/oauth/{id}", delete(move || {
                let cancelled = cancelled.clone();
                async move { cancelled.send(()).await.unwrap(); Json(json!({"cancelled":true})) }
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let token = OwnerToken::parse(&"T".repeat(43)).unwrap();
            let client =
                Arc::new(Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap());
            let start = tokio::spawn(SignIn::start_registered(
                client,
                Provider::Google,
                String::new(),
                FIXTURE_REGISTRATION,
            ));
            starts.recv().await.unwrap();
            if abandon {
                start.abort();
                assert!(matches!(start.await, Err(error) if error.is_cancelled()));
                release.notify_one();
            } else {
                release.notify_one();
                assert!(start.await.unwrap().is_err());
            }
            tokio::time::timeout(Duration::from_secs(5), cancellations.recv())
                .await
                .unwrap()
                .unwrap();
            server.abort();
        }
    }

    #[tokio::test]
    async fn desktop_registration_and_callback_reach_the_selected_api() {
        use crate::config::{Origin, OwnerToken};
        use axum::{
            Json,
            extract::Path,
            routing::{delete, post},
        };
        use serde_json::{Value, json};
        for (provider, provider_name, endpoint, registration, address) in [
            (
                Provider::Google,
                "gmail",
                "https://accounts.google.com/o/oauth2/v2/auth",
                FIXTURE_REGISTRATION,
                "",
            ),
            (
                Provider::Microsoft,
                "microsoft",
                "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
                ("11111111-1111-4111-8111-111111111111", None),
                "owner@example.com",
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin =
                Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            let (started, mut starts) = mpsc::channel(1);
            let (cancelled, mut cancellations) = mpsc::channel(1);
            let router = Router::new()
            .route("/v1/connectors/email/oauth/start", post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let started = started.clone();
                async move {
                    assert!(headers["authorization"].to_str().unwrap().starts_with("Bearer "));
                    assert_eq!(body["provider"], provider_name);
                    assert_eq!(body["address"], address);
                    assert_eq!(body["client_id"], registration.0);
                    assert_eq!(body["client_secret"], json!(registration.1));
                    let mut url = Url::parse(endpoint).unwrap();
                    url.query_pairs_mut().extend_pairs([
                        ("client_id", body["client_id"].as_str().unwrap()), ("redirect_uri", body["redirect_uri"].as_str().unwrap()),
                        ("state", "bound-state"), ("response_type", "code"), ("code_challenge_method", "S256"),
                    ]);
                    started.send(body["redirect_uri"].as_str().unwrap().to_owned()).await.unwrap();
                    Json(json!({"request_id":"11111111-1111-4111-8111-111111111111", "authorization_url":url.as_str()}))
                }
            }))
            .route("/v1/connectors/email/oauth/{id}", delete(move |Path(id): Path<uuid::Uuid>| {
                let cancelled = cancelled.clone();
                async move { cancelled.send(id).await.unwrap(); Json(json!({"cancelled":true})) }
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let token = OwnerToken::parse(&"T".repeat(43)).unwrap();
            let client =
                Arc::new(Client::new(origin, &token, tokio::runtime::Handle::current()).unwrap());
            let mut signin =
                SignIn::start_registered(client.clone(), provider, address.into(), registration)
                    .await
                    .unwrap();
            let callback = starts.recv().await.unwrap();
            if provider == Provider::Microsoft {
                let redirect = Url::parse(&callback).unwrap();
                assert_eq!(redirect.host_str(), Some("localhost"));
                assert_eq!(redirect.path(), "/");
            }
            let response = reqwest::Client::builder()
                .no_proxy()
                .build()
                .unwrap()
                .get(format!("{callback}?state=bound-state&code=provider-code"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let result = signin.wait().await.unwrap();
            assert_eq!(result.code, "provider-code");
            assert_eq!(result.state, "bound-state");
            drop(signin);
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), cancellations.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .to_string(),
                "11111111-1111-4111-8111-111111111111"
            );
            let mut denied =
                SignIn::start_registered(client.clone(), provider, address.into(), registration)
                    .await
                    .unwrap();
            let callback = starts.recv().await.unwrap();
            let response = reqwest::Client::builder().no_proxy().build().unwrap()
                .get(format!("{callback}?state=bound-state&error=access_denied&error_description=untrusted%20secret"))
                .send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let error = denied.wait().await.err().unwrap();
            assert!(error.contains("cancelled"));
            assert!(!error.contains("untrusted secret"));
            drop(denied);
            tokio::time::timeout(Duration::from_secs(5), cancellations.recv())
                .await
                .unwrap()
                .unwrap();
            server.abort();
        }
    }

    #[test]
    fn sign_in_only_opens_google_for_the_registered_app_and_local_callback() {
        let mut url = Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
        url.query_pairs_mut().extend_pairs([
            ("client_id", "registered"),
            ("redirect_uri", "http://127.0.0.1:9999/callback"),
            ("response_type", "code"),
            ("code_challenge_method", "S256"),
            ("state", "bound-state"),
        ]);
        assert_eq!(
            authorization_state(
                Provider::Google,
                url.as_str(),
                "registered",
                "http://127.0.0.1:9999/callback"
            )
            .as_deref(),
            Some("bound-state")
        );
        assert!(
            authorization_state(
                Provider::Google,
                url.as_str(),
                "different",
                "http://127.0.0.1:9999/callback"
            )
            .is_none()
        );
        assert!(
            authorization_state(
                Provider::Google,
                url.as_str(),
                "registered",
                "http://127.0.0.1:8888/callback"
            )
            .is_none()
        );
        url.set_host(Some("attacker.example")).unwrap();
        assert!(
            authorization_state(
                Provider::Google,
                url.as_str(),
                "registered",
                "http://127.0.0.1:9999/callback"
            )
            .is_none()
        );
    }
}
