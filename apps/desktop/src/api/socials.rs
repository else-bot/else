use std::sync::Arc;

use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::{Client, ClientError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    Telegram,
    Discord,
}

impl Provider {
    pub fn label(self) -> &'static str {
        match self {
            Self::Telegram => "Telegram",
            Self::Discord => "Discord",
        }
    }

    pub fn token_help(self) -> &'static str {
        match self {
            Self::Telegram => {
                "Create a bot with @BotFather using /newbot. This token lets the API act as that bot in chats it joins. It does not connect your personal Telegram inbox."
            }
            Self::Discord => {
                "Create an application in the Discord Developer Portal and copy its bot token. This token lets the API act as that bot, within its server and channel permissions. It does not connect your personal account."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SocialSetup {
    pub id: Uuid,
    pub provider: Provider,
    pub reason: String,
    pub status: String,
    pub connection_id: Option<Uuid>,
}

impl SocialSetup {
    pub fn is_pending(&self) -> bool {
        self.status == "pending"
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Connection {
    pub id: Uuid,
    pub provider: Provider,
    pub remote_id: String,
    pub display_name: String,
    pub username: Option<String>,
    pub state: String,
    pub health: String,
    pub generation: i64,
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Destination {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub can_send: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub send: bool,
    pub wake: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakePolicy {
    #[default]
    Never,
    Mentions,
    All,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingInput {
    pub destinations: Vec<String>,
    pub permissions: Permissions,
    pub wake_policy: WakePolicy,
    pub instructions: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Binding {
    pub bot_id: Uuid,
    #[serde(flatten)]
    pub access: BindingInput,
}

#[derive(Deserialize)]
pub struct ConnectionDetail {
    pub connection: Connection,
    pub bindings: Vec<Binding>,
}

#[derive(Deserialize)]
struct ConnectionEnvelope {
    connection: Connection,
}

impl Client {
    pub async fn social_connections(self: Arc<Self>) -> Result<Vec<Connection>, ClientError> {
        #[derive(Deserialize)]
        struct Response {
            connections: Vec<Connection>,
        }
        self.call::<Response>(
            Method::GET,
            "/v1/connectors/socials/connections".into(),
            None,
        )
        .await
        .map(|(_, r)| r.connections)
    }

    pub async fn social_connection(
        self: Arc<Self>,
        id: Uuid,
    ) -> Result<ConnectionDetail, ClientError> {
        self.call(
            Method::GET,
            format!("/v1/connectors/socials/connections/{id}"),
            None,
        )
        .await
        .map(|(_, r)| r)
    }

    pub async fn connect_social(
        self: Arc<Self>,
        provider: Provider,
        token: String,
    ) -> Result<Connection, ClientError> {
        self.call::<ConnectionEnvelope>(
            Method::POST,
            "/v1/connectors/socials/connections".into(),
            Some(json!({"provider": provider, "token": token})),
        )
        .await
        .map(|(_, r)| r.connection)
    }

    pub async fn social_destinations(
        self: Arc<Self>,
        id: Uuid,
    ) -> Result<Vec<Destination>, ClientError> {
        #[derive(Deserialize)]
        struct Response {
            destinations: Vec<Destination>,
        }
        self.call::<Response>(
            Method::GET,
            format!("/v1/connectors/socials/connections/{id}/destinations"),
            None,
        )
        .await
        .map(|(_, r)| r.destinations)
    }

    pub async fn bind_social(
        self: Arc<Self>,
        id: Uuid,
        bot: Uuid,
        access: BindingInput,
    ) -> Result<Binding, ClientError> {
        #[derive(Deserialize)]
        struct Response {
            binding: Binding,
        }
        self.call::<Response>(
            Method::PUT,
            format!("/v1/connectors/socials/connections/{id}/bots/{bot}"),
            Some(json!(access)),
        )
        .await
        .map(|(_, response)| response.binding)
    }

    pub async fn set_social_active(
        self: Arc<Self>,
        id: Uuid,
        active: bool,
    ) -> Result<Connection, ClientError> {
        self.call::<ConnectionEnvelope>(
            Method::PATCH,
            format!("/v1/connectors/socials/connections/{id}"),
            Some(json!({"active": active})),
        )
        .await
        .map(|(_, r)| r.connection)
    }

    pub async fn replace_social_token(
        self: Arc<Self>,
        id: Uuid,
        token: String,
    ) -> Result<(), ClientError> {
        self.call::<serde_json::Value>(
            Method::PUT,
            format!("/v1/connectors/socials/connections/{id}/credentials"),
            Some(json!({"token": token})),
        )
        .await
        .map(|_| ())
    }

    pub async fn disconnect_social(self: Arc<Self>, id: Uuid) -> Result<(), ClientError> {
        self.call::<serde_json::Value>(
            Method::DELETE,
            format!("/v1/connectors/socials/connections/{id}"),
            None,
        )
        .await
        .map(|_| ())
    }

    pub async fn complete_social_setup(
        self: Arc<Self>,
        id: Uuid,
        connection: Uuid,
    ) -> Result<(), ClientError> {
        self.call::<serde_json::Value>(
            Method::POST,
            format!("/v1/connectors/socials/setups/{id}/complete"),
            Some(json!({"connection_id": connection})),
        )
        .await
        .map(|_| ())
    }

    pub async fn cancel_social_setup(self: Arc<Self>, id: Uuid) -> Result<(), ClientError> {
        self.call::<serde_json::Value>(
            Method::DELETE,
            format!("/v1/connectors/socials/setups/{id}"),
            None,
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OwnerToken;

    fn response(value: serde_json::Value) -> &'static str {
        let body = value.to_string();
        Box::leak(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_boxed_str())
    }

    #[tokio::test]
    async fn bot_tokens_use_only_the_owner_connection_write_endpoint() {
        for provider in [Provider::Telegram, Provider::Discord] {
            let id = Uuid::new_v4();
            let (origin, request) = crate::api::tests::serve_once(response(json!({"connection": {
                "id": id, "provider": provider, "remote_id": "9007199254740993",
                "display_name": "Team assistant", "username": "team_bot", "state": "active",
                "health": "connected", "generation": 1, "instructions": ""
            }})))
            .await;
            let owner = OwnerToken::parse(&"T".repeat(43)).unwrap();
            let client =
                Arc::new(Client::new(origin, &owner, tokio::runtime::Handle::current()).unwrap());
            let connection = client
                .connect_social(provider, "private-bot-credential".into())
                .await
                .unwrap();
            assert_eq!(connection.id, id);
            assert_eq!(connection.remote_id, "9007199254740993");
            let request = request.await.unwrap();
            let (head, body) = request.split_once("\r\n\r\n").unwrap();
            assert!(head.starts_with("POST /v1/connectors/socials/connections HTTP/1.1"));
            assert!(
                head.to_lowercase()
                    .contains(&format!("authorization: bearer {}", "t".repeat(43)))
            );
            assert!(!head.contains("private-bot-credential"));
            let body: serde_json::Value = serde_json::from_str(body).unwrap();
            assert_eq!(
                body,
                json!({"provider": provider, "token": "private-bot-credential"})
            );
            assert!(!format!("{connection:?}").contains("private-bot-credential"));
        }
    }

    #[tokio::test]
    async fn access_revocation_keeps_owner_instructions_and_clears_authority() {
        let connection = Uuid::new_v4();
        let bot = Uuid::new_v4();
        let access = BindingInput {
            destinations: Vec::new(),
            permissions: Permissions::default(),
            wake_policy: WakePolicy::Never,
            instructions: "Summarize mentions.\nAsk me before posting.".into(),
        };
        let (origin, request) = crate::api::tests::serve_once(response(json!({"binding": {
            "id": Uuid::new_v4(), "connection_id": connection, "bot_id": bot,
            "destinations": [], "permissions": access.permissions, "wake_policy": "never", "instructions": access.instructions
        }}))).await;
        let owner = OwnerToken::parse(&"T".repeat(43)).unwrap();
        let client =
            Arc::new(Client::new(origin, &owner, tokio::runtime::Handle::current()).unwrap());
        let binding = client
            .bind_social(connection, bot, access.clone())
            .await
            .unwrap();
        assert_eq!(binding.bot_id, bot);
        assert_eq!(binding.access.instructions, access.instructions);
        let request = request.await.unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with(&format!(
            "PUT /v1/connectors/socials/connections/{connection}/bots/{bot} HTTP/1.1"
        )));
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["instructions"], access.instructions);
        assert_eq!(body["destinations"], json!([]));
        assert_eq!(
            body["permissions"],
            json!({"read": false, "write": false, "send": false, "wake": false})
        );
        assert_eq!(body["wake_policy"], "never");
        assert!(body.get("token").is_none());
    }
}
