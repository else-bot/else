use std::{sync::Arc, time::Duration};

use serde_json::json;
use url::Url;
use uuid::Uuid;

use crate::api::{Client, ClientError, McpAuthorization, McpConsent};
use crate::oauth_callback::{self, Callback, WaitError};

pub struct SignIn {
    pub authorization: McpAuthorization,
    callback: oauth_callback::Pending,
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
        expected: McpConsent,
    ) -> Result<Self, ClientError> {
        let this = client.clone();
        let id = expected.id.clone();
        client.run(async move {
            let (listener, callback) = oauth_callback::Listener::bind().await
                .map_err(|_| ClientError::Protocol("Cannot open the local sign-in callback.".into()))?;
            let redirect = format!("http://127.0.0.1:{}/mcp/{}", listener.port(), Uuid::new_v4().simple());
            let authorization = this.clone().authorize_mcp(bot, id.clone(), redirect.clone()).await?;
            let signin = Self { authorization, callback, client: this, bot, id };
            if signin.authorization.provider != expected.provider || signin.authorization.scopes != expected.scopes {
                return Err(ClientError::Protocol("The provider or requested permissions changed. Cancel this card and ask the bot to connect again.".into()));
            }
            let state = authorization_state(&signin.authorization.authorization_url, &redirect)
                .ok_or_else(|| ClientError::Protocol("The API returned an invalid MCP sign-in URL.".into()))?;
            listener.start(&Url::parse(&redirect).expect("local callback URL"), state, Duration::from_secs(signin.authorization.expires_in));
            Ok(signin)
        }).await
    }

    pub async fn wait(&mut self) -> Result<Callback, ClientError> {
        self.callback.wait().await.map_err(|error| {
            ClientError::Protocol(
                match error {
                    WaitError::Stopped => "Sign-in stopped. Try again.",
                    WaitError::Expired => "Sign-in expired. Try again.",
                }
                .into(),
            )
        })
    }

    pub async fn finish(&mut self, callback: Callback) -> Result<(), ClientError> {
        let mut callback = serde_json::to_value(callback)
            .map_err(|_| ClientError::Protocol("The sign-in callback is invalid.".into()))?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        http::{HeaderMap, StatusCode},
    };
    use serde_json::Value;
    use tokio::{net::TcpListener, sync::mpsc};

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
        let expected = McpConsent {
            id: "fixture".into(),
            provider: Some("https://issuer.test".into()),
            scopes: vec!["read".into()],
        };
        let mut signin = SignIn::start(client.clone(), bot, expected.clone())
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
        assert!(SignIn::start(client, bot, expected).await.is_err());
        tokio::time::timeout(Duration::from_secs(2), cancellations.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(completions.try_recv().is_err());
        server.abort();
    }
}
