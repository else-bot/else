use crate::common::{self, TestDb};
use crate::socials::{bot, key, turn};

use async_trait::async_trait;
use else_api::socials::{
    Manager,
    pairing::{Pairing, Progress, Purpose, View},
    providers::{Session, Transport, whatsapp::Vault},
    store,
    types::*,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const DEVICE_SECRET: &[u8] = b"linked-device-signal-identity";

/// A phone that shows one code, then links `number` once the test scans,
/// or reports that no code was scanned.
struct Phone {
    number: Mutex<String>,
    scanned: Notify,
    expire: std::sync::atomic::AtomicBool,
    listen: Mutex<Option<Error>>,
    /// Set once a stopped listener finished shutting its client down.
    closed: std::sync::atomic::AtomicBool,
}
impl Phone {
    fn new(number: &str) -> Arc<Self> {
        Arc::new(Self {
            number: Mutex::new(number.into()),
            scanned: Notify::new(),
            expire: Default::default(),
            listen: Mutex::new(None),
            closed: Default::default(),
        })
    }
}
#[async_trait]
impl Transport for Phone {
    async fn identity(&self, _: Provider, _: &Token) -> Result<Identity, Error> {
        Err(Error::Unsupported)
    }
    async fn pair(
        &self,
        _: &sqlx::SqlitePool,
        vault: Vault,
        progress: &Progress,
        stop: CancellationToken,
    ) -> Result<Identity, Error> {
        vault.put("device", "self", DEVICE_SECRET).await?;
        progress.code("2@scan-me".into(), 60);
        if self.expire.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::Expired);
        }
        tokio::select! {
            _ = stop.cancelled() => return Err(Error::Inactive),
            () = self.scanned.notified() => {}
        }
        let number = self.number.lock().unwrap().clone();
        Ok(Identity {
            id: number.clone(),
            display_name: "Ada".into(),
            username: Some(format!("+{number}")),
        })
    }
    async fn destinations(&self, _: &Session) -> Result<Vec<Destination>, Error> {
        Ok(vec![])
    }
    async fn resolve(&self, _: &Connection, _: &Session, _: &str) -> Result<Destination, Error> {
        Err(Error::Unsupported)
    }
    async fn execute(&self, _: &Session, command: &Command) -> Result<Value, Error> {
        Ok(json!({"ran": command}))
    }
    async fn send(&self, _: &Session, _: &str, _: &str, _: Option<&str>) -> Result<Value, Error> {
        Ok(json!({"id":"sent"}))
    }
    async fn listen(
        &self,
        _: &Connection,
        session: &Session,
        _: i64,
        _: mpsc::Sender<Envelope>,
        stop: CancellationToken,
    ) -> Result<(), Error> {
        assert!(
            matches!(session, Session::WhatsApp(_)),
            "a paired connection opens its device"
        );
        if let Some(error) = self.listen.lock().unwrap().take() {
            return Err(error);
        }
        stop.cancelled().await;
        // A real client flushes its device state here.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

fn manager(db: &TestDb, phone: &Arc<Phone>) -> Arc<Manager> {
    Arc::new(Manager::with_transport(
        db.pool.clone(),
        Arc::new(key()),
        phone.clone(),
    ))
}

/// Wait for the pairing to reach a state `done` accepts.
async fn until(m: &Manager, purpose: Purpose, done: impl Fn(&Pairing) -> bool) -> View {
    let mut after = 0;
    tokio::time::timeout(common::SETTLE, async {
        loop {
            let view = m.pairing(purpose, after).await.unwrap();
            if done(&view.pairing) {
                return view;
            }
            after = view.revision;
        }
    })
    .await
    .expect("pairing settled")
}

async fn stores(db: &TestDb) -> Vec<(Uuid, Option<Uuid>)> {
    sqlx::query_as("SELECT id,connection_id FROM social_device_stores ORDER BY created_at")
        .fetch_all(&db.pool)
        .await
        .unwrap()
}

async fn whatsapp_setup(db: &TestDb) -> (Uuid, store::Setup) {
    let bot = bot(db).await;
    let turn = turn(db, bot, "user").await;
    let setup = store::request_setup(&db.pool, turn, Provider::WhatsApp, "Watch the Family group")
        .await
        .unwrap();
    (turn, setup)
}

async fn paired(db: &TestDb, m: &Arc<Manager>, phone: &Phone) -> (store::Setup, Connection) {
    let (_, setup) = whatsapp_setup(db).await;
    let purpose = Purpose::Setup(setup.id);
    m.start_pairing(purpose).await.unwrap();
    let code = until(m, purpose, |p| matches!(p, Pairing::Code { .. })).await;
    assert!(
        matches!(&code.pairing, Pairing::Code { code, .. } if code == "2@scan-me"),
        "the card shows the phone's code"
    );
    phone.scanned.notify_one();
    let done = until(m, purpose, Pairing::finished).await;
    let Pairing::Paired { connection } = done.pairing else {
        panic!("linked, got {:?}", done.pairing);
    };
    (setup, *connection)
}

#[tokio::test]
async fn a_scanned_code_links_an_encrypted_device_and_returns_the_owners_words() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    let m = manager(&db, &phone);
    let (setup, c) = paired(&db, &m, &phone).await;
    assert_eq!(
        (c.provider, c.remote_id.as_str(), c.username.as_deref()),
        (Provider::WhatsApp, "33612345678", Some("+33612345678"))
    );
    assert_eq!(stores(&db).await.len(), 1);
    assert_eq!(
        stores(&db).await[0].1,
        Some(c.id),
        "the store belongs to the connection"
    );
    let sealed: Vec<Vec<u8>> = sqlx::query_scalar("SELECT value FROM social_device_state")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert!(!sealed.is_empty());
    assert!(
        sealed
            .iter()
            .all(|v| !v.windows(DEVICE_SECRET.len()).any(|w| w == DEVICE_SECRET)),
        "device keys are sealed at rest"
    );
    match store::credential(&db.pool, &key(), &c).await.unwrap() {
        Credential::Device(_) => {}
        Credential::Token(_) => panic!("a paired device holds a device key, not a token"),
    }

    let done = store::settle_setup(
        &db.pool,
        setup.id,
        Some(c.id),
        Some("  Only read the Family group; never draft.  "),
    )
    .await
    .unwrap();
    assert_eq!(
        done.note.as_deref(),
        Some("Only read the Family group; never draft.")
    );
    let delivered = store::setup_outcomes(&mut db.pool.acquire().await.unwrap(), setup.turn_id)
        .await
        .unwrap();
    assert_eq!(
        delivered[0].note.as_deref(),
        Some("Only read the Family group; never draft."),
        "the bot hears the owner's words with the result"
    );
    assert!(
        store::settle_setup(&db.pool, setup.id, Some(c.id), Some("other words"))
            .await
            .is_err(),
        "a settled setup keeps the words it settled with"
    );
}

#[tokio::test]
async fn an_unscanned_or_cancelled_pairing_leaves_no_device_and_can_start_again() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    phone
        .expire
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let m = manager(&db, &phone);
    let (_, setup) = whatsapp_setup(&db).await;
    let purpose = Purpose::Setup(setup.id);
    m.start_pairing(purpose).await.unwrap();
    let done = until(&m, purpose, Pairing::finished).await;
    assert!(
        matches!(done.pairing, Pairing::Expired),
        "{:?}",
        done.pairing
    );
    assert!(
        stores(&db).await.is_empty(),
        "the unscanned device store is deleted"
    );

    phone
        .expire
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let again = m.start_pairing(purpose).await.unwrap();
    assert_eq!(
        again.revision, 1,
        "a finished pairing is replaced by a new one"
    );
    until(&m, purpose, |p| matches!(p, Pairing::Code { .. })).await;
    let running = m.start_pairing(purpose).await.unwrap();
    assert!(
        !running.pairing.finished(),
        "starting a running pairing returns it"
    );
    m.stop_pairing(purpose).await;
    let done = until(&m, purpose, Pairing::finished).await;
    assert!(
        matches!(done.pairing, Pairing::Cancelled),
        "{:?}",
        done.pairing
    );
    assert!(stores(&db).await.is_empty());
    assert!(store::connections(&db.pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn only_a_pending_pairing_setup_starts_a_pairing() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    let m = manager(&db, &phone);
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let telegram = store::request_setup(&db.pool, turn, Provider::Telegram, "Team support")
        .await
        .unwrap();
    assert!(matches!(
        m.start_pairing(Purpose::Setup(telegram.id)).await,
        Err(Error::Unsupported)
    ));
    let (_, setup) = whatsapp_setup(&db).await;
    store::settle_setup(&db.pool, setup.id, None, None)
        .await
        .unwrap();
    assert!(matches!(
        m.start_pairing(Purpose::Setup(setup.id)).await,
        Err(Error::Inactive)
    ));
    assert!(matches!(
        m.pairing(Purpose::Setup(Uuid::new_v4()), 0).await,
        Err(Error::NotFound)
    ));
}

#[tokio::test]
async fn pairing_again_replaces_the_device_only_for_the_same_account() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    let m = manager(&db, &phone);
    let (_, c) = paired(&db, &m, &phone).await;
    let first = stores(&db).await[0].0;
    let repair = Purpose::Repair(c.id);

    *phone.number.lock().unwrap() = "4915112345678".into();
    m.start_pairing(repair).await.unwrap();
    until(&m, repair, |p| matches!(p, Pairing::Code { .. })).await;
    phone.scanned.notify_one();
    let refused = until(&m, repair, Pairing::finished).await;
    assert!(
        matches!(refused.pairing, Pairing::Failed { .. }),
        "{:?}",
        refused.pairing
    );
    assert_eq!(
        stores(&db).await,
        vec![(first, Some(c.id))],
        "the old device stays"
    );
    assert_eq!(
        store::connection(&db.pool, c.id).await.unwrap().generation,
        c.generation
    );

    *phone.number.lock().unwrap() = "33612345678".into();
    m.start_pairing(repair).await.unwrap();
    until(&m, repair, |p| matches!(p, Pairing::Code { .. })).await;
    phone.scanned.notify_one();
    let done = until(&m, repair, Pairing::finished).await;
    let Pairing::Paired { connection } = done.pairing else {
        panic!("re-paired, got {:?}", done.pairing);
    };
    assert_eq!(connection.id, c.id);
    assert_eq!(connection.generation, c.generation + 1);
    let now = stores(&db).await;
    assert_eq!(now.len(), 1);
    assert_ne!(now[0].0, first, "the new device replaces the old one");
    assert_eq!(now[0].1, Some(c.id));

    store::set_state(&db.pool, c.id, ConnectionState::Removed)
        .await
        .unwrap();
    assert!(
        stores(&db).await.is_empty(),
        "removing the connection deletes its device"
    );
}

#[tokio::test]
async fn a_device_the_account_signed_out_asks_to_pair_again_and_stops_retrying() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    let m = manager(&db, &phone);
    let (_, c) = paired(&db, &m, &phone).await;
    *phone.listen.lock().unwrap() = Some(Error::SignedOut);
    m.changed(c.id).await;
    tokio::time::timeout(common::SETTLE, async {
        while store::connection(&db.pool, c.id).await.unwrap().health != "signed_out" {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("health shows the sign-out");
    m.changed(c.id).await;
    tokio::time::timeout(common::SETTLE, async {
        while store::connection(&db.pool, c.id).await.unwrap().health != "connected" {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("a reload after the owner acts listens again");
}

#[tokio::test]
async fn synced_history_is_readable_but_never_wakes_a_trigger() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    let m = manager(&db, &phone);
    let (_, c) = paired(&db, &m, &phone).await;
    let bot = bot(&db).await;
    let owner = turn(&db, bot, "user").await;
    store::set_trigger(
        &db.pool,
        owner,
        &TriggerInput {
            connection_id: c.id,
            destination: None,
            wake: Wake::All,
            instructions: "Tell me when the family asks about Sunday.".into(),
        },
    )
    .await
    .unwrap();
    let c = store::connection(&db.pool, c.id).await.unwrap();
    for (event, replay) in [("old", true), ("new", false)] {
        store::record(
            &db.pool,
            &c,
            &Incoming {
                event_id: format!("family@g.us/{event}"),
                destination: Destination {
                    id: "family@g.us".into(),
                    name: "Family".into(),
                    kind: "group".into(),
                    can_send: true,
                },
                message_id: event.into(),
                author_id: "4915112345678@s.whatsapp.net".into(),
                body: "Sunday lunch?".into(),
                mentioned: false,
                direct: false,
                from_bot: false,
                replay,
            },
            0,
        )
        .await
        .unwrap();
    }
    let woke: Vec<String> = sqlx::query_scalar("SELECT message_id FROM social_deliveries")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(woke, vec!["new".to_owned()]);
    let history = m
        .execute(
            c.id,
            bot,
            owner,
            None,
            Command::History {
                destination: "family@g.us".into(),
                before: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(history["messages"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn creating_a_group_is_a_whatsapp_mutation_with_a_durable_receipt() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    let m = manager(&db, &phone);
    let (_, c) = paired(&db, &m, &phone).await;
    let bot = bot(&db).await;
    let owner = turn(&db, bot, "user").await;
    let create = Command::CreateGroup {
        name: "Sunday lunch".into(),
        participants: vec!["+49 151 12345678".into()],
    };
    assert!(Provider::WhatsApp.supports(CommandKind::CreateGroup));
    assert!(!Provider::Telegram.supports(CommandKind::CreateGroup));
    assert!(CommandKind::CreateGroup.mutates() && CommandKind::CreateGroup.sends());
    assert!(matches!(
        m.execute(c.id, bot, owner, None, create.clone()).await,
        Err(Error::Invalid(_))
    ));
    let action = Uuid::new_v4();
    let receipt = m
        .execute(c.id, bot, owner, Some(action), create.clone())
        .await
        .unwrap();
    assert_eq!(receipt["status"], "succeeded", "{receipt}");
    let again = m
        .execute(c.id, bot, owner, Some(action), create)
        .await
        .unwrap();
    assert_eq!(again, receipt, "a retry returns the stored receipt");
    assert!(
        Command::CreateGroup {
            name: "x".into(),
            participants: vec![],
        }
        .validate()
        .is_err(),
        "a group needs someone besides the owner"
    );
}

#[tokio::test]
async fn a_code_scanned_after_its_setup_ended_links_no_account() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    let m = manager(&db, &phone);
    let (_, setup) = whatsapp_setup(&db).await;
    let purpose = Purpose::Setup(setup.id);
    m.start_pairing(purpose).await.unwrap();
    until(&m, purpose, |p| matches!(p, Pairing::Code { .. })).await;
    sqlx::query("UPDATE social_setups SET status='cancelled' WHERE id=?1")
        .bind(setup.id)
        .execute(&db.pool)
        .await
        .unwrap();
    phone.scanned.notify_one();
    tokio::time::timeout(common::SETTLE, async {
        while !stores(&db).await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the rejected device store is deleted");
    let done = until(&m, purpose, Pairing::finished).await;
    assert!(
        matches!(done.pairing, Pairing::Failed { .. }),
        "linking an ended setup is rejected, got {:?}",
        done.pairing
    );
    let connections: i64 = sqlx::query_scalar("SELECT count(*) FROM social_connections")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(connections, 0, "an ended setup activates nothing");
    assert!(
        matches!(m.start_pairing(purpose).await, Err(Error::Inactive)),
        "an ended setup cannot pair again"
    );
}

#[tokio::test]
async fn a_linked_result_survives_other_pairings_until_its_card_completes() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    let m = manager(&db, &phone);
    let (setup, c) = paired(&db, &m, &phone).await;
    let first = Purpose::Setup(setup.id);
    let (_, other) = whatsapp_setup(&db).await;
    m.start_pairing(Purpose::Setup(other.id)).await.unwrap();
    let read = m.pairing(first, 0).await.unwrap();
    assert!(
        matches!(&read.pairing, Pairing::Paired { connection } if connection.id == c.id),
        "another card starting keeps this card's result, got {:?}",
        read.pairing
    );
    let again = m.start_pairing(first).await.unwrap();
    assert!(
        matches!(&again.pairing, Pairing::Paired { connection } if connection.id == c.id),
        "restarting a linked setup returns its connection, not a new code"
    );
    m.stop_pairing(first).await;
    assert!(
        matches!(m.pairing(first, 0).await, Err(Error::NotFound)),
        "completing the card forgets the result"
    );
}

#[tokio::test]
async fn pausing_a_connection_lets_its_listener_shut_down_its_client() {
    let db = TestDb::migrated().await;
    let phone = Phone::new("33612345678");
    let m = manager(&db, &phone);
    let (_, c) = paired(&db, &m, &phone).await;
    tokio::time::timeout(common::SETTLE, async {
        while store::connection(&db.pool, c.id).await.unwrap().health != "connected" {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the listener connected");
    store::set_state(&db.pool, c.id, ConnectionState::Paused)
        .await
        .unwrap();
    m.changed(c.id).await;
    tokio::time::timeout(common::SETTLE, async {
        while !phone.closed.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the listener finished its shutdown instead of being dropped");
}
