mod common;

use async_trait::async_trait;
use common::TestDb;
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
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn key() -> SecretKey {
    SecretKey::parse("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap()
}
fn token() -> Token {
    Token::parse("private-native-social-test-token".into()).unwrap()
}
fn watch(c: Uuid, destination: Option<&str>, wake: Wake) -> TriggerInput {
    TriggerInput {
        connection_id: c,
        destination: destination.map(str::to_owned),
        wake,
        instructions: "Summarize what the team asks and draft replies for my approval.".into(),
    }
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
        &token(),
        "Use this bot only for team support.",
    )
    .await
    .unwrap()
}
async fn bot(db: &TestDb) -> Uuid {
    let bot = Uuid::new_v4();
    sqlx::query("INSERT INTO bots(id,name) VALUES(?1,'Social test')")
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id) VALUES(?1,?2)")
        .bind(Uuid::new_v4())
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    bot
}
async fn turn(db: &TestDb, bot: Uuid, kind: &str) -> Uuid {
    let (conversation, last, seq): (Uuid, i64, i64) =
        sqlx::query_as("SELECT id,last_turn_no,last_item_seq FROM conversations WHERE bot_id=?1")
            .bind(bot)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let id = Uuid::new_v4();
    let item = Uuid::new_v4();
    sqlx::query("INSERT INTO turns(id,bot_id,conversation_id,request_id,request_hash,turn_no,owner_item_id,provider_id,status,attempt) VALUES(?1,?2,?3,?4,zeroblob(32),?5,?6,'synthetic','running',1)").bind(id).bind(bot).bind(conversation).bind(Uuid::new_v4()).bind(last+1).bind(item).execute(&db.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO items(id,conversation_id,seq,turn_id,kind,text) VALUES(?1,?2,?3,?4,?5,'test')",
    )
    .bind(item)
    .bind(conversation)
    .bind(seq + 1)
    .bind(id)
    .bind(kind)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE conversations SET last_turn_no=?2,last_item_seq=?3 WHERE id=?1")
        .bind(conversation)
        .bind(last + 1)
        .bind(seq + 1)
        .execute(&db.pool)
        .await
        .unwrap();
    id
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

#[tokio::test]
async fn tokens_are_encrypted_identity_bound_and_never_public() {
    let db = TestDb::migrated().await;
    let c = account(&db, "first").await;
    let other = account(&db, "second").await;
    assert_eq!(
        store::credential(&db.pool, &key(), &c)
            .await
            .unwrap()
            .0
            .as_str(),
        token().0.as_str()
    );
    let cipher: Vec<u8> =
        sqlx::query_scalar("SELECT credential FROM social_connections WHERE id=?1")
            .bind(c.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(!String::from_utf8_lossy(&cipher).contains(token().0.as_str()));
    assert!(
        !serde_json::to_string(&c)
            .unwrap()
            .contains(token().0.as_str())
    );
    sqlx::query("UPDATE social_connections SET credential=(SELECT credential FROM social_connections WHERE id=?1),credential_salt=(SELECT credential_salt FROM social_connections WHERE id=?1) WHERE id=?2").bind(c.id).bind(other.id).execute(&db.pool).await.unwrap();
    assert!(matches!(
        store::credential(&db.pool, &key(), &other).await,
        Err(Error::Crypto)
    ));
    assert!(matches!(
        store::create(
            &db.pool,
            &key(),
            Provider::Telegram,
            &Identity {
                id: "first".into(),
                display_name: "x".into(),
                username: None
            },
            &token(),
            ""
        )
        .await,
        Err(Error::Duplicate)
    ));
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

async fn authorize(
    db: &TestDb,
    c: &Connection,
    bot: Uuid,
    turn: Uuid,
    chat: Option<&str>,
    write: bool,
) -> Result<Access, Error> {
    store::authorize(
        &mut db.pool.acquire().await.unwrap(),
        c,
        bot,
        turn,
        chat,
        write,
        false,
    )
    .await
}

#[tokio::test]
async fn owner_work_uses_any_chat_and_a_woken_turn_reads_only_its_own() {
    let db = TestDb::migrated().await;
    let c = account(&db, "scope").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let owner = turn(&db, bot, "user").await;
    let trigger = store::set_trigger(&db.pool, owner, &watch(c.id, Some("100"), Wake::All))
        .await
        .unwrap()
        .value;
    let c = store::connection(&db.pool, c.id).await.unwrap();
    assert_eq!(
        authorize(&db, &c, bot, owner, None, false).await.unwrap(),
        Access::Owner
    );
    authorize(&db, &c, bot, owner, Some("200"), true)
        .await
        .expect("owner work edits any chat");
    authorize(&db, &c, bot, owner, Some("not-seen-yet"), false)
        .await
        .expect("owner work may name a chat the bot has not seen");
    sqlx::query("UPDATE turns SET status='completed' WHERE id=?1")
        .bind(owner)
        .execute(&db.pool)
        .await
        .unwrap();
    let external = turn(&db, bot, "connector").await;
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,trigger_id,destination,message_id,status,turn_id) VALUES(?1,'e',?2,?3,'100','1','delivered',?4)").bind(c.id).bind(bot).bind(trigger.id).bind(external).execute(&db.pool).await.unwrap();
    assert_eq!(
        authorize(&db, &c, bot, external, None, false)
            .await
            .unwrap(),
        Access::Woken(vec!["100".into()])
    );
    authorize(&db, &c, bot, external, Some("100"), false)
        .await
        .unwrap();
    assert!(
        authorize(&db, &c, bot, external, Some("200"), false)
            .await
            .is_err()
    );
    assert!(
        authorize(&db, &c, bot, external, Some("100"), true)
            .await
            .is_err(),
        "a woken turn only reads"
    );
}

#[tokio::test]
async fn setup_needs_only_its_provider_connection_and_requeues() {
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let request = store::request_setup(&db.pool, turn, Provider::Telegram, "Team support")
        .await
        .unwrap();
    let c = account(&db, "setup").await;
    sqlx::query("UPDATE turns SET status='waiting' WHERE id=?1")
        .bind(turn)
        .execute(&db.pool)
        .await
        .unwrap();
    let done = store::settle_setup(&db.pool, request.id, Some(c.id))
        .await
        .unwrap();
    assert_eq!(done.status, "connected");
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?1")
        .bind(turn)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "queued");
    assert_eq!(
        store::settle_setup(&db.pool, request.id, Some(c.id))
            .await
            .unwrap()
            .status,
        "connected",
        "a retry of the same completion changes nothing"
    );
    assert!(
        store::settle_setup(&db.pool, request.id, None)
            .await
            .is_err(),
        "a settled setup cannot be cancelled"
    );
}

#[tokio::test]
async fn an_external_turn_sees_no_connection_it_was_not_delivered_from() {
    let db = TestDb::migrated().await;
    let c = account(&db, "unrelated").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let external = turn(&db, bot, "connector").await;
    let c = store::connection(&db.pool, c.id).await.unwrap();
    assert!(
        authorize(&db, &c, bot, external, None, false)
            .await
            .is_err(),
        "an incoming message elsewhere cannot read this connection"
    );
}

#[tokio::test]
async fn only_discord_intent_changes_restart_the_listener() {
    let db = TestDb::migrated().await;
    let discord = account(&db, "intents").await;
    sqlx::query("UPDATE social_connections SET provider='discord' WHERE id=?1")
        .bind(discord.id)
        .execute(&db.pool)
        .await
        .unwrap();
    let telegram = account(&db, "no-intents").await;
    let bot = bot(&db).await;
    let owner = turn(&db, bot, "user").await;
    let content = |id: Uuid| {
        let pool = db.pool.clone();
        async move { store::connection(&pool, id).await.unwrap().message_content }
    };
    let mention = store::set_trigger(&db.pool, owner, &watch(discord.id, None, Wake::Mentions))
        .await
        .unwrap();
    assert!(!mention.reconnect, "a mention needs no message content");
    assert!(!content(discord.id).await);
    let all = store::set_trigger(&db.pool, owner, &watch(discord.id, None, Wake::All))
        .await
        .unwrap();
    assert!(all.reconnect && content(discord.id).await);
    let same = store::set_trigger(&db.pool, owner, &watch(discord.id, Some("100"), Wake::All))
        .await
        .unwrap();
    assert!(!same.reconnect, "already receiving message content");
    store::delete_trigger(&db.pool, bot, same.value.id)
        .await
        .unwrap();
    let paused = store::set_trigger_enabled(&db.pool, bot, all.value.id, false)
        .await
        .unwrap();
    assert!(paused.reconnect && !content(discord.id).await);
    let deleted = store::delete_trigger(&db.pool, bot, all.value.id)
        .await
        .unwrap();
    assert!(!deleted.reconnect, "a paused trigger asked for nothing");
    let telegram_all = store::set_trigger(&db.pool, owner, &watch(telegram.id, None, Wake::All))
        .await
        .unwrap();
    assert!(
        !telegram_all.reconnect,
        "Telegram receives every message already, so in-flight actions continue"
    );
}

#[tokio::test]
async fn a_bot_sets_its_own_triggers_only_for_owner_work_and_one_per_chat() {
    let db = TestDb::migrated().await;
    let c = account(&db, "triggers").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let external = turn(&db, bot, "connector").await;
    assert!(
        store::set_trigger(&db.pool, external, &watch(c.id, Some("100"), Wake::All))
            .await
            .is_err(),
        "an incoming message cannot set a trigger"
    );
    sqlx::query("UPDATE turns SET status='completed' WHERE id=?1")
        .bind(external)
        .execute(&db.pool)
        .await
        .unwrap();
    let owner = turn(&db, bot, "user").await;
    let first = store::set_trigger(&db.pool, owner, &watch(c.id, Some("100"), Wake::All))
        .await
        .unwrap()
        .value;
    assert_eq!(first.destination_name.as_deref(), Some("Team"));
    let mut replaced = watch(c.id, Some("100"), Wake::Mentions);
    replaced.instructions = "Only answer release questions.".into();
    let second = store::set_trigger(&db.pool, owner, &replaced)
        .await
        .unwrap()
        .value;
    assert_eq!(second.id, first.id, "the same chat replaces its trigger");
    assert_eq!(
        (second.wake, second.instructions.as_str()),
        (Wake::Mentions, "Only answer release questions.")
    );
    let any = store::set_trigger(&db.pool, owner, &watch(c.id, None, Wake::Mentions))
        .await
        .unwrap()
        .value;
    assert_ne!(any.id, first.id);
    let listed = store::triggers(&mut db.pool.acquire().await.unwrap(), bot)
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(
        store::delete_trigger(&db.pool, bot, any.id)
            .await
            .unwrap()
            .connection,
        c.id
    );
    assert!(
        store::delete_trigger(&db.pool, Uuid::new_v4(), first.id)
            .await
            .is_err(),
        "only its bot deletes it"
    );
}

#[tokio::test]
async fn action_receipts_reject_changed_retries_and_survive_crash_as_uncertain() {
    let db = TestDb::migrated().await;
    let c = account(&db, "actions").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let c = store::connection(&db.pool, c.id).await.unwrap();
    let id = Uuid::new_v4();
    let payload = json!({"operation":"delete","destination":"100","message_id":"1"});
    assert!(
        store::begin_action(
            &db.pool,
            &c,
            store::Action {
                bot,
                turn,
                id,
                destination: "100",
                payload: &payload,
                send: false
            }
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(
        store::begin_action(
            &db.pool,
            &c,
            store::Action {
                bot,
                turn,
                id,
                destination: "100",
                payload: &payload,
                send: false
            }
        )
        .await
        .unwrap()
        .unwrap()["status"],
        "dispatching"
    );
    assert!(matches!(
        store::begin_action(
            &db.pool,
            &c,
            store::Action {
                bot,
                turn,
                id,
                destination: "100",
                payload: &json!({"different":true}),
                send: false,
            }
        )
        .await,
        Err(Error::Conflict)
    ));
    store::recover(&db.pool).await.unwrap();
    assert_eq!(
        store::begin_action(
            &db.pool,
            &c,
            store::Action {
                bot,
                turn,
                id,
                destination: "100",
                payload: &payload,
                send: false
            }
        )
        .await
        .unwrap()
        .unwrap()["status"],
        "uncertain"
    );
}

#[tokio::test]
async fn incoming_delivery_is_deduplicated_and_telegram_history_pages_numerically() {
    let db = TestDb::migrated().await;
    let c = account(&db, "history").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    trigger_row(&db, c.id, bot, Some("100"), "mentions").await;
    let c = store::connection(&db.pool, c.id).await.unwrap();
    for id in 1..=11 {
        let event = Incoming {
            event_id: id.to_string(),
            destination: Destination {
                id: "100".into(),
                name: "Team".into(),
                kind: "group".into(),
                can_send: true,
            },
            message_id: id.to_string(),
            author_id: "human".into(),
            body: format!("message {id}"),
            mentioned: true,
            direct: false,
            from_bot: false,
        };
        store::record(&db.pool, &c, &event, id + 1).await.unwrap();
        store::record(&db.pool, &c, &event, id + 1).await.unwrap();
    }
    let history = store::local_messages(&db.pool, c.id, "100", Some("10"), None, 100)
        .await
        .unwrap();
    assert_eq!(history["messages"].as_array().unwrap().len(), 9);
    assert_eq!(history["messages"][0]["id"], "9");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM social_deliveries")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 11);
    let registry = else_api::providers::Registry::new(vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )]);
    assert_eq!(
        else_api::socials::delivery::admit_pending(&db.pool, &registry, 1)
            .await
            .unwrap(),
        1
    );
    let turn: Uuid = sqlx::query_scalar(
        "SELECT turn_id FROM social_deliveries WHERE status='delivered' LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        else_api::db::delegations::origin(&mut db.pool.acquire().await.unwrap(), turn)
            .await
            .unwrap(),
        else_api::app::providers::Origin::SocialEvent
    );
}

#[derive(Default)]
struct Fake {
    started: Notify,
    release: Notify,
    blocked: AtomicBool,
    sends: AtomicUsize,
    sent_to: std::sync::Mutex<Vec<String>>,
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
    ) -> Result<Value, Error> {
        self.started.notify_one();
        if self.blocked.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        self.sends.fetch_add(1, Ordering::SeqCst);
        self.sent_to.lock().unwrap().push(destination.to_owned());
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

async fn health_after_listening(refused: bool) -> String {
    let db = TestDb::migrated().await;
    let c = account(&db, "listener").await;
    let fake = Arc::new(Fake::default());
    fake.refused.store(refused, Ordering::SeqCst);
    let manager = Arc::new(Manager::with_transport(
        db.pool.clone(),
        Arc::new(key()),
        fake,
    ));
    manager.changed(c.id).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    store::connection(&db.pool, c.id).await.unwrap().health
}

#[tokio::test]
async fn a_bot_whose_updates_go_elsewhere_shows_the_conflict_instead_of_connected() {
    assert_eq!(health_after_listening(true).await, "receiver_conflict");
}

#[tokio::test]
async fn a_listener_that_stays_open_is_connected() {
    assert_eq!(health_after_listening(false).await, "connected");
}

#[tokio::test]
async fn changing_generation_cancels_inflight_sdk_wait_and_records_uncertainty() {
    let db = TestDb::migrated().await;
    let c = account(&db, "cancel").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let fake = Arc::new(Fake::default());
    fake.blocked.store(true, Ordering::SeqCst);
    let manager = Arc::new(Manager::with_transport(
        db.pool.clone(),
        Arc::new(key()),
        fake.clone(),
    ));
    let id = Uuid::new_v4();
    let running = manager.clone();
    let task = tokio::spawn(async move {
        running
            .send(
                c.id,
                bot,
                turn,
                id,
                SendMessage {
                    destination: "100".into(),
                    body: "approved body".into(),
                    reply_to: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), fake.started.notified())
        .await
        .unwrap();
    store::set_state(&db.pool, c.id, "paused").await.unwrap();
    manager.changed(c.id).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    fake.release.notify_one();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
    let status: String = sqlx::query_scalar("SELECT status FROM social_actions WHERE action_id=?1")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "uncertain");
}

#[tokio::test]
async fn stopped_turn_cancels_sdk_wait_without_connection_change() {
    let db = TestDb::migrated().await;
    let c = account(&db, "stop").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let fake = Arc::new(Fake::default());
    fake.blocked.store(true, Ordering::SeqCst);
    let manager = Arc::new(Manager::with_transport(
        db.pool.clone(),
        Arc::new(key()),
        fake.clone(),
    ));
    let running = manager.clone();
    let id = Uuid::new_v4();
    let task = tokio::spawn(async move {
        running
            .send(
                c.id,
                bot,
                turn,
                id,
                SendMessage {
                    destination: "100".into(),
                    body: "body".into(),
                    reply_to: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), fake.started.notified())
        .await
        .unwrap();
    sqlx::query("UPDATE turns SET status='cancelling' WHERE id=?1")
        .bind(turn)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    fake.release.notify_one();
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn restored_connections_are_passive_and_credentials_are_removed_on_disconnect() {
    let db = TestDb::migrated().await;
    let c = account(&db, "restore").await;
    store::restore_passive(&mut db.pool.acquire().await.unwrap())
        .await
        .unwrap();
    let restored = store::connection(&db.pool, c.id).await.unwrap();
    assert_eq!(restored.state, "paused");
    assert!(restored.generation > c.generation);
    store::set_state(&db.pool, c.id, "removed").await.unwrap();
    let secret: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT credential FROM social_connections WHERE id=?1")
            .bind(c.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(secret.is_none());
}

struct TestEnv;
impl else_api::config::Source for TestEnv {
    fn get(&self, name: &str) -> Option<String> {
        (name == "HOME").then(|| "/tmp".into())
    }
}

async fn http(
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
async fn rpc(router: &axum::Router, bearer: &str, name: &str, args: Value) -> Value {
    let(status,body)=http(router,"POST","/tools/v1/mcp",bearer,json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}})).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_ne!(body["result"]["isError"], true, "{body}");
    serde_json::from_str(body["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn owner_credentials_task_authority_and_approved_native_send_stay_separate() {
    use else_api::{
        credentials::OwnerToken, grants::Tool, http::AppState, providers::Registry,
        scheduler::Scheduler,
    };
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(Manager::with_transport(
        db.pool.clone(),
        Arc::new(key()),
        fake.clone(),
    ));
    let scheduler = Scheduler::new(db.pool.clone(), Registry::new(vec![]), 1);
    let stop = CancellationToken::new();
    let state = Arc::new(
        AppState::new(
            OwnerToken::parse(common::TOKEN).unwrap(),
            db.pool.clone(),
            scheduler.clone(),
            else_api::events::EventHub::start(db.pool.clone(), stop.clone()),
        )
        .with_secret_key(Arc::new(key()))
        .with_socials(manager),
    );
    let router = else_api::http::router(&else_api::config::Config::load(&TestEnv).unwrap(), state);
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?1")
        .bind(turn)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let grant = scheduler
        .grants()
        .issue(
            bot,
            conversation,
            turn,
            vec![Tool::Social, Tool::AskOwner],
            Duration::from_secs(300),
        )
        .await;
    let denied = http(
        &router,
        "GET",
        "/v1/connectors/socials/connections",
        &grant,
        Value::Null,
    )
    .await;
    assert_eq!(denied.0, axum::http::StatusCode::FORBIDDEN);
    let rejected = http(
        &router,
        "POST",
        "/v1/connectors/socials/connections",
        common::TOKEN,
        json!({"provider":"telegram","token":"NEVER-ECHO-THIS-CANARY","extra":true}),
    )
    .await;
    assert_eq!(rejected.0, axum::http::StatusCode::BAD_REQUEST);
    assert!(!rejected.1.to_string().contains("NEVER-ECHO-THIS-CANARY"));
    let(status,created)=http(&router,"POST","/v1/connectors/socials/connections",common::TOKEN,json!({"provider":"telegram","token":"private-owner-bot-token","instructions":"Team support only"})).await;
    assert_eq!(status, axum::http::StatusCode::OK, "{created}");
    assert!(!created.to_string().contains("private-owner-bot-token"));
    let connection: Uuid = created["connection"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    destinations(&db, connection).await;
    let discovery = rpc(&router, &grant, "social_connections", json!({})).await;
    assert_eq!(
        discovery["connections"][0]["destinations"][0]["id"], "100",
        "any bot working for the owner sees the connection's chats: {discovery}"
    );
    let proposed=rpc(&router,&grant,"draft_message",json!({"platform":"telegram","account_id":connection,"destination":"100","body":"Original draft","reply_to":"10"})).await;
    let draft = proposed["draft"]["draft_id"].as_str().unwrap();
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
    let (status, approved) = http(
        &router,
        "POST",
        &format!("/v1/bots/{bot}/outgoing-drafts/{draft}/approve"),
        common::TOKEN,
        json!({"version":1,"body":"The owner's exact approved revision"}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{approved}");
    sqlx::query("UPDATE turns SET attempt=2 WHERE id=?1")
        .bind(turn)
        .execute(&db.pool)
        .await
        .unwrap();
    let grant = scheduler
        .grants()
        .issue(
            bot,
            conversation,
            turn,
            vec![Tool::Social, Tool::AskOwner],
            Duration::from_secs(300),
        )
        .await;
    let result = rpc(&router, &grant, "send_draft", json!({"draft_id":draft})).await;
    assert_eq!(result["status"], "sent", "{result}");
    assert_eq!(
        rpc(&router, &grant, "send_draft", json!({"draft_id":draft})).await["status"],
        "sent"
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    let body: String = sqlx::query_scalar(
        "SELECT json_extract(result,'$.body') FROM social_actions WHERE connection_id=?1",
    )
    .bind(connection)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(body, "The owner's exact approved revision");
    stop.cancel();
}

#[tokio::test]
async fn a_public_telegram_username_works_wherever_a_chat_id_does() {
    use else_api::{
        credentials::OwnerToken, grants::Tool, http::AppState, providers::Registry,
        scheduler::Scheduler,
    };
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(Manager::with_transport(
        db.pool.clone(),
        Arc::new(key()),
        fake.clone(),
    ));
    let scheduler = Scheduler::new(db.pool.clone(), Registry::new(vec![]), 1);
    let stop = CancellationToken::new();
    let state = Arc::new(
        AppState::new(
            OwnerToken::parse(common::TOKEN).unwrap(),
            db.pool.clone(),
            scheduler.clone(),
            else_api::events::EventHub::start(db.pool.clone(), stop.clone()),
        )
        .with_secret_key(Arc::new(key()))
        .with_socials(manager),
    );
    let router = else_api::http::router(&else_api::config::Config::load(&TestEnv).unwrap(), state);
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id=?1")
        .bind(turn)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let issue = || {
        scheduler.grants().issue(
            bot,
            conversation,
            turn,
            vec![Tool::Social, Tool::Routines, Tool::AskOwner],
            Duration::from_secs(300),
        )
    };
    let grant = issue().await;
    let (status, created) = http(
        &router,
        "POST",
        "/v1/connectors/socials/connections",
        common::TOKEN,
        json!({"provider":"telegram","token":"private-owner-bot-token"}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{created}");
    let connection = created["connection"]["id"].as_str().unwrap().to_owned();

    let set = rpc(
        &router,
        &grant,
        "social_trigger_set",
        json!({"connection_id":connection,"destination":"@team_alerts","wake":"mentions","instructions":"Summarize alerts for me."}),
    )
    .await;
    assert_eq!(
        (
            set["trigger"]["destination"].as_str(),
            set["trigger"]["destination_name"].as_str()
        ),
        (Some("-100777"), Some("Team alerts")),
        "a trigger keys on the chat ID incoming messages carry: {set}"
    );
    let discovery = rpc(&router, &grant, "social_connections", json!({})).await;
    assert_eq!(
        discovery["connections"][0]["destinations"][0],
        json!({"id":"-100777","name":"Team alerts","kind":"group","can_send":true}),
        "a looked-up chat is listed before it posts anything: {discovery}"
    );

    let proposed = rpc(
        &router,
        &grant,
        "draft_message",
        json!({"platform":"telegram","account_id":connection,"destination":"https://t.me/team_alerts","body":"Test"}),
    )
    .await;
    let draft = proposed["draft"]["draft_id"].as_str().unwrap();
    assert_eq!(
        proposed["draft"]["content"]["destination"], "-100777",
        "the owner approves the chat the username named when drafted: {proposed}"
    );
    let (status, approved) = http(
        &router,
        "POST",
        &format!("/v1/bots/{bot}/outgoing-drafts/{draft}/approve"),
        common::TOKEN,
        json!({"version":1}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{approved}");
    sqlx::query("UPDATE turns SET attempt=2 WHERE id=?1")
        .bind(turn)
        .execute(&db.pool)
        .await
        .unwrap();
    let grant = issue().await;
    let result = rpc(&router, &grant, "send_draft", json!({"draft_id":draft})).await;
    assert_eq!(result["status"], "sent", "{result}");
    assert_eq!(*fake.sent_to.lock().unwrap(), vec!["-100777".to_owned()]);

    sqlx::query("UPDATE turns SET status='completed' WHERE id=?1")
        .bind(turn)
        .execute(&db.pool)
        .await
        .unwrap();
    let woken = crate::turn(&db, bot, "connector").await;
    let grant = scheduler
        .grants()
        .issue(
            bot,
            conversation,
            woken,
            vec![Tool::Social],
            Duration::from_secs(300),
        )
        .await;
    let (_, refused) = http(&router, "POST", "/tools/v1/mcp", &grant, json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"social_action","arguments":{"connection_id":connection,"command":{"operation":"chat","destination":"@other_channel"}}}})).await;
    assert_eq!(refused["result"]["isError"], true, "{refused}");
    assert!(
        refused.to_string().contains("no permission"),
        "a woken turn cannot look up another chat: {refused}"
    );
    stop.cancel();
}

#[tokio::test]
async fn cancelled_setup_cannot_complete_or_touch_pending_deliveries() {
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let c = account(&db, "cancelled-setup").await;
    destinations(&db, c.id).await;
    let trigger = trigger_row(&db, c.id, bot, Some("100"), "all").await;
    let c = store::connection(&db.pool, c.id).await.unwrap();
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,trigger_id,destination,message_id) VALUES(?1,'pending',?2,?3,'100','1')")
        .bind(c.id).bind(bot).bind(trigger).execute(&db.pool).await.unwrap();
    let request = store::request_setup(&db.pool, turn, Provider::Telegram, "Connect again")
        .await
        .unwrap();
    store::settle_setup(&db.pool, request.id, None)
        .await
        .unwrap();
    assert!(
        store::settle_setup(&db.pool, request.id, Some(c.id))
            .await
            .is_err()
    );
    assert_eq!(
        store::connection(&db.pool, c.id).await.unwrap().generation,
        c.generation
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM social_deliveries WHERE event_id='pending'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn retention_expires_pending_deliveries_and_recovery_admits_remaining_messages() {
    let db = TestDb::migrated().await;
    let c = account(&db, "retention").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let trigger = trigger_row(&db, c.id, bot, Some("100"), "mentions").await;
    let c = store::connection(&db.pool, c.id).await.unwrap();
    sqlx::query("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000) INSERT INTO social_messages(connection_id,destination,message_id,author_id,body,event_id,created_at) SELECT ?1,'100',CAST(x AS TEXT),'human','backlog',CAST(x AS TEXT),printf('2020-01-01T00:%05d',x) FROM n")
        .bind(c.id).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,trigger_id,destination,message_id) SELECT connection_id,event_id,?2,?3,destination,message_id FROM social_messages WHERE connection_id=?1")
        .bind(c.id).bind(bot).bind(trigger).execute(&db.pool).await.unwrap();
    let unavailable = else_api::providers::Registry::new(vec![]);
    assert_eq!(
        else_api::socials::delivery::admit_pending(&db.pool, &unavailable, 1)
            .await
            .unwrap(),
        0
    );
    let event = Incoming {
        event_id: "new".into(),
        destination: Destination {
            id: "100".into(),
            name: "Team".into(),
            kind: "group".into(),
            can_send: true,
        },
        message_id: "5001".into(),
        author_id: "human".into(),
        body: "New message".into(),
        mentioned: true,
        direct: false,
        from_bot: false,
    };
    store::record(&db.pool, &c, &event, 5002).await.unwrap();
    let orphan_pending: i64 = sqlx::query_scalar("SELECT count(*) FROM social_deliveries d WHERE d.status='pending' AND NOT EXISTS(SELECT 1 FROM social_messages m WHERE m.connection_id=d.connection_id AND m.event_id=d.event_id)")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(orphan_pending, 0);
    let expired: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM social_deliveries WHERE status='expired' AND settled_at IS NOT NULL",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(expired, 1);
    let registry = else_api::providers::Registry::new(vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )]);
    assert_eq!(
        else_api::socials::delivery::admit_pending(&db.pool, &registry, 1)
            .await
            .unwrap(),
        1
    );
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
    }
}

#[tokio::test]
async fn a_message_wakes_only_bots_whose_enabled_trigger_matches_it() {
    let db = TestDb::migrated().await;
    let c = account(&db, "wake").await;
    let (watcher, mentions, paused) = (bot(&db).await, bot(&db).await, bot(&db).await);
    trigger_row(&db, c.id, watcher, Some("100"), "all").await;
    trigger_row(&db, c.id, watcher, None, "all").await;
    trigger_row(&db, c.id, mentions, None, "mentions").await;
    let off = trigger_row(&db, c.id, paused, Some("100"), "all").await;
    store::set_trigger_enabled(&db.pool, paused, off, false)
        .await
        .unwrap();
    let c = store::connection(&db.pool, c.id).await.unwrap();
    store::record(&db.pool, &c, &incoming("plain", "100", false, false), 1)
        .await
        .unwrap();
    store::record(&db.pool, &c, &incoming("mention", "200", true, false), 2)
        .await
        .unwrap();
    store::record(&db.pool, &c, &incoming("echo", "100", true, true), 3)
        .await
        .unwrap();
    let woken: Vec<(String, Uuid, Option<String>)> = sqlx::query_as(
        "SELECT d.event_id, d.bot_id, t.destination FROM social_deliveries d
         JOIN social_triggers t ON t.id = d.trigger_id ORDER BY d.event_id, d.bot_id",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let mut expected = vec![
        ("mention".to_owned(), mentions, None),
        ("mention".to_owned(), watcher, None),
        ("plain".to_owned(), watcher, Some("100".to_owned())),
    ];
    expected.sort();
    assert_eq!(
        woken, expected,
        "paused triggers and the bot's own messages wake nobody"
    );
}

#[tokio::test]
async fn upgrading_turns_wake_access_into_triggers_and_drops_bindings() {
    use std::borrow::Cow;
    let db = TestDb::empty().await;
    let before = sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            else_api::db::MIGRATOR
                .migrations
                .iter()
                .filter(|m| m.version < 33)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before.run(&db.pool).await.unwrap();
    let (watcher, reader, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    for bot in [watcher, reader] {
        sqlx::query("INSERT INTO bots(id,name) VALUES(?1,'Old')")
            .bind(bot)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO social_connections(id,provider,remote_id,display_name,credential_salt,credential) VALUES(?1,'telegram','1','Old bot',x'00',x'00')")
        .bind(c).execute(&db.pool).await.unwrap();
    let long = "Answer support questions. ".repeat(300);
    let wake = json!({"destinations":["100","200","100"],"permissions":{"read":true,"write":true,"send":true,"wake":true},"wake_policy":"mentions","instructions":long.trim()}).to_string();
    let read = r#"{"destinations":["100"],"permissions":{"read":true,"write":false,"send":false,"wake":false},"wake_policy":"never","instructions":""}"#;
    let binding = Uuid::new_v4();
    for (bot, id, access) in [
        (watcher, binding, wake.as_str()),
        (reader, Uuid::new_v4(), read),
    ] {
        sqlx::query(
            "INSERT INTO social_bindings(id,connection_id,bot_id,access) VALUES(?1,?2,?3,?4)",
        )
        .bind(id)
        .bind(c)
        .bind(bot)
        .bind(access)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,binding_id,destination,message_id) VALUES(?1,'e',?2,?3,'100','1')")
        .bind(c).bind(watcher).bind(binding).execute(&db.pool).await.unwrap();

    else_api::db::MIGRATOR.run(&db.pool).await.unwrap();

    let triggers: Vec<(Uuid, Option<String>, String, String)> = sqlx::query_as(
        "SELECT bot_id, destination, wake, instructions FROM social_triggers ORDER BY destination",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let answer = long.trim().to_owned();
    assert!(
        answer.len() > 4000,
        "legacy instructions allowed up to 8000 bytes"
    );
    assert_eq!(
        triggers,
        [
            (
                watcher,
                Some("100".into()),
                "mentions".into(),
                answer.clone()
            ),
            (watcher, Some("200".into()), "mentions".into(), answer),
        ],
        "only access that woke the bot becomes one trigger per chat"
    );
    let delivery: String = sqlx::query_scalar("SELECT status FROM social_deliveries")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(delivery, "revoked");
    let bindings: i64 =
        sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name='social_bindings'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(bindings, 0);
}

#[tokio::test]
async fn completing_another_bots_setup_keeps_an_approved_send_running() {
    use else_api::{
        credentials::OwnerToken, http::AppState, providers::Registry, scheduler::Scheduler,
    };
    let db = TestDb::migrated().await;
    let c = account(&db, "shared").await;
    destinations(&db, c.id).await;
    let (sender, asker) = (bot(&db).await, bot(&db).await);
    let sending = turn(&db, sender, "user").await;
    let asking = turn(&db, asker, "user").await;
    let setup = store::request_setup(&db.pool, asking, Provider::Telegram, "Post updates")
        .await
        .unwrap();
    let fake = Arc::new(Fake::default());
    fake.blocked.store(true, Ordering::SeqCst);
    let manager = Arc::new(Manager::with_transport(
        db.pool.clone(),
        Arc::new(key()),
        fake.clone(),
    ));
    let scheduler = Scheduler::new(db.pool.clone(), Registry::new(vec![]), 1);
    let stop = CancellationToken::new();
    let state = Arc::new(
        AppState::new(
            OwnerToken::parse(common::TOKEN).unwrap(),
            db.pool.clone(),
            scheduler,
            else_api::events::EventHub::start(db.pool.clone(), stop.clone()),
        )
        .with_secret_key(Arc::new(key()))
        .with_socials(manager.clone()),
    );
    let router = else_api::http::router(&else_api::config::Config::load(&TestEnv).unwrap(), state);
    let action = Uuid::new_v4();
    let running = manager.clone();
    let send = tokio::spawn(async move {
        running
            .send(
                c.id,
                sender,
                sending,
                action,
                SendMessage {
                    destination: "100".into(),
                    body: "approved body".into(),
                    reply_to: None,
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), fake.started.notified())
        .await
        .unwrap();
    let (status, body) = http(
        &router,
        "POST",
        &format!("/v1/connectors/socials/setups/{}/complete", setup.id),
        common::TOKEN,
        json!({ "connection_id": c.id }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    fake.release.notify_one();
    let sent = tokio::time::timeout(Duration::from_secs(2), send)
        .await
        .unwrap()
        .unwrap();
    assert!(sent.is_ok(), "the other bot's send survives: {sent:?}");
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
}
