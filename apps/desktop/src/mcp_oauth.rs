use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use url::Url;
use uuid::Uuid;

use crate::api::{Client, ClientError, McpAuthorization};

pub struct SignIn {
    pub authorization: McpAuthorization,
    callback: oneshot::Receiver<Result<Value, String>>,
    _cancel: oneshot::Sender<()>,
    client: Arc<Client>,
    bot: Uuid,
    id: String,
}

impl Drop for SignIn {
    fn drop(&mut self) {
        self.client
            .clone()
            .cancel_mcp(self.bot, self.id.clone(), self.authorization.request_id);
    }
}

impl SignIn {
    pub async fn start(
        client: Arc<Client>,
        bot: Uuid,
        id: String,
        expected: Value,
    ) -> Result<Self, ClientError> {
        let this = client.clone();
        client.run(async move {
            let listener = TcpListener::bind("127.0.0.1:0").await
                .map_err(|_| ClientError::Protocol("Cannot open the local sign-in callback.".into()))?;
            let host = listener.local_addr().map_err(|_| ClientError::Protocol("Cannot read the local callback address.".into()))?.to_string();
            let path = format!("/mcp/{}", Uuid::new_v4().simple());
            let redirect = format!("http://{host}{path}");
            let authorization = this.clone().authorize_mcp(bot, id.clone(), redirect.clone()).await?;
            let (callback_tx, callback) = oneshot::channel();
            let (cancel_tx, cancel) = oneshot::channel();
            let signin = Self { authorization, callback, _cancel: cancel_tx, client: this, bot, id };
            if json!(signin.authorization.provider) != expected["provider"] || json!(signin.authorization.scopes) != expected["scopes"] {
                return Err(ClientError::Protocol("The provider or requested permissions changed. Cancel this card and ask the bot to connect again.".into()));
            }
            let state = authorization_state(&signin.authorization.authorization_url, &redirect)
                .ok_or_else(|| ClientError::Protocol("The API returned an invalid MCP sign-in URL.".into()))?;
            tokio::spawn(serve(listener, path, host, state, callback_tx, cancel, signin.authorization.expires_in.min(600)));
            Ok(signin)
        }).await
    }

    pub async fn wait(&mut self) -> Result<Value, ClientError> {
        (&mut self.callback)
            .await
            .map_err(|_| ClientError::Protocol("Sign-in stopped. Try again.".into()))?
            .map_err(ClientError::Protocol)
    }

    pub async fn finish(&mut self, mut callback: Value) -> Result<(), ClientError> {
        callback["request_id"] = json!(self.authorization.request_id);
        self.client
            .clone()
            .complete_mcp(self.bot, self.id.clone(), callback)
            .await
    }
}

fn authorization_state(value: &str, redirect: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    let one = |key: &str| {
        let mut values = url
            .query_pairs()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned());
        let value = values.next()?;
        values.next().is_none().then_some(value)
    };
    if one("redirect_uri")? != redirect
        || one("response_type")? != "code"
        || one("code_challenge_method")? != "S256"
    {
        return None;
    }
    one("state").filter(|s| !s.is_empty() && s.len() <= 128)
}

fn parse_callback(query: &str, state: &str) -> Option<Value> {
    if query.len() > 16_384 {
        return None;
    }
    let pairs: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
    let one = |key: &str| {
        let mut values = pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_ref());
        let value = values.next()?;
        values.next().is_none().then_some(value)
    };
    if one("state")? != state {
        return None;
    }
    let iss = if pairs.iter().any(|(k, _)| k == "iss") {
        Some(one("iss")?)
    } else {
        None
    };
    let code = one("code");
    let error = one("error");
    if pairs
        .iter()
        .filter(|(k, _)| k == "code" || k == "error")
        .count()
        != 1
    {
        return None;
    }
    if code.is_some_and(|s| s.is_empty() || s.len() > 8192) || error.is_some_and(|s| s.len() > 128)
    {
        return None;
    }
    Some(json!({"state":state,"code":code,"error":error,"iss":iss}))
}

#[derive(Clone)]
struct Callback {
    state: String,
    host: String,
    tx: mpsc::Sender<Value>,
}

async fn receive(
    State(callback): State<Callback>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if headers.get("host").and_then(|s| s.to_str().ok()) != Some(callback.host.as_str()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(value) = parse_callback(query.as_deref().unwrap_or_default(), &callback.state) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if callback.tx.try_send(value).is_err() {
        return StatusCode::GONE.into_response();
    }
    ([("cache-control", "no-store"), ("referrer-policy", "no-referrer"), ("content-security-policy", "default-src 'none'; frame-ancestors 'none'")],
        Html("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>else.bot sign-in</title><h1>Return to else.bot</h1><p>Your sign-in response was received. You can close this tab.</p></html>")).into_response()
}

async fn serve(
    listener: TcpListener,
    path: String,
    host: String,
    state: String,
    callback: oneshot::Sender<Result<Value, String>>,
    cancel: oneshot::Receiver<()>,
    expires: u64,
) {
    let (tx, mut rx) = mpsc::channel(1);
    let router = Router::new()
        .route(&path, get(receive))
        .with_state(Callback { host, state, tx });
    let (shutdown, stopped) = oneshot::channel::<()>();
    let server = axum::serve(listener, router).with_graceful_shutdown(async {
        let _ = stopped.await;
    });
    let server = std::future::IntoFuture::into_future(server);
    tokio::pin!(server);
    tokio::select! {
        value = rx.recv() => { let _ = callback.send(value.ok_or_else(|| "Sign-in stopped.".into())); }
        _ = cancel => {}
        _ = tokio::time::sleep(Duration::from_secs(expires)) => { let _ = callback.send(Err("Sign-in expired. Try again.".into())); }
        _ = &mut server => { let _ = callback.send(Err("The local sign-in callback stopped.".into())); return; }
    }
    let _ = shutdown.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_requires_state_and_one_unambiguous_outcome() {
        assert_eq!(
            parse_callback("state=valid&code=ok&iss=https%3A%2F%2Fissuer.test", "valid").unwrap()["code"],
            "ok"
        );
        for query in [
            "state=wrong&code=ok",
            "state=valid&state=valid&code=ok",
            "state=valid&code=ok&code=other",
            "state=valid&code=ok&error=access_denied",
            "state=valid&code=ok&iss=a&iss=b",
            "state=valid&error=x&error=y",
        ] {
            assert!(parse_callback(query, "valid").is_none(), "{query}");
        }
        assert_eq!(
            parse_callback("state=valid&error=access_denied", "valid").unwrap()["error"],
            "access_denied"
        );
    }

    #[tokio::test]
    async fn loopback_accepts_only_matching_host_path_and_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let (tx, rx) = oneshot::channel();
        let (_cancel, cancel) = oneshot::channel();
        let server = tokio::spawn(serve(
            listener,
            "/callback".into(),
            host.clone(),
            "valid".into(),
            tx,
            cancel,
            10,
        ));
        let client = reqwest::Client::new();
        let url = format!("http://{host}/callback");
        assert_eq!(
            client
                .get(format!("{url}?state=wrong&code=bad"))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            client
                .get(format!("{url}?state=valid&code=bad"))
                .header("host", "attacker.test")
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(
            client
                .get(format!("{url}?state=valid&code=good"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(rx.await.unwrap().unwrap()["code"], "good");
        server.await.unwrap();
    }
    #[tokio::test]
    async fn browser_callback_completes_on_selected_api_and_rejects_changed_consent() {
        use crate::config::{Origin, OwnerToken};
        use axum::{Json, routing::post};
        use std::sync::atomic::{AtomicBool, Ordering};
        let bot = Uuid::new_v4();
        let request = Uuid::new_v4();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = Origin::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (sent, mut received) = mpsc::channel(2);
        let (cancelled, mut cancellations) = mpsc::channel(2);
        let (completed, mut completions) = mpsc::channel(2);
        let changed = Arc::new(AtomicBool::new(false));
        let change = changed.clone();
        let router = Router::new()
            .route("/v1/bots/{bot}/mcp/fixture/oauth/start", post(move |Json(body): Json<Value>| {
                let sent = sent.clone();
                let changed = change.load(Ordering::SeqCst);
                async move {
                    let redirect = body["redirect_uri"].as_str().unwrap();
                    let mut url = Url::parse("https://issuer.test/authorize").unwrap();
                    url.query_pairs_mut().extend_pairs([("redirect_uri", redirect), ("state", "state"), ("response_type", "code"), ("code_challenge_method", "S256")]);
                    sent.send(redirect.to_owned()).await.unwrap();
                    Json(json!({"request_id":request,"authorization_url":url.as_str(),"expires_in":600,"provider":"https://issuer.test", "scopes": if changed { vec!["read", "write"] } else { vec!["read"] }}))
                }
            }))
            .route("/v1/bots/{bot}/mcp/fixture/oauth/complete", post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let completed = completed.clone();
                async move {
                    assert!(headers["authorization"].to_str().unwrap().starts_with("Bearer "));
                    completed.send(body).await.unwrap();
                    Json(json!({"status":"connected"}))
                }
            }))
            .route("/v1/bots/{bot}/mcp/fixture/oauth/{request}", axum::routing::delete(move || {
                let cancelled = cancelled.clone();
                async move { cancelled.send(()).await.unwrap(); StatusCode::NO_CONTENT }
            }));
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let owner = OwnerToken::parse(&"T".repeat(43)).unwrap();
        let client =
            Arc::new(Client::new(origin, &owner, tokio::runtime::Handle::current()).unwrap());
        let expected = json!({"provider":"https://issuer.test","scopes":["read"]});
        let mut signin = SignIn::start(client.clone(), bot, "fixture".into(), expected.clone())
            .await
            .unwrap();
        let callback_url = received.recv().await.unwrap();
        let response = reqwest::get(format!(
            "{callback_url}?state=state&code=private-code&iss=https%3A%2F%2Fissuer.test"
        ))
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        let callback = signin.wait().await.unwrap();
        signin.finish(callback).await.unwrap();
        let complete = completions.recv().await.unwrap();
        assert_eq!(complete["request_id"], request.to_string());
        assert_eq!(complete["code"], "private-code");
        drop(signin);
        tokio::time::timeout(Duration::from_secs(2), cancellations.recv())
            .await
            .unwrap()
            .unwrap();
        changed.store(true, Ordering::SeqCst);
        assert!(
            SignIn::start(client, bot, "fixture".into(), expected)
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), cancellations.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(completions.try_recv().is_err());
        server.abort();
    }
}
