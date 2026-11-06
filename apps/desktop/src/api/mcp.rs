use super::{Client, ClientError};
use reqwest::Method;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone, Deserialize)]
pub struct InstalledMcp {
    pub id: String,
    pub name: String,
    pub description: String,
    pub installed_by: String,
    pub installed_at: String,
}

impl Client {
    pub async fn installed_mcps(self: Arc<Self>) -> Result<Vec<InstalledMcp>, ClientError> {
        #[derive(Deserialize)]
        struct Library {
            mcps: Vec<InstalledMcp>,
        }
        self.call::<Library>(Method::GET, "/v1/settings/mcp".into(), None)
            .await
            .map(|(_, library)| library.mcps)
    }

    pub async fn remove_mcp(self: Arc<Self>, id: String) -> Result<(), ClientError> {
        self.call::<()>(Method::DELETE, format!("/v1/settings/mcp/{id}"), None)
            .await
            .map(|_| ())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct McpConsent {
    pub id: String,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

#[derive(Deserialize)]
pub struct McpAuthorization {
    pub request_id: uuid::Uuid,
    pub authorization_url: String,
    pub expires_in: u64,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

impl Client {
    pub async fn authorize_mcp(
        self: Arc<Self>,
        bot: uuid::Uuid,
        id: String,
        redirect_uri: String,
    ) -> Result<McpAuthorization, ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this
                .request(
                    Method::POST,
                    &format!("/v1/bots/{bot}/mcp/{id}/oauth/start"),
                )
                .timeout(std::time::Duration::from_secs(90))
                .json(&serde_json::json!({"redirect_uri":redirect_uri}));
            this.send_json::<McpAuthorization>(request)
                .await
                .map(|(_, body)| body)
        })
        .await
    }

    pub async fn complete_mcp(
        self: Arc<Self>,
        bot: uuid::Uuid,
        id: String,
        callback: serde_json::Value,
    ) -> Result<(), ClientError> {
        let this = self.clone();
        self.run(async move {
            let request = this
                .request(
                    Method::POST,
                    &format!("/v1/bots/{bot}/mcp/{id}/oauth/complete"),
                )
                .timeout(std::time::Duration::from_secs(60))
                .json(&callback);
            this.send_json::<serde_json::Value>(request)
                .await
                .map(|_| ())
        })
        .await
    }

    pub fn cancel_mcp(self: Arc<Self>, bot: uuid::Uuid, id: String, request: uuid::Uuid) {
        self.clone().runtime.spawn(async move {
            let _ = self
                .call::<()>(
                    Method::DELETE,
                    format!("/v1/bots/{bot}/mcp/{id}/oauth/{request}"),
                    None,
                )
                .await;
        });
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpState {
    Disconnected,
    Authorizing,
    Connected,
    ReauthRequired,
}

#[derive(Deserialize)]
pub struct McpConnectionStatus {
    pub status: McpState,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

impl Client {
    pub async fn mcp_connection(
        self: Arc<Self>,
        bot: uuid::Uuid,
        id: String,
    ) -> Result<McpConnectionStatus, ClientError> {
        self.call(
            Method::GET,
            format!("/v1/bots/{bot}/mcp/{id}/connection"),
            None,
        )
        .await
        .map(|(_, body)| body)
    }

    pub async fn cancel_pending_mcp(
        self: Arc<Self>,
        bot: uuid::Uuid,
        id: String,
    ) -> Result<McpConnectionStatus, ClientError> {
        self.call(
            Method::DELETE,
            format!("/v1/bots/{bot}/mcp/{id}/oauth"),
            None,
        )
        .await
        .map(|(_, body)| body)
    }
}
