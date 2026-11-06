use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use url::Url;

use crate::api::{Client, ClientError, EmailAuthorization, EmailProvider};

const REGISTRATION: Option<(&str, &str)> = include!(concat!(env!("OUT_DIR"), "/google_oauth.rs"));

pub(super) fn configured() -> bool {
    REGISTRATION.is_some()
}

pub(super) struct SignIn {
    pub authorization: EmailAuthorization,
    callback: oneshot::Receiver<Result<Callback, String>>,
    _cancel: oneshot::Sender<()>,
    client: Arc<Client>,
}

impl Drop for SignIn {
    fn drop(&mut self) {
        self.client
            .clone()
            .cancel_email_authorization(self.authorization.request_id);
    }
}

pub(super) struct Callback {
    pub state: String,
    pub code: String,
}

impl SignIn {
    pub async fn start(client: Arc<Client>) -> Result<Self, ClientError> {
        let registration = REGISTRATION.ok_or_else(|| {
            ClientError::Protocol(
                "This build does not include Gmail sign-in. Use an official else.bot release."
                    .into(),
            )
        })?;
        Self::start_registered(client, registration).await
    }

    async fn start_registered(
        client: Arc<Client>,
        registration: (&'static str, &'static str),
    ) -> Result<Self, ClientError> {
        let this = client.clone();
        client
            .run(async move { Self::start_on_runtime(this, registration).await })
            .await
    }

    async fn start_on_runtime(
        client: Arc<Client>,
        (client_id, client_secret): (&str, &str),
    ) -> Result<Self, ClientError> {
        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|_| {
            ClientError::Protocol("Cannot open the local Google sign-in callback.".into())
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| ClientError::Protocol(e.to_string()))?
            .port();
        let path = format!("/google/{}", uuid::Uuid::new_v4().simple());
        let redirect_uri = format!("http://127.0.0.1:{port}{path}");
        let authorization = client
            .clone()
            .authorize_email(
                EmailProvider::Gmail,
                String::new(),
                client_id.into(),
                Some(client_secret.into()),
                redirect_uri.clone(),
            )
            .await?;
        let (callback_tx, callback) = oneshot::channel();
        let (cancel, cancel_rx) = oneshot::channel();
        let signin = Self {
            authorization,
            callback,
            _cancel: cancel,
            client,
        };
        let state = authorization_state(
            &signin.authorization.authorization_url,
            client_id,
            &redirect_uri,
        )
        .ok_or_else(|| {
            ClientError::Protocol("The API returned an invalid Google sign-in URL.".into())
        })?;
        tokio::spawn(serve(listener, path, state, callback_tx, cancel_rx));
        Ok(signin)
    }

    pub async fn wait(&mut self) -> Result<Callback, String> {
        (&mut self.callback)
            .await
            .map_err(|_| "Google sign-in stopped. Try again.".to_owned())?
    }
}

fn authorization_state(value: &str, client_id: &str, redirect_uri: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    if url.scheme() != "https"
        || url.host_str() != Some("accounts.google.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/o/oauth2/v2/auth"
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

fn parse_callback(query: &str, expected_state: &str) -> Option<Result<Callback, String>> {
    if query.len() > 16_384 {
        return None;
    }
    let pairs: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
    let one = |name: &str| {
        let mut values = pairs
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_ref());
        let value = values.next()?;
        values.next().is_none().then_some(value)
    };
    if one("state")? != expected_state {
        return None;
    }
    if let Some(error) = one("error") {
        if pairs.iter().any(|(key, _)| key == "code") {
            return None;
        }
        return Some(Err(if error == "access_denied" {
            "Google sign-in was cancelled. You can try again."
        } else {
            "Google could not authorize this account. Try signing in again."
        }
        .into()));
    }
    if pairs.iter().any(|(key, _)| key == "error") {
        return None;
    }
    let code = one("code")?;
    if code.is_empty() || code.len() > 8192 {
        return None;
    }
    Some(Ok(Callback {
        state: expected_state.into(),
        code: code.into(),
    }))
}

#[derive(Clone)]
struct BrowserCallback {
    state: String,
    host: String,
    requests: mpsc::Sender<Result<Callback, String>>,
}

async fn receive(
    State(callback): State<BrowserCallback>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if headers.get("host").and_then(|v| v.to_str().ok()) != Some(callback.host.as_str()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(result) = parse_callback(query.as_deref().unwrap_or_default(), &callback.state) else {
        return (
            StatusCode::BAD_REQUEST,
            "This sign-in response is invalid. Return to else.bot and try again.",
        )
            .into_response();
    };
    if callback.requests.send(result).await.is_err() {
        return StatusCode::GONE.into_response();
    }
    (
        [("cache-control", "no-store"), ("referrer-policy", "no-referrer"),
         ("content-security-policy", "default-src 'none'; frame-ancestors 'none'")],
        Html("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>else.bot sign-in</title><h1>Return to else.bot</h1><p>Your sign-in response was received. You can close this tab.</p></html>"),
    ).into_response()
}

async fn serve(
    listener: TcpListener,
    path: String,
    state: String,
    callback: oneshot::Sender<Result<Callback, String>>,
    cancel: oneshot::Receiver<()>,
) {
    let host = listener
        .local_addr()
        .expect("bound callback listener")
        .to_string();
    let (requests, mut receiver) = mpsc::channel(1);
    let router = Router::new()
        .route(&path, get(receive))
        .with_state(BrowserCallback {
            state,
            host,
            requests,
        });
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    let server = axum::serve(listener, router).with_graceful_shutdown(async {
        let _ = shutdown_rx.await;
    });
    let server = std::future::IntoFuture::into_future(server);
    tokio::pin!(server);
    tokio::select! {
        result = receiver.recv() => { let _ = callback.send(result.unwrap_or_else(|| Err("Google sign-in stopped. Try again.".into()))); }
        _ = cancel => {}
        _ = tokio::time::sleep(Duration::from_secs(600)) => { let _ = callback.send(Err("Google sign-in expired. Try again.".into())); }
        _ = &mut server => { let _ = callback.send(Err("The local Google callback stopped. Try again.".into())); return; }
    }
    let _ = shutdown.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_REGISTRATION: (&str, &str) =
        ("fixture.apps.googleusercontent.com", "fixture-secret");

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
            let start = tokio::spawn(SignIn::start_registered(client, FIXTURE_REGISTRATION));
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (started, mut starts) = mpsc::channel(1);
        let (cancelled, mut cancellations) = mpsc::channel(1);
        let router = Router::new()
            .route("/v1/connectors/email/oauth/start", post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let started = started.clone();
                async move {
                    assert!(headers["authorization"].to_str().unwrap().starts_with("Bearer "));
                    assert_eq!(body["provider"], "gmail");
                    assert_eq!(body["address"], "");
                    assert_eq!(body["client_id"], "fixture.apps.googleusercontent.com");
                    assert_eq!(body["client_secret"], "fixture-secret");
                    let mut url = Url::parse("https://accounts.google.com/o/oauth2/v2/auth").unwrap();
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
        let mut signin = SignIn::start_registered(client, FIXTURE_REGISTRATION)
            .await
            .unwrap();
        let callback = starts.recv().await.unwrap();
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
        server.abort();
    }

    async fn listener() -> (
        String,
        oneshot::Receiver<Result<Callback, String>>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/callback", listener.local_addr().unwrap());
        let (result, callback) = oneshot::channel();
        let (cancel, cancelled) = oneshot::channel();
        let task = tokio::spawn(serve(
            listener,
            "/callback".into(),
            "expected-state".into(),
            result,
            cancelled,
        ));
        (url, callback, cancel, task)
    }

    #[tokio::test]
    async fn browser_callback_rejects_forgery_then_delivers_code_once() {
        let (url, callback, _cancel, task) = listener().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for query in [
            "state=wrong&code=stolen",
            "state=expected-state&state=wrong&code=stolen",
            "state=expected-state&code=one&code=two",
            "state=expected-state&code=one&error=access_denied",
        ] {
            assert_eq!(
                client
                    .get(format!("{url}?{query}"))
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            client
                .get(format!("{url}?state=expected-state&code=stolen"))
                .header("host", "attacker.example")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let response = client
            .get(format!("{url}?state=expected-state&code=valid%2Bcode"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.text().await.unwrap().contains("valid+code"));
        let result = callback.await.unwrap().unwrap();
        assert_eq!(result.state, "expected-state");
        assert_eq!(result.code, "valid+code");
        task.await.unwrap();
        assert!(client.get(url).send().await.is_err());
    }

    #[tokio::test]
    async fn denial_and_cancellation_close_the_listener() {
        let (url, callback, _cancel, task) = listener().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            client
                .get(format!("{url}?state=expected-state&error=access_denied&error_description=untrusted%20secret"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        let error = callback.await.unwrap().err().unwrap();
        assert!(error.contains("cancelled"));
        assert!(!error.contains("untrusted secret"));
        task.await.unwrap();
        let (url, callback, cancel, task) = listener().await;
        drop(cancel);
        task.await.unwrap();
        assert!(callback.await.is_err());
        assert!(client.get(url).send().await.is_err());
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
            authorization_state(url.as_str(), "registered", "http://127.0.0.1:9999/callback")
                .as_deref(),
            Some("bound-state")
        );
        assert!(
            authorization_state(url.as_str(), "different", "http://127.0.0.1:9999/callback")
                .is_none()
        );
        assert!(
            authorization_state(url.as_str(), "registered", "http://127.0.0.1:8888/callback")
                .is_none()
        );
        url.set_host(Some("attacker.example")).unwrap();
        assert!(
            authorization_state(url.as_str(), "registered", "http://127.0.0.1:9999/callback")
                .is_none()
        );
    }
}
