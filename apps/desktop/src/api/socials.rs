use else_wire::socials::{
    Connection, ConnectionEnvelope, Connections, PairingEnvelope, PairingView, Provider, Trigger,
    TriggerEnvelope,
};
use reqwest::Method;
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

use super::{Client, ClientError};

/// Where a device pairing links: a bot's setup card, or a connection the
/// account signed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingTarget {
    Setup(Uuid),
    Connection(Uuid),
}

impl PairingTarget {
    fn path(self) -> String {
        match self {
            Self::Setup(id) => format!("/v1/connectors/socials/setups/{id}/pairing"),
            Self::Connection(id) => format!("/v1/connectors/socials/connections/{id}/pairing"),
        }
    }
}

impl Client {
    pub async fn social_connections(self: Arc<Self>) -> Result<Vec<Connection>, ClientError> {
        self.call::<Connections>(
            Method::GET,
            "/v1/connectors/socials/connections".into(),
            None,
        )
        .await
        .map(|r| r.connections)
    }

    pub async fn social_connection(self: Arc<Self>, id: Uuid) -> Result<Connection, ClientError> {
        self.call::<ConnectionEnvelope>(
            Method::GET,
            format!("/v1/connectors/socials/connections/{id}"),
            None,
        )
        .await
        .map(|r| r.connection)
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
        .map(|r| r.connection)
    }

    pub async fn set_social_trigger_enabled(
        self: Arc<Self>,
        bot: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Trigger, ClientError> {
        self.call::<TriggerEnvelope>(
            Method::PATCH,
            format!("/v1/bots/{bot}/social-triggers/{id}"),
            Some(json!({"enabled": enabled})),
        )
        .await
        .map(|response| response.trigger)
    }

    pub async fn delete_social_trigger(
        self: Arc<Self>,
        bot: Uuid,
        id: Uuid,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::DELETE,
            format!("/v1/bots/{bot}/social-triggers/{id}"),
            None,
        )
        .await
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
        .map(|r| r.connection)
    }

    pub async fn replace_social_token(
        self: Arc<Self>,
        id: Uuid,
        token: String,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::PUT,
            format!("/v1/connectors/socials/connections/{id}/credentials"),
            Some(json!({"token": token})),
        )
        .await
    }

    pub async fn disconnect_social(self: Arc<Self>, id: Uuid) -> Result<(), ClientError> {
        self.call_unit(
            Method::DELETE,
            format!("/v1/connectors/socials/connections/{id}"),
            None,
        )
        .await
    }

    pub async fn complete_social_setup(
        self: Arc<Self>,
        id: Uuid,
        connection: Uuid,
        note: Option<String>,
    ) -> Result<(), ClientError> {
        self.call_unit(
            Method::POST,
            format!("/v1/connectors/socials/setups/{id}/complete"),
            Some(match note {
                Some(note) => json!({"connection_id": connection, "note": note}),
                None => json!({"connection_id": connection}),
            }),
        )
        .await
    }

    /// Start pairing a device, or rejoin the pairing already running.
    pub async fn start_social_pairing(
        self: Arc<Self>,
        target: PairingTarget,
    ) -> Result<PairingView, ClientError> {
        self.call::<PairingEnvelope>(Method::POST, target.path(), None)
            .await
            .map(|body| body.pairing)
    }

    /// The pairing once it changes past `after`, or as it is after the API's
    /// wait runs out.
    pub async fn social_pairing(
        self: Arc<Self>,
        target: PairingTarget,
        after: u64,
    ) -> Result<PairingView, ClientError> {
        self.call::<PairingEnvelope>(
            Method::GET,
            format!("{}?after={after}", target.path()),
            None,
        )
        .await
        .map(|body| body.pairing)
    }

    pub async fn stop_social_pairing(self: Arc<Self>, connection: Uuid) -> Result<(), ClientError> {
        self.call_unit(
            Method::DELETE,
            PairingTarget::Connection(connection).path(),
            None,
        )
        .await
    }

    pub async fn cancel_social_setup(self: Arc<Self>, id: Uuid) -> Result<(), ClientError> {
        self.call_unit(
            Method::DELETE,
            format!("/v1/connectors/socials/setups/{id}"),
            None,
        )
        .await
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
    async fn completing_setup_sends_the_connection_and_the_owners_words() {
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
            .complete_social_setup(
                setup,
                connection,
                Some("Only read the Family group.".into()),
            )
            .await
            .unwrap();
        let request = request.await.unwrap();
        let (head, body) = request.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with(&format!(
            "POST /v1/connectors/socials/setups/{setup}/complete HTTP/1.1"
        )));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            json!({"connection_id": connection, "note": "Only read the Family group."})
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
