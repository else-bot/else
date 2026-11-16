use super::pending::SENDER_BYTES;
use super::{Backend, PREKEY, id, now};
use crate::socials::providers::whatsapp::vault::Vault;
use crate::socials::{store, types::DeviceKey};
use sqlx::SqlitePool;
use std::sync::Arc;
use uuid::Uuid;
use whatsapp_rust::bytes::Bytes;
use whatsapp_rust::wacore::{
    appstate::{hash::HashState, processor::AppStateMutationMAC},
    reporting_token::MESSAGE_SECRET_SIZE,
    store::traits::{
        AppStateSyncKey, AppSyncStore, DeviceStore, LidPnMappingEntry, MsgSecretEntry,
        MsgSecretStore, PendingInboundKey, PendingInboundLease, PendingInboundMutation,
        PendingInboundPayload, PendingInboundRevision, ProtocolStore, SignalStore,
    },
};

async fn pool() -> SqlitePool {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::migrate(&db).await.unwrap();
    db
}

fn key(byte: u8) -> DeviceKey {
    DeviceKey(zeroize::Zeroizing::new([byte; 32]))
}

async fn backend(db: &SqlitePool, device: Uuid, byte: u8) -> Backend {
    Backend::new(Vault::new(db.clone(), device, &key(byte)))
}

#[tokio::test]
async fn a_paired_device_loads_after_a_restart() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let first = backend(&db, device, 7).await;
    assert!(
        !first.exists().await.unwrap(),
        "a fresh store starts unpaired"
    );
    first.create().await.unwrap();
    let mut saved = first.load().await.unwrap().expect("create stores a device");
    assert!(saved.pn.is_none(), "a new device pairs by QR");
    saved.pn = Some("15551234567@s.whatsapp.net".parse().unwrap());
    saved.push_name = "Owner".into();
    first.save(&saved).await.unwrap();
    drop(first);

    let restarted = backend(&db, device, 7).await;
    assert!(restarted.exists().await.unwrap());
    restarted.create().await.unwrap();
    let loaded = restarted.load().await.unwrap().unwrap();
    assert_eq!(loaded.pn, saved.pn, "create keeps a paired device");
    assert_eq!(loaded.push_name, "Owner");
    assert_eq!(loaded.registration_id, saved.registration_id);
    assert_eq!(
        loaded.identity_key.public_key.public_key_bytes(),
        saved.identity_key.public_key.public_key_bytes()
    );
    assert_eq!(
        *loaded.device_props,
        *whatsapp_rust::wacore::store::device::DEVICE_PROPS,
        "runtime device props are restored on load"
    );
}

#[tokio::test]
async fn signal_rows_behave_like_the_reference_store() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let b = backend(&db, device, 1).await;

    b.put_identity("100@s.whatsapp.net", [9; 32]).await.unwrap();
    assert_eq!(
        b.load_identity("100@s.whatsapp.net").await.unwrap(),
        Some([9; 32])
    );
    b.put_session("100:2@s.whatsapp.net", b"session")
        .await
        .unwrap();
    assert!(b.has_session("100:2@s.whatsapp.net").await.unwrap());
    assert!(b.has_signal_state_for_user("100").await.unwrap());
    assert!(
        !b.has_signal_state_for_user("10").await.unwrap(),
        "a user is a whole user part, not a prefix"
    );
    b.set_sender_key_status(
        "g@g.us",
        &[("100:3@s.whatsapp.net", true), ("200@lid", false)],
    )
    .await
    .unwrap();
    let mut addresses: Vec<String> = b
        .get_signal_addresses_for_user("100")
        .await
        .unwrap()
        .iter()
        .map(|a| a.to_string())
        .collect();
    addresses.sort();
    assert_eq!(
        addresses,
        [
            "100:2@s.whatsapp.net",
            "100:3@s.whatsapp.net",
            "100@s.whatsapp.net"
        ]
    );
    let mut devices = b.get_sender_key_devices("g@g.us").await.unwrap();
    devices.sort();
    assert_eq!(
        devices,
        [
            ("100:3@s.whatsapp.net".into(), true),
            ("200@lid".into(), false)
        ]
    );
    b.delete_sender_key_device_rows(&["200@lid"]).await.unwrap();
    assert_eq!(b.get_sender_key_devices("g@g.us").await.unwrap().len(), 1);
    b.clear_sender_key_devices("g@g.us").await.unwrap();
    assert!(b.get_sender_key_devices("g@g.us").await.unwrap().is_empty());

    assert_eq!(b.get_max_prekey_id().await.unwrap(), 0);
    b.store_prekeys_batch(
        &[
            (9, Bytes::from_static(b"a")),
            (10, Bytes::from_static(b"b")),
        ],
        false,
    )
    .await
    .unwrap();
    b.store_prekey(2, b"c", false).await.unwrap();
    assert_eq!(
        b.get_max_prekey_id().await.unwrap(),
        10,
        "ids order numerically, not as text"
    );
    b.mark_prekeys_uploaded(&[9]).await.unwrap();
    let (_, _, uploaded) = b.vault.get_row(PREKEY, &id(9)).await.unwrap().unwrap();
    assert!(uploaded);
    assert_eq!(b.load_prekey(10).await.unwrap().as_deref(), Some(&b"b"[..]));
    b.remove_prekey(10).await.unwrap();
    assert_eq!(b.get_max_prekey_id().await.unwrap(), 9);

    b.save_base_key("a", "m1", b"k").await.unwrap();
    assert!(b.has_same_base_key("a", "m1", b"k").await.unwrap());
    assert_eq!(b.delete_expired_base_keys(now() - 60).await.unwrap(), 0);
    assert_eq!(b.delete_expired_base_keys(now() + 60).await.unwrap(), 1);

    b.store_sent_message("c", "m", b"payload").await.unwrap();
    assert_eq!(
        b.take_sent_message("c", "m").await.unwrap().as_deref(),
        Some(&b"payload"[..])
    );
    assert_eq!(b.take_sent_message("c", "m").await.unwrap(), None);

    let secret = [4; MESSAGE_SECRET_SIZE];
    let entry = |expires_at, message_ts| MsgSecretEntry {
        chat: Arc::from("c"),
        sender: Arc::from("s"),
        msg_id: Arc::from("m"),
        secret,
        expires_at,
        message_ts,
    };
    b.put_msg_secrets(vec![entry(100, 50)]).await.unwrap();
    b.put_msg_secrets(vec![entry(80, 0)]).await.unwrap();
    let stored = b
        .get_stored_msg_secret("c", "s", "m")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.message_ts,
        Some(50),
        "a zero time never clobbers a known one"
    );
    assert_eq!(
        b.delete_expired_msg_secrets(99).await.unwrap(),
        0,
        "the later deadline wins"
    );
    assert_eq!(b.delete_expired_msg_secrets(100).await.unwrap(), 1);

    let mapping = |phone: &str| LidPnMappingEntry {
        lid: "300".into(),
        phone_number: phone.into(),
        created_at: 1,
        updated_at: 1,
        learning_source: "usync".into(),
    };
    b.put_lid_mapping(&mapping("111")).await.unwrap();
    b.put_lid_mapping(&mapping("222")).await.unwrap();
    assert!(
        b.get_pn_mapping("111").await.unwrap().is_none(),
        "a moved number unmaps"
    );
    assert_eq!(b.get_pn_mapping("222").await.unwrap().unwrap().lid, "300");

    b.commit_patch(
        "regular",
        HashState::default(),
        &[],
        &[AppStateMutationMAC {
            index_mac: vec![1],
            value_mac: vec![2],
        }],
    )
    .await
    .unwrap();
    assert_eq!(
        b.get_mutation_mac("regular", &[1]).await.unwrap(),
        Some(vec![2])
    );
    assert!(b.get_version("regular").await.unwrap().is_some());
    b.clear_mutation_macs("regular").await.unwrap();
    assert_eq!(b.get_mutation_mac("regular", &[1]).await.unwrap(), None);

    b.set_sync_key(&[5, 6], AppStateSyncKey::default())
        .await
        .unwrap();
    assert_eq!(b.get_latest_sync_key_id().await.unwrap(), Some(vec![5, 6]));
}

#[tokio::test]
async fn concurrent_token_merges_keep_every_input() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let b = backend(&db, device, 1).await;
    let touches = (0..16).map(|i| b.touch_tc_token_sender_timestamp("u", 5000 + i));
    let tokens = (0..16).map(|i| {
        let token = vec![u8::try_from(i).unwrap()];
        let b = &b;
        async move { b.store_received_tc_token("u", &token, 4000 + i).await }
    });
    let (touched, stored) = tokio::join!(
        futures_util::future::join_all(touches),
        futures_util::future::join_all(tokens.rev())
    );
    touched.into_iter().chain(stored).for_each(|r| r.unwrap());
    let entry = b.get_tc_token("u").await.unwrap().unwrap();
    assert_eq!(
        entry.sender_timestamp,
        Some(5015),
        "a sender timestamp never regresses"
    );
    assert_eq!(entry.token, vec![15], "the newest token wins");
    assert_eq!(entry.token_timestamp, 4015);

    let secret = [4; MESSAGE_SECRET_SIZE];
    let entry = |expires_at, message_ts| MsgSecretEntry {
        chat: Arc::from("c"),
        sender: Arc::from("s"),
        msg_id: Arc::from("m"),
        secret,
        expires_at,
        message_ts,
    };
    let (first, second) = tokio::join!(
        b.put_msg_secrets(vec![entry(100, 50)]),
        b.put_msg_secrets(vec![entry(200, 0)])
    );
    first.unwrap();
    second.unwrap();
    let stored = b
        .get_stored_msg_secret("c", "s", "m")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.message_ts, Some(50), "the known time survives");
    assert_eq!(
        b.delete_expired_msg_secrets(199).await.unwrap(),
        0,
        "the later deadline survives"
    );
}

#[tokio::test]
async fn another_key_cannot_open_the_store() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let owner = backend(&db, device, 1).await;
    owner
        .put_session("100@s.whatsapp.net", b"session")
        .await
        .unwrap();
    let stranger = backend(&db, device, 2).await;
    assert!(
        stranger.get_session("100@s.whatsapp.net").await.is_err(),
        "a wrong key fails instead of returning garbage"
    );
}

#[tokio::test]
async fn stored_values_are_sealed_at_rest() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let b = backend(&db, device, 3).await;
    let marker = b"plaintext-marker-5f1c";
    b.put_session("100@s.whatsapp.net", marker).await.unwrap();
    b.put_sender_key("g@g.us", marker).await.unwrap();
    let values: Vec<Vec<u8>> = sqlx::query_scalar("SELECT value FROM social_device_state")
        .fetch_all(&db)
        .await
        .unwrap();
    assert_eq!(values.len(), 2);
    for value in values {
        assert!(!value.windows(marker.len()).any(|w| w == marker));
    }
}

fn lease() -> PendingInboundLease {
    PendingInboundLease::new(Arc::new(()))
}

fn payload(bytes: &'static [u8]) -> PendingInboundPayload {
    PendingInboundPayload::new(Bytes::from_static(bytes), lease())
}

const KEY: PendingInboundKey<'static> = PendingInboundKey {
    chat: "1@g.us",
    sender: "2:3@s.whatsapp.net",
    id: "M1",
};

#[tokio::test]
async fn a_pending_message_is_kept_until_deleted_and_expires_by_age() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let b = backend(&db, device, 7).await;
    b.store_pending_inbound("1@g.us", "2@s.whatsapp.net", "M1", b"one")
        .await
        .unwrap();
    b.store_pending_inbound("1@g.us", "2@s.whatsapp.net", "M2", b"two")
        .await
        .unwrap();
    assert_eq!(
        b.get_pending_inbound("1@g.us", "2@s.whatsapp.net", "M1")
            .await
            .unwrap()
            .as_deref(),
        Some(&b"one"[..])
    );
    b.delete_pending_inbound("1@g.us", "2@s.whatsapp.net", "M1")
        .await
        .unwrap();
    assert!(
        b.get_pending_inbound("1@g.us", "2@s.whatsapp.net", "M1")
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        b.delete_expired_pending_inbound(now() - 60).await.unwrap(),
        0
    );
    assert_eq!(
        b.delete_expired_pending_inbound(now() + 60).await.unwrap(),
        1
    );
    assert!(
        b.get_pending_inbound("1@g.us", "2@s.whatsapp.net", "M2")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_pending_row_changes_only_from_the_revision_its_owner_saw() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let b = backend(&db, device, 7).await;
    let missing = PendingInboundRevision::missing();
    assert_eq!(
        b.delete_pending_inbound_if(KEY, missing, 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Applied,
        "deleting an absent row that was expected absent is a no-op"
    );
    assert_eq!(
        b.replace_pending_inbound_if(KEY, missing, payload(b"first"), 16)
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    assert_eq!(
        b.replace_pending_inbound_if(KEY, missing, payload(b"first"), 16)
            .await
            .unwrap(),
        PendingInboundMutation::Applied,
        "installing the same bytes again is idempotent"
    );
    assert_eq!(
        b.replace_pending_inbound_if(KEY, missing, payload(b"other"), 16)
            .await
            .unwrap(),
        PendingInboundMutation::Conflict,
        "a stale expected revision is refused"
    );
    let stored = b
        .read_pending_inbound_bounded(KEY, 16, lease())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.bytes(), b"first");
    assert!(
        b.read_pending_inbound_bounded(KEY, 2, lease())
            .await
            .is_err(),
        "a row over the bound is an error, not absence"
    );
    assert_eq!(
        b.delete_pending_inbound_if(KEY, missing, 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Conflict,
        "a present row is not deleted as if absent"
    );
    assert_eq!(
        b.delete_pending_inbound_if(KEY, stored.revision(), 16, lease())
            .await
            .unwrap(),
        PendingInboundMutation::Applied
    );
    assert!(
        b.read_pending_inbound_bounded(KEY, 16, lease())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_group_message_is_found_by_its_senders_device_alias() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let b = backend(&db, device, 7).await;
    b.replace_pending_inbound_if(KEY, PendingInboundRevision::missing(), payload(b"hi"), 16)
        .await
        .unwrap();
    let alias = PendingInboundKey {
        sender: "2@s.whatsapp.net",
        ..KEY
    };
    let (sender, record) = b
        .read_pending_inbound_group_bounded(alias, 16, lease())
        .await
        .unwrap()
        .expect("the device spelling matches its user");
    assert_eq!(sender, KEY.sender);
    assert_eq!(record.bytes(), b"hi");
    let other = PendingInboundKey {
        sender: "9@s.whatsapp.net",
        ..KEY
    };
    assert!(
        b.read_pending_inbound_group_bounded(other, 16, lease())
            .await
            .unwrap()
            .is_none()
    );
    b.replace_pending_inbound_if(
        PendingInboundKey {
            sender: "2:4@s.whatsapp.net",
            ..KEY
        },
        PendingInboundRevision::missing(),
        payload(b"again"),
        16,
    )
    .await
    .unwrap();
    assert!(
        b.read_pending_inbound_group_bounded(alias, 16, lease())
            .await
            .is_err(),
        "two device spellings of one sender are ambiguous"
    );
}

#[tokio::test]
async fn a_captured_history_chunk_stays_until_forgotten() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let b = backend(&db, device, 7).await;
    b.capture_history("H1", b"zlib").await.unwrap();
    b.capture_history("H1", b"zlib").await.unwrap();
    assert_eq!(
        b.history().await.unwrap(),
        vec![("H1".into(), b"zlib".to_vec())]
    );
    b.forget_history("H1").await.unwrap();
    assert!(b.history().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_group_sender_longer_than_its_bound_is_refused_unread() {
    let db = pool().await;
    let device = store::new_device(&db).await.unwrap();
    let b = backend(&db, device, 7).await;
    let long = format!("{}@s.whatsapp.net", "2".repeat(SENDER_BYTES));
    b.replace_pending_inbound_if(
        PendingInboundKey {
            sender: &long,
            ..KEY
        },
        PendingInboundRevision::missing(),
        payload(b"hi"),
        16,
    )
    .await
    .unwrap();
    assert!(
        b.read_pending_inbound_group_bounded(KEY, 16, lease())
            .await
            .is_err(),
        "an oversized sender is an error, never absence"
    );
}
