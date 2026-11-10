use std::sync::Arc;

use chrono::{DateTime, Utc};
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

impl Connection {
    /// Another webhook, app, or server takes this bot's messages, so none
    /// reach the connection.
    pub fn receiver_conflict(&self) -> bool {
        self.health == "receiver_conflict"
    }

    pub fn health_label(&self) -> &str {
        if self.receiver_conflict() {
            "messages go elsewhere"
        } else {
            &self.health
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakePolicy {
    Mentions,
    All,
}

impl WakePolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mentions => "on mention",
            Self::All => "every message",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Trigger {
    pub id: Uuid,
    pub connection_id: Uuid,
    pub bot_id: Uuid,
    pub provider: Provider,
    pub connection_name: String,
    pub username: Option<String>,
    pub destination: Option<String>,
    pub destination_name: Option<String>,
    pub wake: WakePolicy,
    pub instructions: String,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub last_woke_at: Option<DateTime<Utc>>,
}

impl Trigger {
    pub fn title(&self) -> &str {
        self.destination_name
            .as_deref()
            .or(self.destination.as_deref())
            .unwrap_or("Any chat")
    }
}

#[derive(Deserialize)]
pub struct ConnectionDetail {
    pub connection: Connection,
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

    pub async fn set_social_trigger_enabled(
        self: Arc<Self>,
        bot: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Trigger, ClientError> {
        #[derive(Deserialize)]
        struct Response {
            trigger: Trigger,
        }
        self.call::<Response>(
            Method::PATCH,
            format!("/v1/bots/{bot}/social-triggers/{id}"),
            Some(json!({"enabled": enabled})),
        )
        .await
        .map(|(_, response)| response.trigger)
    }

    pub async fn delete_social_trigger(
        self: Arc<Self>,
        bot: Uuid,
        id: Uuid,
    ) -> Result<(), ClientError> {
        self.call::<serde_json::Value>(
            Method::DELETE,
            format!("/v1/bots/{bot}/social-triggers/{id}"),
            None,
        )
        .await
        .map(|_| ())
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
    async fn completing_setup_sends_only_the_connection_id() {
        let setup = Uuid::new_v4();
        let connection = Uuid::new_v4();
        let (origin, request) = crate::api::tests::serve_once(response(json!({"setup": {
            "id": setup, "provider": "telegram", "reason": "Watch messages",
            "status": "connected", "connection_id": connection
        }})))
        .await;
        let owner = OwnerToken::parse(&"T".repeat(43)).unwrap();
        let client =
            Arc::new(Client::new(origin, &owner, tokio::runtime::Handle::current()).unwrap());
        client
            .complete_social_setup(setup, connection)
            .await
            .unwrap();
        let request = request.await.unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with(&format!(
            "POST /v1/connectors/socials/setups/{setup}/complete HTTP/1.1"
        )));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            json!({"connection_id": connection})
        );
    }

    #[tokio::test]
    async fn trigger_changes_use_the_bot_scoped_routes() {
        let bot = Uuid::new_v4();
        let id = Uuid::new_v4();
        let (origin, request) = crate::api::tests::serve_once(response(json!({"trigger": {
            "id": id, "bot_id": bot, "connection_id": Uuid::new_v4(), "provider": "discord",
            "connection_name": "Team bot", "username": null, "destination": null,
            "destination_name": null, "wake": "all", "instructions": "Summarize questions.",
            "enabled": false, "created_at": "2026-11-01T07:00:00.123+00:00", "updated_at": "2026-11-02T07:00:00.456+00:00"
        }}))).await;
        let owner = OwnerToken::parse(&"T".repeat(43)).unwrap();
        let client =
            Arc::new(Client::new(origin, &owner, tokio::runtime::Handle::current()).unwrap());
        let trigger = client
            .set_social_trigger_enabled(bot, id, false)
            .await
            .unwrap();
        assert_eq!(trigger.id, id);
        assert!(!trigger.enabled);
        let request = request.await.unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with(&format!(
            "PATCH /v1/bots/{bot}/social-triggers/{id} HTTP/1.1"
        )));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            json!({"enabled": false})
        );

        let (origin, request) =
            crate::api::tests::serve_once("HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await;
        let client =
            Arc::new(Client::new(origin, &owner, tokio::runtime::Handle::current()).unwrap());
        client.delete_social_trigger(bot, id).await.unwrap();
        assert!(request.await.unwrap().starts_with(&format!(
            "DELETE /v1/bots/{bot}/social-triggers/{id} HTTP/1.1"
        )));
    }
}
