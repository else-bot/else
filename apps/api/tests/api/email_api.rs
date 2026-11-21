use crate::common;
use else_api::db::turns::TurnStatus;

use axum::http::StatusCode;
use common::{
    TestApp,
    recorder::{self, GATE, GATE_EVERY_ATTEMPT, tool},
};
use else_api::{
    connectors::{ConnectorManager, store, types::*},
    grants::Tool,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod app_passwords;
mod inbox_cards;
mod introduction;
mod mail_events;
mod oauth_sign_in_again;
mod reader;
mod removal;
mod runtime;
mod sign_in;
mod triggers;

fn config() -> MailConfig {
    MailConfig {
        provider: Provider::Imap,
        address: "bot@example.com".into(),
        username: "bot@example.com".into(),
        imap: Endpoint {
            host: "mail.example.com".into(),
            port: 993,
            start_tls: false,
        },
        smtp: Endpoint {
            host: "mail.example.com".into(),
            port: 465,
            start_tls: false,
        },
        archive_folder: "Archive".into(),
        trash_folder: "Trash".into(),
        drafts_folder: "Drafts".into(),
        sent_folder: "Sent".into(),
    }
}
async fn account(app: &TestApp) -> Account {
    store::create(
        &app.db.pool,
        &app.state.secret_key,
        &config(),
        &Credential::Password {
            password: "private-app-password".into(),
        },
    )
    .await
    .unwrap()
}
/// Waits until the account's runtime reports `want`.
async fn health_becomes(app: &TestApp, account: Uuid, want: Health) {
    tokio::time::timeout(common::SETTLE, async {
        while store::account(&app.db.pool, account)
            .await
            .unwrap()
            .unwrap()
            .health
            != want
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the inbox never became {want:?}"));
}

fn permissions() -> Permissions {
    Permissions {
        read: true,
        write: true,
        send: true,
        wake: true,
    }
}

struct Transport {
    sends: AtomicUsize,
    uncertain: bool,
    /// Marks dispatching actions uncertain while a command is in flight, as
    /// startup recovery or a restore does.
    recovers: Option<sqlx::SqlitePool>,
}

/// A password QuietInbox's server rejects, as Apple rejects a revoked
/// app-specific password.
const REVOKED: &str = "revoked-app-password";

#[derive(Default)]
struct QuietInbox {
    scans: AtomicUsize,
    waits: AtomicUsize,
    ready: tokio::sync::Notify,
    /// Rejects every new sign-in, as after the owner revokes the password.
    /// A watch session that already signed in keeps waiting.
    revoked: std::sync::atomic::AtomicBool,
    /// Holds the next scan after it signs in, until `release_scan` or the
    /// watch stops.
    hold_scan: std::sync::atomic::AtomicBool,
    scan_held: tokio::sync::Notify,
    release_scan: tokio::sync::Notify,
}
impl QuietInbox {
    fn signs_in(&self, auth: &Auth) -> Result<(), MailError> {
        if auth.secret.as_str() == REVOKED || self.revoked.load(Ordering::SeqCst) {
            Err(MailError::new(MailErrorCode::MailAuthenticationFailed))
        } else {
            Ok(())
        }
    }
}
#[async_trait::async_trait]
impl MailTransport for QuietInbox {
    async fn verify(&self, _: &MailConfig, auth: &Auth) -> Result<(), MailError> {
        self.signs_in(auth)
    }
    async fn execute(
        &self,
        _: &MailConfig,
        auth: &Auth,
        _: &MailCommand,
    ) -> Result<Value, MailError> {
        self.signs_in(auth)?;
        Ok(json!({"folders":[]}))
    }
    async fn fetch(&self, _: &MailConfig, _: &Auth, _: &MessageRef) -> Result<Fetched, MailError> {
        Err(MailError::new(MailErrorCode::MessageNotFound))
    }
    async fn reader(&self, _: &MailConfig, _: &Auth) -> Result<Box<dyn MailReader>, MailError> {
        Err(MailError::new(MailErrorCode::MailTransportFailed))
    }
    async fn scan(
        &self,
        _: &MailConfig,
        auth: &Auth,
        _: Option<&Cursor>,
    ) -> Result<Scan, MailError> {
        self.signs_in(auth)?;
        if self.hold_scan.swap(false, Ordering::SeqCst) {
            self.scan_held.notify_one();
            self.release_scan.notified().await;
        }
        self.scans.fetch_add(1, Ordering::SeqCst);
        Ok(Scan {
            cursor: Cursor {
                uid_validity: 1,
                last_uid: 0,
            },
            arrivals: vec![],
            more: false,
        })
    }
    async fn wait(
        &self,
        _: &MailConfig,
        _: &Auth,
        _: &Cursor,
        stop: CancellationToken,
    ) -> Result<(), MailError> {
        self.waits.fetch_add(1, Ordering::SeqCst);
        self.ready.notify_one();
        stop.cancelled().await;
        Ok(())
    }
}

#[async_trait::async_trait]
impl MailTransport for Transport {
    async fn verify(&self, _: &MailConfig, _: &Auth) -> Result<(), MailError> {
        Ok(())
    }
    async fn execute(&self, _: &MailConfig, _: &Auth, _: &MailCommand) -> Result<Value, MailError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        if let Some(pool) = &self.recovers {
            store::recover(pool).await.unwrap();
        }
        if self.uncertain {
            Err(MailError::new(MailErrorCode::MailTransportFailed))
        } else {
            Ok(json!({"sent":true}))
        }
    }
    async fn fetch(&self, _: &MailConfig, _: &Auth, _: &MessageRef) -> Result<Fetched, MailError> {
        Err(MailError::new(MailErrorCode::MessageNotFound))
    }
    async fn reader(&self, _: &MailConfig, _: &Auth) -> Result<Box<dyn MailReader>, MailError> {
        Err(MailError::new(MailErrorCode::MailTransportFailed))
    }
    async fn scan(&self, _: &MailConfig, _: &Auth, _: Option<&Cursor>) -> Result<Scan, MailError> {
        Ok(Scan {
            cursor: Cursor {
                uid_validity: 1,
                last_uid: 0,
            },
            arrivals: vec![],
            more: false,
        })
    }
    async fn wait(
        &self,
        _: &MailConfig,
        _: &Auth,
        _: &Cursor,
        stop: CancellationToken,
    ) -> Result<(), MailError> {
        stop.cancelled().await;
        Ok(())
    }
}

/// A mail-triggered turn for one arrival in `folder` of `account`, whose
/// trigger tells the bot `instructions`.
async fn mail_event(
    h: &recorder::Harness,
    account: &Account,
    folder: &str,
    instructions: &str,
) -> Uuid {
    let initial = Scan {
        cursor: Cursor {
            uid_validity: 1,
            last_uid: 8,
        },
        arrivals: vec![],
        more: false,
    };
    assert!(
        store::record_scan(
            &h.app.db.pool,
            account.id,
            account.generation,
            None,
            &initial,
        )
        .await
        .unwrap(),
        "no watch recorded a scan first"
    );
    // Before the arrival: the scheduler may admit it on its next scan.
    sqlx::query("UPDATE connector_grants SET wake_instructions = ?1")
        .bind(instructions)
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    let arrival = Scan {
        cursor: Cursor {
            uid_validity: 1,
            last_uid: 9,
        },
        arrivals: vec![Arrival {
            message: MessageRef {
                folder: folder.into(),
                uid_validity: 1,
                uid: 9,
            },
            envelope: Envelope {
                message_id: Some("fixture@coupons.test".into()),
                subject: Some("Verify your email".into()),
                sender: Some("Coupon Co".into()),
            },
        }],
        more: false,
    };
    assert!(
        store::record_scan(
            &h.app.db.pool,
            account.id,
            account.generation,
            Some(&initial.cursor),
            &arrival,
        )
        .await
        .unwrap()
    );
    else_api::connector_core::inbound::admit_pending::<else_api::connectors::delivery::Mail>(
        &h.app.db.pool,
        &h.app.state.registry,
        32,
    )
    .await
    .unwrap();
    h.app.scheduler.wake();
    sqlx::query_scalar("SELECT turn_id FROM connector_deliveries WHERE turn_id IS NOT NULL LIMIT 1")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap()
}

fn gmail(address: &str) -> (MailConfig, Credential) {
    (
        Provider::Gmail.config(address.into()).unwrap(),
        Credential::OAuth2 {
            client_id: "desktop".into(),
            client_secret: None,
            refresh_token: "private-refresh".into(),
            access_token: None,
            expires_at: None,
        },
    )
}
