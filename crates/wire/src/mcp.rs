use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledMcp {
    pub id: String,
    pub name: String,
    pub description: String,
    pub installed_by: String,
    pub installed_at: String,
    pub transport: Option<String>,
    /// The HTTP server's origin; absent for local commands and older servers.
    #[serde(default)]
    pub website: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    pub mcps: Vec<InstalledMcp>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthorization {
    pub request_id: Uuid,
    pub authorization_url: String,
    pub expires_in: u64,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}

wire_state! {
    pub enum McpState {
        Disconnected = "disconnected",
        Authorizing = "authorizing",
        Connected = "connected",
        ReauthRequired = "reauth_required",
    }
    _ => Unknown
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpConnectionStatus {
    pub status: McpState,
    pub provider: Option<String>,
    pub scopes: Vec<String>,
}
