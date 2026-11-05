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
fn access(destinations: &[&str]) -> BindingInput {
    BindingInput {
        destinations: destinations.iter().map(|s| s.to_string()).collect(),
        permissions: Permissions {
            read: true,
            write: true,
            send: true,
            wake: true,
        },
        wake_policy: WakePolicy::Mentions,
        instructions: "Only help when mentioned. Draft before sending.".into(),
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

#[tokio::test]
async fn grants_require_real_destinations_and_scope_external_turns() {
    let db = TestDb::migrated().await;
    let c = account(&db, "scope").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let owner = turn(&db, bot, "user").await;
    assert!(
        store::bind(&db.pool, c.id, bot, access(&["999"]))
            .await
            .is_err()
    );
    let binding = store::bind(&db.pool, c.id, bot, access(&["100", "200"]))
        .await
        .unwrap();
    let c = store::connection(&db.pool, c.id).await.unwrap();
    store::authorize(
        &mut *db.pool.acquire().await.unwrap(),
        &c,
        bot,
        owner,
        Some("100"),
        false,
        false,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE turns SET status='completed' WHERE id=?1")
        .bind(owner)
        .execute(&db.pool)
        .await
        .unwrap();
    let external = turn(&db, bot, "connector").await;
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,binding_id,destination,message_id,status,turn_id) VALUES(?1,'e',?2,?3,'100','1','delivered',?4)").bind(c.id).bind(bot).bind(binding.id).bind(external).execute(&db.pool).await.unwrap();
    let scoped = store::authorize(
        &mut *db.pool.acquire().await.unwrap(),
        &c,
        bot,
        external,
        None,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(scoped.destinations, ["100"]);
    store::authorize(
        &mut *db.pool.acquire().await.unwrap(),
        &c,
        bot,
        external,
        Some("100"),
        false,
        false,
    )
    .await
    .unwrap();
    assert!(
        store::authorize(
            &mut *db.pool.acquire().await.unwrap(),
            &c,
            bot,
            external,
            Some("200"),
            false,
            false
        )
        .await
        .is_err()
    );
    store::bind(&db.pool, c.id, bot, access(&["100"]))
        .await
        .unwrap();
    let fresh = store::connection(&db.pool, c.id).await.unwrap();
    assert!(
        store::authorize(
            &mut *db.pool.acquire().await.unwrap(),
            &fresh,
            bot,
            external,
            Some("100"),
            false,
            false
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn setup_requires_its_provider_bot_and_current_binding_and_requeues() {
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let request = store::request_setup(&db.pool, turn, Provider::Telegram, "Team support")
        .await
        .unwrap();
    let c = account(&db, "setup").await;
    destinations(&db, c.id).await;
    let mut invalid = access(&["100"]);
    invalid.permissions = Permissions::default();
    assert!(
        store::settle_setup(&db.pool, request.id, Some((c.id, invalid)))
            .await
            .is_err()
    );
    assert!(store::bindings(&db.pool, c.id).await.unwrap().is_empty());
    sqlx::query("UPDATE turns SET status='waiting' WHERE id=?1")
        .bind(turn)
        .execute(&db.pool)
        .await
        .unwrap();
    let done = store::settle_setup(&db.pool, request.id, Some((c.id, access(&["100"]))))
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
        store::settle_setup(&db.pool, request.id, Some((c.id, access(&["100"]))))
            .await
            .unwrap()
            .status,
        "connected"
    );
    let generation = store::connection(&db.pool, c.id).await.unwrap().generation;
    store::settle_setup(&db.pool, request.id, Some((c.id, access(&["100"]))))
        .await
        .unwrap();
    assert_eq!(
        store::connection(&db.pool, c.id).await.unwrap().generation,
        generation
    );
}

#[tokio::test]
async fn action_receipts_reject_changed_retries_and_survive_crash_as_uncertain() {
    let db = TestDb::migrated().await;
    let c = account(&db, "actions").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    store::bind(&db.pool, c.id, bot, access(&["100"]))
        .await
        .unwrap();
    let c = store::connection(&db.pool, c.id).await.unwrap();
    let id = Uuid::new_v4();
    let payload = json!({"operation":"delete","destination":"100","message_id":"1"});
    assert!(
        store::begin_action(&db.pool, &c, bot, turn, id, "100", &payload, false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store::begin_action(&db.pool, &c, bot, turn, id, "100", &payload, false)
            .await
            .unwrap()
            .unwrap()["status"],
        "dispatching"
    );
    assert!(matches!(
        store::begin_action(
            &db.pool,
            &c,
            bot,
            turn,
            id,
            "100",
            &json!({"different":true}),
            false
        )
        .await,
        Err(Error::Conflict)
    ));
    store::recover(&db.pool).await.unwrap();
    assert_eq!(
        store::begin_action(&db.pool, &c, bot, turn, id, "100", &payload, false)
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
    store::bind(&db.pool, c.id, bot, access(&["100"]))
        .await
        .unwrap();
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
        else_api::db::delegations::origin(&mut *db.pool.acquire().await.unwrap(), turn)
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
    async fn execute(&self, _: &Session, _: &Command) -> Result<Value, Error> {
        Ok(json!({"ok":true}))
    }
    async fn send(
        &self,
        _: &Session,
        _: &str,
        body: &str,
        _: Option<&str>,
    ) -> Result<Value, Error> {
        self.started.notify_one();
        if self.blocked.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        self.sends.fetch_add(1, Ordering::SeqCst);
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
        stop.cancelled().await;
        Ok(())
    }
}

#[tokio::test]
async fn changing_generation_cancels_inflight_sdk_wait_and_records_uncertainty() {
    let db = TestDb::migrated().await;
    let c = account(&db, "cancel").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    store::bind(&db.pool, c.id, bot, access(&["100"]))
        .await
        .unwrap();
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
                "100".into(),
                "approved body".into(),
                None,
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
    store::bind(&db.pool, c.id, bot, access(&["100"]))
        .await
        .unwrap();
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
            .send(c.id, bot, turn, id, "100".into(), "body".into(), None)
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
    store::restore_passive(&mut *db.pool.acquire().await.unwrap())
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
    let (status, binding) = http(
        &router,
        "PUT",
        &format!("/v1/connectors/socials/connections/{connection}/bots/{bot}"),
        common::TOKEN,
        json!(access(&["100"])),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{binding}");
    let discovery = rpc(&router, &grant, "social_connections", json!({})).await;
    assert_eq!(
        discovery["connections"][0]["instructions"],
        "Only help when mentioned. Draft before sending."
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
async fn cancelled_setup_cannot_mutate_existing_access_or_pending_deliveries() {
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, "user").await;
    let c = account(&db, "cancelled-setup").await;
    destinations(&db, c.id).await;
    let original = store::bind(&db.pool, c.id, bot, access(&["100"]))
        .await
        .unwrap();
    let c = store::connection(&db.pool, c.id).await.unwrap();
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,binding_id,destination,message_id) VALUES(?1,'pending',?2,?3,'100','1')")
        .bind(c.id).bind(bot).bind(original.id).execute(&db.pool).await.unwrap();
    let request = store::request_setup(&db.pool, turn, Provider::Telegram, "Update access")
        .await
        .unwrap();
    store::settle_setup(&db.pool, request.id, None)
        .await
        .unwrap();
    assert!(
        store::settle_setup(&db.pool, request.id, Some((c.id, access(&["200"]))))
            .await
            .is_err()
    );
    assert_eq!(
        store::connection(&db.pool, c.id).await.unwrap().generation,
        c.generation
    );
    let bindings = store::bindings(&db.pool, c.id).await.unwrap();
    assert_eq!(bindings[0].id, original.id);
    assert_eq!(bindings[0].access.destinations, ["100"]);
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
    let binding = store::bind(&db.pool, c.id, bot, access(&["100"]))
        .await
        .unwrap();
    let c = store::connection(&db.pool, c.id).await.unwrap();
    sqlx::query("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<5000) INSERT INTO social_messages(connection_id,destination,message_id,author_id,body,event_id,created_at) SELECT ?1,'100',CAST(x AS TEXT),'human','backlog',CAST(x AS TEXT),printf('2020-01-01T00:%05d',x) FROM n")
        .bind(c.id).execute(&db.pool).await.unwrap();
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,binding_id,destination,message_id) SELECT connection_id,event_id,?2,?3,destination,message_id FROM social_messages WHERE connection_id=?1")
        .bind(c.id).bind(bot).bind(binding.id).execute(&db.pool).await.unwrap();
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
