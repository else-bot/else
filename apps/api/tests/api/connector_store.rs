use crate::common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestDb, seed};
use else_api::connector_core::actions::{Outcome, Start};
use else_api::connector_core::inbound;
use else_api::connectors::delivery;
use else_api::connectors::store::{self, StoreError};
use else_api::connectors::types::{
    AccountState, Arrival, Credential, Cursor, Draft, Endpoint, MailCommand, MailConfig,
    MessageRef, Permissions, Provider, Scan,
};
use else_api::credentials::SecretKey;
use else_api::db::turns::TurnStatus;
use else_api::providers::Registry;
use else_api::providers::synthetic::Synthetic;
use serde_json::json;
use uuid::Uuid;

mod access;
mod actions;
mod credentials;
mod mail;

const PASSWORD: &str = "correct horse battery staple";

fn key() -> SecretKey {
    SecretKey::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap()
}

fn registry() -> Registry {
    Registry::new(vec![Arc::new(Synthetic::new(Duration::from_millis(5)))])
}

fn config(address: &str) -> MailConfig {
    let endpoint = |host: &str, port| Endpoint {
        host: host.into(),
        port,
        start_tls: false,
    };
    MailConfig {
        provider: Provider::Imap,
        address: address.into(),
        username: address.into(),
        imap: endpoint("imap.example.test", 993),
        smtp: endpoint("smtp.example.test", 465),
        archive_folder: "Archive".into(),
        trash_folder: "Trash".into(),
        drafts_folder: "Drafts".into(),
        sent_folder: "Sent".into(),
    }
}

fn password(text: &str) -> Credential {
    Credential::Password {
        password: text.into(),
    }
}

fn all() -> Permissions {
    Permissions {
        read: true,
        write: true,
        send: true,
        wake: true,
    }
}

fn read_only() -> Permissions {
    Permissions {
        read: true,
        ..Permissions::default()
    }
}

fn message(uid_validity: u32, uid: u32) -> MessageRef {
    MessageRef {
        folder: "INBOX".into(),
        uid_validity,
        uid,
    }
}

fn scan(uid_validity: u32, last_uid: u32, uids: &[u32]) -> Scan {
    Scan {
        cursor: Cursor {
            uid_validity,
            last_uid,
        },
        arrivals: uids
            .iter()
            .map(|&uid| Arrival {
                message: message(uid_validity, uid),
                envelope: Default::default(),
            })
            .collect(),
        more: false,
    }
}

async fn deliveries(db: &TestDb, bot: Uuid) -> Vec<(i64, String)> {
    sqlx::query_as(
        "SELECT e.uid, d.status FROM connector_deliveries d JOIN connector_events e ON e.id = d.event_id
         WHERE d.bot_id = ?1 ORDER BY e.uid, d.created_at",
    )
    .bind(bot)
    .fetch_all(&db.pool)
    .await
    .unwrap()
}

async fn connector_turns(db: &TestDb, bot: Uuid) -> Vec<(Uuid, String)> {
    sqlx::query_as(
        "SELECT t.id, i.text FROM turns t JOIN items i ON i.id = t.owner_item_id
         WHERE t.bot_id = ?1 AND i.kind = 'connector' ORDER BY t.turn_no",
    )
    .bind(bot)
    .fetch_all(&db.pool)
    .await
    .unwrap()
}

async fn watched(db: &TestDb, address: &str) -> (Uuid, i64) {
    let account = store::create(&db.pool, &key(), &config(address), &password(PASSWORD))
        .await
        .unwrap();
    assert!(
        store::record_scan(&db.pool, account.id, 1, None, &scan(7, 10, &[]))
            .await
            .unwrap()
    );
    (account.id, account.generation)
}
