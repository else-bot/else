use crate::common;

use async_trait::async_trait;
use common::TestDb;
use else_api::db::turns::{ItemKind, TurnStatus};
use else_api::{
    credentials::SecretKey,
    socials::{
        Manager,
        providers::{Session, Transport},
        store,
        types::*,
    },
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Observed {
    operation: &'static str,
    client: Arc<twilight_http::Client>,
}
struct ObserveSessions(mpsc::Sender<Observed>);
impl ObserveSessions {
    async fn observe(&self, operation: &'static str, session: &Session) {
        let Session::Discord { http, .. } = session else {
            panic!("Discord connection must use a Discord SDK session");
        };
        self.0
            .send(Observed {
                operation,
                client: http.clone(),
            })
            .await
            .unwrap();
    }
}
#[async_trait]
impl Transport for ObserveSessions {
    async fn identity(&self, _: Provider, _: &Token) -> Result<Identity, Error> {
        unreachable!("Fixture creates an already validated connection")
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
    async fn destinations(&self, session: &Session) -> Result<Vec<Destination>, Error> {
        self.observe("discover", session).await;
        Ok(vec![Destination {
            id: "100".into(),
            name: "Team".into(),
            kind: "channel".into(),
            can_send: true,
        }])
    }
    async fn resolve(
        &self,
        _: &Connection,
        session: &Session,
        _: &str,
    ) -> Result<Destination, Error> {
        self.observe("resolve", session).await;
        Ok(Destination {
            id: "100".into(),
            name: "Team".into(),
            kind: "channel".into(),
            can_send: true,
        })
    }
    async fn execute(&self, session: &Session, _: &Command) -> Result<Value, Error> {
        self.observe("execute", session).await;
        Ok(json!({"ok": true}))
    }
    async fn send(
        &self,
        session: &Session,
        _: &str,
        _: &str,
        _: Option<&str>,
        _: &[SendFile],
    ) -> Result<Value, Error> {
        self.observe("send", session).await;
        Ok(json!({"id": "sent"}))
    }
    async fn listen(
        &self,
        _: &Connection,
        session: &Session,
        _: i64,
        _: mpsc::Sender<Envelope>,
        stop: CancellationToken,
    ) -> Result<(), Error> {
        self.observe("listen", session).await;
        stop.cancelled().await;
        Ok(())
    }
}
async fn observed(
    rx: &mut mpsc::Receiver<Observed>,
    operation: &str,
) -> Arc<twilight_http::Client> {
    common::until(operation, async {
        loop {
            let next = rx.recv().await.unwrap();
            if next.operation == operation {
                return next.client;
            }
            assert_eq!(next.operation, "listen");
        }
    })
    .await
}

#[tokio::test]
async fn discovery_commands_and_listener_share_sdk_until_credential_rotation() {
    let db = TestDb::migrated().await;
    let key = Arc::new(SecretKey::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap());
    let token = Token::parse("session-lifecycle-test-token".into()).unwrap();
    let c = store::create(
        &db.pool,
        &key,
        Provider::Discord,
        &Identity {
            id: "sdk-session".into(),
            display_name: "SDK session".into(),
            username: None,
        },
        &Secret::Token(token),
        None,
        "",
    )
    .await
    .unwrap();
    let bot = common::seed::bot(&db.pool, "Session test").await.id;
    let turn = common::seed::turn(bot)
        .status(TurnStatus::Running)
        .attempt(1)
        .message(ItemKind::User, "test")
        .insert(&db.pool)
        .await;
    store::save_destinations(
        &db.pool,
        c.id,
        &[Destination {
            id: "100".into(),
            name: "Team".into(),
            kind: "channel".into(),
            can_send: true,
        }],
    )
    .await
    .unwrap();
    let (tx, mut rx) = mpsc::channel(16);
    let manager = Arc::new(Manager::new(
        db.pool.clone(),
        key.clone(),
        db.scheduler(),
        Arc::new(ObserveSessions(tx)),
    ));
    let stop = CancellationToken::new();
    let running = tokio::spawn(manager.clone().run(stop.clone()));

    manager.discover(c.id).await.unwrap();
    let first = common::until("discovery and listener sessions", async {
        let mut discovery = None;
        let mut listener = None;
        loop {
            let next = rx.recv().await.unwrap();
            match next.operation {
                "discover" => discovery = Some(next.client),
                "listen" => listener = Some(next.client),
                other => panic!("unexpected initial operation: {other}"),
            }
            if let (Some(discovery), Some(listener)) = (&discovery, &listener) {
                assert!(Arc::ptr_eq(discovery, listener));
                return discovery.clone();
            }
        }
    })
    .await;
    manager
        .execute(
            c.id,
            bot,
            turn,
            None,
            Command::Chat {
                destination: "100".into(),
            },
        )
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first, &observed(&mut rx, "execute").await));
    manager
        .send(
            c.id,
            bot,
            turn,
            Uuid::new_v4(),
            SendMessage {
                destination: "100".into(),
                body: "approved message".into(),
                reply_to: None,
                files: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first, &observed(&mut rx, "send").await));
    manager.discover(c.id).await.unwrap();
    assert!(Arc::ptr_eq(&first, &observed(&mut rx, "discover").await));

    let current = store::connection(&db.pool, c.id).await.unwrap();
    let replacement = Token::parse("rotated-session-test-token".into()).unwrap();
    store::replace(&db.pool, &key, &current, &Secret::Token(replacement))
        .await
        .unwrap();
    manager.changed(c.id).await;
    manager.discover(c.id).await.unwrap();
    let rotated = observed(&mut rx, "discover").await;
    assert!(!Arc::ptr_eq(&first, &rotated));
    let retired = Arc::downgrade(&first);
    drop(first);
    assert!(
        retired.upgrade().is_none(),
        "old credential session must be released before replacement discovery"
    );
    stop.cancel();
    common::until("the session manager to stop", running)
        .await
        .unwrap();
}
