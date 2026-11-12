use crate::common;

use std::sync::Arc;
use std::time::Duration;

use common::TestDb;
use else_api::connectors::delivery;
use else_api::connectors::store::{self, ActionStart, ActionStatus, StoreError};
use else_api::connectors::types::{
    Arrival, Credential, Cursor, Draft, Endpoint, MailCommand, MailConfig, MessageRef, Permissions,
    Provider, Scan,
};
use else_api::credentials::SecretKey;
use else_api::providers::Registry;
use else_api::providers::synthetic::Synthetic;
use serde_json::json;
use uuid::Uuid;

const PASSWORD: &str = "correct horse battery staple";

fn key() -> SecretKey {
    SecretKey::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap()
}

fn other_key() -> SecretKey {
    SecretKey::parse("HwECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap()
}

fn registry() -> Registry {
    Registry::new(vec![Arc::new(Synthetic::new(Duration::from_millis(5)))])
}

#[tokio::test]
async fn deleting_a_bot_revokes_mail_and_releases_its_grant_slots() {
    let db = TestDb::migrated().await;
    let account = store::create(
        &db.pool,
        &key(),
        &config("deleted@example.test"),
        &password(PASSWORD),
    )
    .await
    .unwrap();
    let bot = bot(&db, "Temporary").await;
    store::set_permissions(&db.pool, account.id, bot, &all())
        .await
        .unwrap();
    store::record_scan(
        &db.pool,
        account.id,
        account.generation,
        None,
        &scan(1, 0, &[]),
    )
    .await
    .unwrap();
    store::record_scan(
        &db.pool,
        account.id,
        account.generation,
        Some(&Cursor {
            uid_validity: 1,
            last_uid: 0,
        }),
        &scan(1, 1, &[1]),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE bots SET status='deleting',deleted_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE id=?1").bind(bot).execute(&db.pool).await.unwrap();
    assert!(
        store::grants(&db.pool, account.id)
            .await
            .unwrap()
            .is_empty()
    );
    let revoked: String =
        sqlx::query_scalar("SELECT status FROM connector_deliveries WHERE bot_id=?1")
            .bind(bot)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(revoked, "revoked");
}

#[tokio::test]
async fn unavailable_providers_do_not_starve_other_mail_when_admission_is_limited() {
    let db = TestDb::migrated().await;
    let account = store::create(
        &db.pool,
        &key(),
        &config("fairness@example.test"),
        &password(PASSWORD),
    )
    .await
    .unwrap();
    let blocked = bot(&db, "Blocked").await;
    let ready = bot(&db, "Ready").await;
    sqlx::query("INSERT INTO bot_profiles (bot_id,provider_id,model,reasoning_effort,service_tier) VALUES (?1,'codex','test','medium','default')").bind(blocked).execute(&db.pool).await.unwrap();
    for bot in [blocked, ready] {
        store::set_permissions(&db.pool, account.id, bot, &all())
            .await
            .unwrap();
    }
    store::record_scan(
        &db.pool,
        account.id,
        account.generation,
        None,
        &scan(1, 0, &[]),
    )
    .await
    .unwrap();
    store::record_scan(
        &db.pool,
        account.id,
        account.generation,
        Some(&Cursor {
            uid_validity: 1,
            last_uid: 0,
        }),
        &scan(1, 1, &[1]),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE connector_deliveries SET created_at=CASE WHEN bot_id=?1 THEN '2000-01-01T00:00:00Z' ELSE '2001-01-01T00:00:00Z' END").bind(blocked).execute(&db.pool).await.unwrap();
    let registry = Registry::new(vec![
        Arc::new(else_api::providers::codex::Codex::detect(None, None, None)),
        Arc::new(Synthetic::new(Duration::from_millis(5))),
    ]);
    assert_eq!(
        delivery::admit_pending(&db.pool, &registry, 1)
            .await
            .unwrap(),
        1
    );
    assert_eq!(connector_turns(&db, ready).await.len(), 1);
    assert!(connector_turns(&db, blocked).await.is_empty());
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
            })
            .collect(),
        more: false,
    }
}

async fn bot(db: &TestDb, name: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, ?2)")
        .bind(id)
        .bind(name)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (?1, ?2)")
        .bind(Uuid::new_v4())
        .bind(id)
        .execute(&db.pool)
        .await
        .unwrap();
    id
}

async fn turn(db: &TestDb, bot: Uuid, status: &str) -> Uuid {
    let id = Uuid::new_v4();
    let (conversation, last): (Uuid, i64) = sqlx::query_as(
        "SELECT id, last_turn_no FROM conversations WHERE bot_id = ?1 AND kind = 'owner'",
    )
    .bind(bot)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status)
         VALUES (?1, ?2, ?3, ?4, zeroblob(32), ?5, ?6, 'synthetic', ?7)",
    )
    .bind(id)
    .bind(bot)
    .bind(conversation)
    .bind(Uuid::new_v4())
    .bind(last + 1)
    .bind(Uuid::new_v4())
    .bind(status)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE conversations SET last_turn_no = ?2 WHERE id = ?1")
        .bind(conversation)
        .bind(last + 1)
        .execute(&db.pool)
        .await
        .unwrap();
    id
}

async fn set_turn(db: &TestDb, turn: Uuid, status: &str) {
    sqlx::query("UPDATE turns SET status = ?2 WHERE id = ?1")
        .bind(turn)
        .bind(status)
        .execute(&db.pool)
        .await
        .unwrap();
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

#[tokio::test]
async fn credentials_are_encrypted_and_bound_to_their_account() {
    let db = TestDb::migrated().await;
    let first = store::create(
        &db.pool,
        &key(),
        &config("one@example.test"),
        &password(PASSWORD),
    )
    .await
    .unwrap();
    let second = store::create(
        &db.pool,
        &key(),
        &config("two@example.test"),
        &password("another secret"),
    )
    .await
    .unwrap();
    assert!(
        !serde_json::to_string(&first).unwrap().contains(PASSWORD),
        "an account never carries its credential"
    );
    let opened = store::credential(&db.pool, &key(), first.id).await.unwrap();
    let Credential::Password { password: stored } = &opened else {
        panic!("a password credential");
    };
    assert_eq!(stored, PASSWORD);
    let (salt, cipher): (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT credential_salt, credential FROM connector_accounts WHERE id = ?1")
            .bind(first.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        !cipher
            .windows(PASSWORD.len())
            .any(|w| w == PASSWORD.as_bytes()),
        "stored encrypted"
    );
    assert!(matches!(
        store::credential(&db.pool, &other_key(), first.id).await,
        Err(StoreError::Crypto)
    ));

    sqlx::query(
        "UPDATE connector_accounts SET credential_salt = ?2, credential = ?3 WHERE id = ?1",
    )
    .bind(second.id)
    .bind(&salt)
    .bind(&cipher)
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        matches!(
            store::credential(&db.pool, &key(), second.id).await,
            Err(StoreError::Crypto)
        ),
        "a credential copied to another account does not decrypt"
    );
    let err = store::credential(&db.pool, &key(), second.id)
        .await
        .err()
        .unwrap();
    assert!(
        !format!("{err} {err:?}").contains(PASSWORD),
        "errors carry no secret"
    );

    assert!(
        !store::save_credential(&db.pool, &key(), first.id, 2, &password("stale"))
            .await
            .unwrap(),
        "fenced by generation"
    );
    assert!(
        store::save_credential(&db.pool, &key(), first.id, 1, &password("refreshed"))
            .await
            .unwrap()
    );
    let opened = store::credential(&db.pool, &key(), first.id).await.unwrap();
    let Credential::Password {
        password: refreshed,
    } = &opened
    else {
        panic!("a password credential");
    };
    assert_eq!(refreshed, "refreshed");
    sqlx::query(
        "UPDATE connector_accounts SET credential_salt = ?2, credential = ?3 WHERE id = ?1",
    )
    .bind(first.id)
    .bind(&salt)
    .bind(&cipher)
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        matches!(
            store::credential(&db.pool, &key(), first.id).await,
            Err(StoreError::Crypto)
        ),
        "an older credential version does not decrypt in place of the current one"
    );

    assert!(
        store::replace_credential(&db.pool, &key(), first.id, &password("replaced"))
            .await
            .unwrap()
    );
    assert_eq!(
        store::account(&db.pool, first.id)
            .await
            .unwrap()
            .unwrap()
            .generation,
        2,
        "replacement fences old work"
    );

    assert!(store::remove(&db.pool, first.id).await.unwrap());
    assert!(!store::remove(&db.pool, first.id).await.unwrap());
    let erased: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT credential FROM connector_accounts WHERE id = ?1")
            .bind(first.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(erased.is_none(), "removal erases the credential");
    assert!(matches!(
        store::credential(&db.pool, &key(), first.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(store::account(&db.pool, first.id).await.unwrap().is_none());
}

#[tokio::test]
async fn accounts_and_grants_are_bounded() {
    let db = TestDb::migrated().await;
    let first = store::create(
        &db.pool,
        &key(),
        &config("a0@example.test"),
        &password(PASSWORD),
    )
    .await
    .unwrap();
    assert!(matches!(
        store::create(
            &db.pool,
            &key(),
            &config("A0@example.test"),
            &password(PASSWORD)
        )
        .await,
        Err(StoreError::AddressTaken)
    ));
    for i in 1..32 {
        store::create(
            &db.pool,
            &key(),
            &config(&format!("a{i}@example.test")),
            &password(PASSWORD),
        )
        .await
        .unwrap();
    }
    assert!(matches!(
        store::create(
            &db.pool,
            &key(),
            &config("extra@example.test"),
            &password(PASSWORD)
        )
        .await,
        Err(StoreError::Limit(_))
    ));
    assert!(matches!(
        store::create(&db.pool, &key(), &config("bad"), &password(PASSWORD)).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(store::accounts(&db.pool).await.unwrap().len(), 32);

    let accounts = store::accounts(&db.pool).await.unwrap();
    let mut granted = 0;
    'grant: for i in 0..5 {
        let bot = bot(&db, &format!("Bot {i}")).await;
        for account in &accounts {
            if granted == 128 {
                assert!(matches!(
                    store::set_permissions(&db.pool, account.id, bot, &read_only()).await,
                    Err(StoreError::Limit(_))
                ));
                break 'grant;
            }
            assert!(
                store::set_permissions(&db.pool, account.id, bot, &read_only())
                    .await
                    .unwrap()
            );
            granted += 1;
        }
    }
    assert_eq!(granted, 128);
    assert!(store::remove(&db.pool, first.id).await.unwrap());
    assert!(
        store::grants(&db.pool, first.id).await.unwrap().is_empty(),
        "removal drops grants"
    );
}

#[tokio::test]
async fn explicit_permissions_restrict_bots_and_revocation_withdraws_pending_mail() {
    let db = TestDb::migrated().await;
    let (account, generation) = watched(&db, "inbox@example.test").await;
    let (waker, reader) = (bot(&db, "Waker").await, bot(&db, "Reader").await);
    assert!(matches!(
        store::set_permissions(
            &db.pool,
            account,
            waker,
            &Permissions {
                wake: true,
                ..Permissions::default()
            }
        )
        .await,
        Err(StoreError::Invalid(_))
    ));
    assert!(
        store::set_permissions(
            &db.pool,
            account,
            waker,
            &Permissions {
                read: true,
                wake: true,
                ..Permissions::default()
            }
        )
        .await
        .unwrap()
    );
    assert!(
        store::set_permissions(&db.pool, account, reader, &read_only())
            .await
            .unwrap()
    );
    assert!(
        !store::set_permissions(&db.pool, account, reader, &read_only())
            .await
            .unwrap(),
        "unchanged"
    );

    let old = Cursor {
        uid_validity: 7,
        last_uid: 10,
    };
    assert!(
        store::record_scan(
            &db.pool,
            account,
            generation,
            Some(&old),
            &scan(7, 12, &[11, 12])
        )
        .await
        .unwrap()
    );
    assert_eq!(
        deliveries(&db, waker).await,
        vec![(11, "pending".into()), (12, "pending".into())]
    );
    assert!(
        deliveries(&db, reader).await.is_empty(),
        "read without wake is never woken"
    );

    let running = turn(&db, reader, "running").await;
    let move_it = MailCommand::Move {
        message: message(7, 11),
        folder: "Archive".into(),
    };
    assert!(matches!(
        store::begin_action(
            &db.pool,
            account,
            generation,
            reader,
            running,
            Uuid::new_v4(),
            &move_it
        )
        .await,
        Err(StoreError::Forbidden)
    ));
    let read = MailCommand::Read {
        message: message(7, 11),
    };
    store::authorize(&db.pool, account, generation, reader, running, &read)
        .await
        .unwrap();
    let stranger = bot(&db, "Stranger").await;
    let stranger_turn = turn(&db, stranger, "running").await;
    store::authorize(
        &db.pool,
        account,
        generation,
        stranger,
        stranger_turn,
        &read,
    )
    .await
    .unwrap();
    let defaults = store::permissions(&db.pool, account, stranger)
        .await
        .unwrap()
        .unwrap();
    assert!(defaults.read && defaults.write && defaults.send);
    assert!(!defaults.wake);
    store::set_permissions(&db.pool, account, stranger, &Permissions::default())
        .await
        .unwrap();
    assert!(matches!(
        store::authorize(
            &db.pool,
            account,
            generation,
            stranger,
            stranger_turn,
            &read
        )
        .await,
        Err(StoreError::Forbidden)
    ));

    assert!(
        store::set_permissions(&db.pool, account, waker, &read_only())
            .await
            .unwrap()
    );
    assert_eq!(
        deliveries(&db, waker).await,
        vec![(11, "revoked".into()), (12, "revoked".into())]
    );
    assert!(
        store::set_permissions(
            &db.pool,
            account,
            waker,
            &Permissions {
                read: true,
                wake: true,
                ..Permissions::default()
            }
        )
        .await
        .unwrap()
    );
    assert_eq!(
        delivery::admit_pending(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        0,
        "revoked mail never reaches the new grant"
    );

    let old = Cursor {
        uid_validity: 7,
        last_uid: 12,
    };
    assert!(
        store::record_scan(
            &db.pool,
            account,
            generation,
            Some(&old),
            &scan(7, 13, &[13])
        )
        .await
        .unwrap()
    );
    assert_eq!(
        delivery::admit_pending(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        1
    );
    let turns = connector_turns(&db, waker).await;
    assert_eq!(turns.len(), 1);
    let messages = delivery::for_turn(&mut db.pool.acquire().await.unwrap(), turns[0].0)
        .await
        .unwrap();
    assert_eq!(
        messages,
        vec![(account, message(7, 13))],
        "a new grant covers only later mail"
    );
}

#[tokio::test]
async fn scans_are_fenced_bounded_and_quiet_at_first() {
    let db = TestDb::migrated().await;
    let account = store::create(
        &db.pool,
        &key(),
        &config("scan@example.test"),
        &password(PASSWORD),
    )
    .await
    .unwrap();
    let bot = bot(&db, "Waker").await;
    store::set_permissions(&db.pool, account.id, bot, &all())
        .await
        .unwrap();
    let events = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM connector_events")
            .fetch_one(&db.pool)
            .await
            .unwrap()
    };

    assert!(
        store::record_scan(&db.pool, account.id, 1, None, &scan(7, 10, &[8, 9, 10]))
            .await
            .unwrap()
    );
    assert_eq!(events().await, 0, "mail already there wakes no one");
    let start = Cursor {
        uid_validity: 7,
        last_uid: 10,
    };
    assert_eq!(
        store::cursor(&db.pool, account.id).await.unwrap(),
        Some(start.clone())
    );
    assert!(
        !store::record_scan(&db.pool, account.id, 1, None, &scan(7, 11, &[11]))
            .await
            .unwrap(),
        "stale cursor"
    );
    assert!(
        !store::record_scan(&db.pool, account.id, 2, Some(&start), &scan(7, 11, &[11]))
            .await
            .unwrap(),
        "stale generation"
    );
    assert!(
        !store::record_scan(&db.pool, account.id, 1, Some(&start), &scan(7, 9, &[]))
            .await
            .unwrap(),
        "never moves back"
    );

    let mut noisy = scan(7, 14, &[9, 11, 11, 12, 15]);
    noisy.arrivals.push(Arrival {
        message: message(6, 13),
    });
    assert!(
        store::record_scan(&db.pool, account.id, 1, Some(&start), &noisy)
            .await
            .unwrap()
    );
    assert_eq!(
        deliveries(&db, bot).await,
        vec![(11, "pending".into()), (12, "pending".into())]
    );

    let at = Cursor {
        uid_validity: 7,
        last_uid: 14,
    };
    let many: Vec<u32> = (15..=164).collect();
    assert!(
        store::record_scan(&db.pool, account.id, 1, Some(&at), &scan(7, 164, &many))
            .await
            .unwrap()
    );
    assert_eq!(events().await, 2 + store::MAX_ARRIVALS as i64);
    let stopped = Cursor {
        uid_validity: 7,
        last_uid: 14 + store::MAX_ARRIVALS as u32,
    };
    assert_eq!(
        store::cursor(&db.pool, account.id).await.unwrap(),
        Some(stopped.clone()),
        "the next scan continues"
    );

    assert!(
        store::record_scan(
            &db.pool,
            account.id,
            1,
            Some(&stopped),
            &scan(8, 3, &[1, 2, 3])
        )
        .await
        .unwrap()
    );
    assert_eq!(
        events().await,
        2 + store::MAX_ARRIVALS as i64,
        "a new UIDVALIDITY rescans quietly"
    );

    let paused = store::set_active(&db.pool, account.id, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((paused.state.as_str(), paused.generation), ("paused", 2));
    let resting = Cursor {
        uid_validity: 8,
        last_uid: 3,
    };
    assert!(
        !store::record_scan(&db.pool, account.id, 2, Some(&resting), &scan(8, 4, &[4]))
            .await
            .unwrap(),
        "paused"
    );
    assert_eq!(
        store::set_active(&db.pool, account.id, false)
            .await
            .unwrap()
            .unwrap()
            .generation,
        2,
        "idempotent"
    );
    let resumed = store::set_active(&db.pool, account.id, true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.generation, 3);
    assert_eq!(
        store::cursor(&db.pool, account.id).await.unwrap(),
        Some(resting.clone()),
        "resume keeps the cursor"
    );
    assert!(
        store::record_scan(&db.pool, account.id, 3, Some(&resting), &scan(8, 4, &[4]))
            .await
            .unwrap()
    );
    assert_eq!(events().await, 3 + store::MAX_ARRIVALS as i64);
}

#[tokio::test]
async fn admission_batches_mail_and_leaves_it_pending_under_backpressure() {
    let db = TestDb::migrated().await;
    let (account, generation) = watched(&db, "queue@example.test").await;
    let bot = bot(&db, "Waker").await;
    store::set_permissions(&db.pool, account, bot, &all())
        .await
        .unwrap();
    let record = |from: u32, to: u32| {
        let pool = db.pool.clone();
        async move {
            let old = Cursor {
                uid_validity: 7,
                last_uid: from,
            };
            let uids: Vec<u32> = (from + 1..=to).collect();
            assert!(
                store::record_scan(&pool, account, generation, Some(&old), &scan(7, to, &uids))
                    .await
                    .unwrap()
            );
        }
    };

    record(10, 13).await;
    assert_eq!(
        delivery::admit_pending(&db.pool, &Registry::new(vec![]), 8)
            .await
            .unwrap(),
        0,
        "no provider"
    );
    assert_eq!(
        delivery::admit_pending(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        1
    );
    let turns = connector_turns(&db, bot).await;
    assert_eq!(turns.len(), 1, "one turn carries all new mail");
    assert!(
        turns[0].1.contains("queue@example.test") && turns[0].1.contains("uid 13"),
        "{}",
        turns[0].1
    );
    assert_eq!(
        delivery::for_turn(&mut db.pool.acquire().await.unwrap(), turns[0].0)
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        store::event_account(&db.pool, turns[0].0).await.unwrap(),
        Some(account)
    );

    record(13, 14).await;
    assert_eq!(
        delivery::admit_pending(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        0,
        "waits for the unfinished turn"
    );
    assert_eq!(
        delivery::admit_pending(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        0,
        "and does not repeat"
    );
    set_turn(&db, turns[0].0, "completed").await;

    let mut queued = Vec::new();
    for _ in 0..4 {
        queued.push(turn(&db, bot, "queued").await);
    }
    assert_eq!(
        delivery::admit_pending(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        0,
        "queue full"
    );
    for id in queued {
        set_turn(&db, id, "completed").await;
    }
    store::set_active(&db.pool, account, false).await.unwrap();
    assert_eq!(
        delivery::admit_pending(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        0,
        "paused account"
    );
    store::set_active(&db.pool, account, true).await.unwrap();
    assert_eq!(
        deliveries(&db, bot).await.last(),
        Some(&(14, "pending".into()))
    );

    assert_eq!(
        delivery::admit_pending(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        1
    );
    assert_eq!(connector_turns(&db, bot).await.len(), 2);
    assert!(
        deliveries(&db, bot)
            .await
            .iter()
            .all(|(_, status)| status == "delivered")
    );
}

#[tokio::test]
async fn an_action_is_dispatched_once_and_never_replayed_after_a_crash() {
    let db = TestDb::migrated().await;
    let (account, generation) = watched(&db, "act@example.test").await;
    let bot = bot(&db, "Actor").await;
    store::set_permissions(
        &db.pool,
        account,
        bot,
        &Permissions {
            send: false,
            ..all()
        },
    )
    .await
    .unwrap();
    let running = turn(&db, bot, "running").await;
    let archive = MailCommand::Archive {
        message: message(7, 3),
    };
    let action = Uuid::new_v4();

    let begin = |action: Uuid, command: MailCommand| {
        let pool = db.pool.clone();
        async move {
            store::begin_action(&pool, account, generation, bot, running, action, &command).await
        }
    };
    assert_eq!(
        begin(action, archive.clone()).await.unwrap(),
        ActionStart::Dispatch
    );
    assert_eq!(
        begin(action, archive.clone()).await.unwrap(),
        ActionStart::Existing(json!({ "action_id": action, "status": "dispatching" })),
        "a retry while in flight is not sent again"
    );
    assert!(matches!(
        begin(
            action,
            MailCommand::Trash {
                message: message(7, 3)
            }
        )
        .await,
        Err(StoreError::Conflict)
    ));
    let receipt =
        json!({ "action_id": action, "status": "succeeded", "result": { "moved": true } });
    assert!(
        store::finish_action(&db.pool, account, action, ActionStatus::Succeeded, &receipt)
            .await
            .unwrap()
    );
    assert!(
        !store::finish_action(&db.pool, account, action, ActionStatus::Failed, &json!({}))
            .await
            .unwrap()
    );
    assert_eq!(
        begin(action, archive.clone()).await.unwrap(),
        ActionStart::Existing(receipt.clone())
    );
    assert_eq!(
        store::action_status(&db.pool, account, bot, action)
            .await
            .unwrap(),
        Some(receipt)
    );

    let crashed = Uuid::new_v4();
    assert_eq!(
        begin(crashed, archive.clone()).await.unwrap(),
        ActionStart::Dispatch
    );
    assert_eq!(store::recover(&db.pool).await.unwrap(), 1);
    assert_eq!(
        begin(crashed, archive.clone()).await.unwrap(),
        ActionStart::Existing(json!({ "action_id": crashed, "status": "uncertain" }))
    );
    assert!(
        !store::finish_action(
            &db.pool,
            account,
            crashed,
            ActionStatus::Succeeded,
            &json!({})
        )
        .await
        .unwrap()
    );

    let draft = Draft {
        to: vec!["x@example.test".into()],
        cc: vec![],
        bcc: vec![],
        subject: "s".into(),
        body: "b".into(),
        attachments: vec![],
        reply_to: None,
    };
    assert!(matches!(
        begin(Uuid::new_v4(), MailCommand::Send { draft }).await,
        Err(StoreError::Forbidden)
    ));
    assert!(matches!(
        store::begin_action(
            &db.pool,
            account,
            generation + 1,
            bot,
            running,
            Uuid::new_v4(),
            &archive
        )
        .await,
        Err(StoreError::Inactive)
    ));
    set_turn(&db, running, "completed").await;
    assert!(matches!(
        begin(Uuid::new_v4(), archive.clone()).await,
        Err(StoreError::NotRunning)
    ));
}

#[tokio::test]
async fn a_restored_database_stays_passive_until_resumed() {
    let db = TestDb::migrated().await;
    let (account, generation) = watched(&db, "restore@example.test").await;
    let bot = bot(&db, "Waker").await;
    store::set_permissions(&db.pool, account, bot, &all())
        .await
        .unwrap();
    let old = Cursor {
        uid_validity: 7,
        last_uid: 10,
    };
    assert!(
        store::record_scan(
            &db.pool,
            account,
            generation,
            Some(&old),
            &scan(7, 11, &[11])
        )
        .await
        .unwrap()
    );
    let running = turn(&db, bot, "running").await;
    let action = Uuid::new_v4();
    let flag = MailCommand::SetFlagged {
        message: message(7, 11),
        flagged: true,
    };
    store::begin_action(&db.pool, account, generation, bot, running, action, &flag)
        .await
        .unwrap();

    let mut conn = db.pool.acquire().await.unwrap();
    store::restore_passive(&mut conn).await.unwrap();
    drop(conn);
    let restored = store::account(&db.pool, account).await.unwrap().unwrap();
    assert_eq!(
        (restored.state.as_str(), restored.generation),
        ("paused", generation + 1)
    );
    assert_eq!(store::cursor(&db.pool, account).await.unwrap(), None);
    assert_eq!(deliveries(&db, bot).await, vec![(11, "cancelled".into())]);
    assert_eq!(
        store::action_status(&db.pool, account, bot, action)
            .await
            .unwrap(),
        Some(json!({ "action_id": action, "status": "uncertain" }))
    );
    assert_eq!(
        delivery::admit_pending(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        0
    );

    let resumed = store::set_active(&db.pool, account, true)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store::record_scan(
            &db.pool,
            account,
            resumed.generation,
            None,
            &scan(7, 20, &[12, 20])
        )
        .await
        .unwrap()
    );
    assert_eq!(
        deliveries(&db, bot).await.len(),
        1,
        "the first scan after a restore is quiet"
    );

    assert!(store::remove(&db.pool, account).await.unwrap());
    let again = store::create(
        &db.pool,
        &key(),
        &config("restore@example.test"),
        &password(PASSWORD),
    )
    .await
    .unwrap();
    assert_ne!(again.id, account);
    assert_eq!(
        store::cursor(&db.pool, again.id).await.unwrap(),
        None,
        "a re-added account starts quiet"
    );
}
