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
