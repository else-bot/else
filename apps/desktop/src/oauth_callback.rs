use std::time::Duration;

use axum::{
    Router,
    extract::{RawQuery, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use serde::Serialize;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
};
use url::Url;

#[derive(Debug, PartialEq, Serialize)]
pub struct Callback {
    pub state: String,
    #[serde(flatten)]
    pub outcome: Outcome,
    #[serde(rename = "iss", skip_serializing_if = "Option::is_none")]
    pub issuer: Option<String>,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Code(String),
    Error(String),
}

#[derive(Debug, PartialEq)]
pub enum WaitError {
    Stopped,
    Expired,
}

pub struct Listener {
    socket: TcpListener,
    port: u16,
    result: oneshot::Sender<Result<Callback, WaitError>>,
    cancelled: oneshot::Receiver<()>,
}

pub struct Pending {
    result: Option<oneshot::Receiver<Result<Callback, WaitError>>>,
    _cancel: oneshot::Sender<()>,
}

impl Listener {
    pub async fn bind() -> std::io::Result<(Self, Pending)> {
        let socket = TcpListener::bind("127.0.0.1:0").await?;
        let port = socket.local_addr()?.port();
        let (result, receive) = oneshot::channel();
        let (cancel, cancelled) = oneshot::channel();
        Ok((
            Self {
                socket,
                port,
                result,
                cancelled,
            },
            Pending {
                result: Some(receive),
                _cancel: cancel,
            },
        ))
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn start(
        self,
        redirect_uri: &Url,
        state: String,
        expires: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let host = redirect_uri.authority().to_owned();
        let path = redirect_uri.path().to_owned();
        tokio::spawn(self.serve(path, host, state, expires))
    }

    async fn serve(self, path: String, host: String, state: String, expires: Duration) {
        let (requests, mut receiver) = mpsc::channel::<Delivery>(1);
        let router = Router::new()
            .route(&path, get(receive))
            .with_state(BrowserCallback {
                state,
                host,
                requests,
            });
        let (shutdown, stopped) = oneshot::channel::<()>();
        let server = axum::serve(self.socket, router).with_graceful_shutdown(async {
            let _ = stopped.await;
        });
        let server = std::future::IntoFuture::into_future(server);
        tokio::pin!(server);
        tokio::select! {
            delivery = receiver.recv() => {
                receiver.close();
                if let Some(delivery) = delivery {
                    let _ = delivery.accepted.send(());
                    let _ = self.result.send(Ok(delivery.callback));
                }
            }
            _ = self.cancelled => {}
            _ = tokio::time::sleep(expires.min(Duration::from_secs(600))) => {
                let _ = self.result.send(Err(WaitError::Expired));
            }
            _ = &mut server => {
                let _ = self.result.send(Err(WaitError::Stopped));
                return;
            }
        }
        drop(receiver);
        let _ = shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
    }
}

impl Pending {
    pub async fn wait(&mut self) -> Result<Callback, WaitError> {
        self.result
            .take()
            .ok_or(WaitError::Stopped)?
            .await
            .map_err(|_| WaitError::Stopped)?
    }
}

fn parse(query: &str, state: &str) -> Option<Callback> {
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
    if one("state")? != state {
        return None;
    }
    let issuer = if pairs.iter().any(|(key, _)| key == "iss") {
        Some(one("iss")?.to_owned())
    } else {
        None
    };
    if pairs
        .iter()
        .filter(|(key, _)| key == "code" || key == "error")
        .count()
        != 1
    {
        return None;
    }
    let outcome = if let Some(code) = one("code") {
        if code.is_empty() || code.len() > 8192 {
            return None;
        }
        Outcome::Code(code.to_owned())
    } else {
        let error = one("error")?;
        if error.is_empty() || error.len() > 128 {
            return None;
        }
        Outcome::Error(error.to_owned())
    };
    Some(Callback {
        state: state.to_owned(),
        outcome,
        issuer,
    })
}

struct Delivery {
    callback: Callback,
    accepted: oneshot::Sender<()>,
}

#[derive(Clone)]
struct BrowserCallback {
    state: String,
    host: String,
    requests: mpsc::Sender<Delivery>,
}

async fn receive(
    State(callback): State<BrowserCallback>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Response {
    if headers.get("host").and_then(|value| value.to_str().ok()) != Some(callback.host.as_str()) {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(callback_value) = parse(query.as_deref().unwrap_or_default(), &callback.state) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (accepted, acceptance) = oneshot::channel();
    if callback
        .requests
        .try_send(Delivery {
            callback: callback_value,
            accepted,
        })
        .is_err()
        || acceptance.await.is_err()
    {
        return StatusCode::GONE.into_response();
    }
    ([("cache-control", "no-store"), ("referrer-policy", "no-referrer"), ("content-security-policy", "default-src 'none'; frame-ancestors 'none'")],
        Html("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>else.bot sign-in</title><h1>Return to else.bot</h1><p>Your sign-in response was received. You can close this tab.</p></html>")).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn listener() -> (String, Pending, tokio::task::JoinHandle<()>) {
        let (listener, pending) = Listener::bind().await.unwrap();
        let uri = Url::parse(&format!("http://127.0.0.1:{}/callback", listener.port())).unwrap();
        let task = listener.start(&uri, "expected-state".into(), Duration::from_secs(10));
        (uri.to_string(), pending, task)
    }

    #[test]
    fn callbacks_require_one_state_one_outcome_and_at_most_one_issuer() {
        let callback = parse("state=valid&code=ok&iss=https%3A%2F%2Fissuer.test", "valid").unwrap();
        assert_eq!(callback.outcome, Outcome::Code("ok".into()));
        assert_eq!(callback.issuer.as_deref(), Some("https://issuer.test"));
        assert_eq!(
            serde_json::to_value(callback).unwrap(),
            serde_json::json!({"state":"valid","code":"ok","iss":"https://issuer.test"})
        );
        for query in [
            "state=wrong&code=ok",
            "state=valid&state=valid&code=ok",
            "state=valid&code=ok&code=other",
            "state=valid&code=ok&error=access_denied",
            "state=valid&code=ok&iss=a&iss=b",
            "state=valid&error=x&error=y",
            "state=valid&error=",
            "state=valid&code=",
        ] {
            assert!(parse(query, "valid").is_none(), "{query}");
        }
        assert_eq!(
            parse(
                "state=valid&error=access_denied&error_description=untrusted",
                "valid"
            )
            .unwrap()
            .outcome,
            Outcome::Error("access_denied".into())
        );
    }

    #[tokio::test]
    async fn browser_callback_rejects_forgery_then_delivers_code_once() {
        let (url, mut pending, task) = listener().await;
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
        let request = format!("{url}?state=expected-state&code=valid%2Bcode");
        let (first, replay) =
            tokio::join!(client.get(&request).send(), client.get(&request).send());
        let responses = [first, replay];
        assert_eq!(
            responses
                .iter()
                .filter(|r| r.as_ref().is_ok_and(|r| r.status() == StatusCode::OK))
                .count(),
            1
        );
        let response = responses
            .into_iter()
            .filter_map(Result::ok)
            .find(|r| r.status() == StatusCode::OK)
            .unwrap();
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.text().await.unwrap().contains("valid+code"));
        let callback = pending.wait().await.unwrap();
        assert_eq!(callback.state, "expected-state");
        assert_eq!(callback.outcome, Outcome::Code("valid+code".into()));
        assert_eq!(pending.wait().await, Err(WaitError::Stopped));
        task.await.unwrap();
        assert!(client.get(url).send().await.is_err());
    }

    #[tokio::test]
    async fn denial_expiry_and_dropped_receivers_close_the_listener() {
        let (url, mut pending, task) = listener().await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            client
                .get(format!("{url}?state=expected-state&error=access_denied"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            pending.wait().await.unwrap().outcome,
            Outcome::Error("access_denied".into())
        );
        task.await.unwrap();
        let (url, pending, task) = listener().await;
        drop(pending);
        task.await.unwrap();
        assert!(client.get(url).send().await.is_err());
        let (listener, mut pending) = Listener::bind().await.unwrap();
        let uri = Url::parse(&format!("http://127.0.0.1:{}/callback", listener.port())).unwrap();
        let task = listener.start(&uri, "state".into(), Duration::ZERO);
        assert_eq!(pending.wait().await, Err(WaitError::Expired));
        task.await.unwrap();
    }
}
