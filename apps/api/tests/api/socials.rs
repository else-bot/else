use crate::common;

use async_trait::async_trait;
use common::recorder::{Called, GATE, GATE_AFTER_OPENING, GATE_EVERY_ATTEMPT, harness, tool};
use common::{TestApp, TestDb};
use else_api::app::outgoing::send::{Connected, send_connected};
use else_api::app::outgoing::{Channels, Platform};
use else_api::connector_core::actions::Start;
use else_api::connectors::types::{MailCommand, MailError};
use else_api::db::turns::{ItemKind, TurnStatus};
use else_api::grants::Tool;
use else_api::{
    credentials::SecretKey,
    socials::{
        Manager,
        providers::{Session, Transport},
        store,
        types::*,
    },
};
use else_wire::socials::Connection as WireConnection;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
mod connections;
mod sends;
mod setup;
mod state;

pub(crate) fn key() -> SecretKey {
    SecretKey::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap()
}
fn token() -> Token {
    Token::parse("private-native-social-test-token".into()).unwrap()
}
async fn account(db: &TestDb, name: &str) -> Connection {
    store::create(
        &db.pool,
        &key(),
        Provider::Telegram,
        &Identity {
            id: name.into(),
            display_name: name.into(),
            username: Some(name.into()),
        },
        &Secret::Token(token()),
        None,
        "Use this bot only for team support.",
    )
    .await
    .unwrap()
}
pub(crate) async fn bot(db: &TestDb) -> Uuid {
    common::seed::bot(&db.pool, "Social test").await.id
}
/// An owner turn the scheduler runs until the app stops.
pub(crate) async fn turn(db: &TestDb, bot: Uuid, kind: ItemKind) -> Uuid {
    insert_turn(db, bot, kind, TurnStatus::Running, "test").await
}

/// A turn whose owner item has `kind`, inserted with `status`.
async fn insert_turn(
    db: &TestDb,
    bot: Uuid,
    kind: ItemKind,
    status: TurnStatus,
    text: &str,
) -> Uuid {
    common::seed::turn(bot)
        .status(status)
        .attempt(1)
        .message(kind, text)
        .insert(&db.pool)
        .await
}
async fn destinations(db: &TestDb, c: Uuid) {
    store::save_destinations(
        &db.pool,
        c,
        &[
            Destination {
                id: "100".into(),
                name: "Team".into(),
                kind: "group".into(),
                can_send: true,
            },
            Destination {
                id: "200".into(),
                name: "Other".into(),
                kind: "group".into(),
                can_send: true,
            },
        ],
    )
    .await
    .unwrap()
}

/// A trigger as a bot would have set it for an owner request.
async fn trigger_row(
    db: &TestDb,
    c: Uuid,
    bot: Uuid,
    destination: Option<&str>,
    wake: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO social_triggers(id,connection_id,bot_id,destination,wake,instructions) VALUES(?1,?2,?3,?4,?5,'Tell me what the team asks.')")
        .bind(id)
        .bind(c)
        .bind(bot)
        .bind(destination)
        .bind(wake)
        .execute(&db.pool)
        .await
        .unwrap();
    id
}

struct Abandon<'a>(Option<&'a Notify>);
impl Drop for Abandon<'_> {
    fn drop(&mut self) {
        if let Some(abandoned) = self.0 {
            abandoned.notify_one();
        }
    }
}

#[derive(Default)]
struct Fake {
    started: Notify,
    release: Notify,
    /// Notified when a send future is dropped before it finishes.
    abandoned: Notify,
    blocked: AtomicBool,
    sends: AtomicUsize,
    sent_to: std::sync::Mutex<Vec<String>>,
    /// Each sent file as (name, media type, bytes).
    sent_files: std::sync::Mutex<Vec<(String, String, Vec<u8>)>>,
    refused: AtomicBool,
}
#[async_trait]
impl Transport for Fake {
    async fn identity(&self, _: Provider, _: &Token) -> Result<Identity, Error> {
        Ok(Identity {
            id: "fake".into(),
            display_name: "Fake bot".into(),
            username: None,
        })
    }
    async fn pair(
        &self,
        _: &sqlx::SqlitePool,
        _: else_api::socials::providers::whatsapp::Vault,
        _: &else_api::socials::pairing::Progress,
        _: CancellationToken,
    ) -> Result<Identity, Error> {
        unreachable!("Telegram and Discord fixtures never pair a device")
    }
    async fn destinations(&self, _: &Session) -> Result<Vec<Destination>, Error> {
        Ok(vec![])
    }
    async fn resolve(&self, _: &Connection, _: &Session, name: &str) -> Result<Destination, Error> {
        match name {
            "@team_alerts" => Ok(Destination {
                id: "-100777".into(),
                name: "Team alerts".into(),
                kind: "group".into(),
                can_send: true,
            }),
            _ => Err(Error::Invalid("no such public chat")),
        }
    }
    async fn execute(&self, _: &Session, _: &Command) -> Result<Value, Error> {
        Ok(json!({"ok":true}))
    }
    async fn send(
        &self,
        _: &Session,
        destination: &str,
        body: &str,
        _: Option<&str>,
        files: &[SendFile],
    ) -> Result<Value, Error> {
        self.started.notify_one();
        let mut abandon = Abandon(Some(&self.abandoned));
        if self.blocked.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        abandon.0 = None;
        self.sends.fetch_add(1, Ordering::SeqCst);
        self.sent_to.lock().unwrap().push(destination.to_owned());
        self.sent_files.lock().unwrap().extend(
            files
                .iter()
                .map(|f| (f.name.clone(), f.media_type.clone(), f.data.to_vec())),
        );
        Ok(json!({"id":"sent","body":body}))
    }
    async fn listen(
        &self,
        _: &Connection,
        _: &Session,
        _: i64,
        _: mpsc::Sender<Envelope>,
        stop: CancellationToken,
    ) -> Result<(), Error> {
        if self.refused.load(Ordering::SeqCst) {
            return Err(Error::ReceiverConflict);
        }
        stop.cancelled().await;
        Ok(())
    }
}

pub(crate) async fn http(
    router: &axum::Router,
    method: &str,
    path: &str,
    bearer: &str,
    body: Value,
) -> (axum::http::StatusCode, Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let request = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7879")
        .header("authorization", format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
fn social_result(called: Called) -> Value {
    assert_eq!(called.status, axum::http::StatusCode::OK, "{}", called.body);
    called.ok();
    called.result
}

fn incoming(event: &str, chat: &str, mentioned: bool, from_bot: bool) -> Incoming {
    Incoming {
        event_id: event.into(),
        destination: Destination {
            id: chat.into(),
            name: "Chat".into(),
            kind: "group".into(),
            can_send: true,
        },
        message_id: event.into(),
        author_id: "human".into(),
        body: "hello".into(),
        mentioned,
        direct: false,
        from_bot,
        replay: false,
    }
}
