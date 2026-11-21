//! Connecting a social account, its listener, and the triggers a bot sets.

use super::*;

fn watch(c: Uuid, destination: Option<&str>, wake: Wake) -> TriggerInput {
    TriggerInput {
        connection_id: c,
        destination: destination.map(str::to_owned),
        wake,
        instructions: "Summarize what the team asks and draft replies for my approval.".into(),
    }
}

#[tokio::test]
async fn tokens_are_encrypted_identity_bound_and_never_public() {
    let db = TestDb::migrated().await;
    let c = account(&db, "first").await;
    let other = account(&db, "second").await;
    let Credential::Token(stored) = store::credential(&db.pool, &key(), &c).await.unwrap() else {
        panic!("a bot connection holds a token");
    };
    assert_eq!(stored.0.as_bytes(), token().0.as_bytes());
    let cipher: Vec<u8> =
        sqlx::query_scalar("SELECT credential FROM social_connections WHERE id=?1")
            .bind(c.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(!String::from_utf8_lossy(&cipher).contains(token().0.as_str()));
    assert!(
        !serde_json::to_string(&WireConnection::from(&c))
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
            &Secret::Token(token()),
            None,
            ""
        )
        .await,
        Err(Error::Duplicate)
    ));
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
    let owner = turn(&db, bot, ItemKind::User).await;
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
    // End the turn by hand so the bot's next turn can take its active slot.
    common::seed::force_status(&db.pool, owner, TurnStatus::Completed).await;
    let external = turn(&db, bot, ItemKind::Connector).await;
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
    let turn = turn(&db, bot, ItemKind::User).await;
    let request = store::request_setup(&db.pool, turn, Provider::Telegram, "Team support")
        .await
        .unwrap();
    let c = account(&db, "setup").await;
    // Park the turn on its setup, so settling the setup requeues it.
    common::seed::force_status(&db.pool, turn, TurnStatus::Waiting).await;
    let done = store::settle_setup(&db.pool, request.id, Some(c.id), None)
        .await
        .unwrap();
    assert_eq!(done.status, store::SetupStatus::Connected);
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?1")
        .bind(turn)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "queued");
    assert_eq!(
        store::settle_setup(&db.pool, request.id, Some(c.id), None)
            .await
            .unwrap()
            .status,
        store::SetupStatus::Connected,
        "a retry of the same completion changes nothing"
    );
    assert!(
        store::settle_setup(&db.pool, request.id, None, None)
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
    let external = turn(&db, bot, ItemKind::Connector).await;
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
    let owner = turn(&db, bot, ItemKind::User).await;
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
    let external = turn(&db, bot, ItemKind::Connector).await;
    assert!(
        store::set_trigger(&db.pool, external, &watch(c.id, Some("100"), Wake::All))
            .await
            .is_err(),
        "an incoming message cannot set a trigger"
    );
    // End the turn by hand so the bot's next turn can take its active slot.
    common::seed::force_status(&db.pool, external, TurnStatus::Completed).await;
    let owner = turn(&db, bot, ItemKind::User).await;
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

async fn health_after_listening(refused: bool) -> Health {
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
    let health = |db: &TestDb| {
        let pool = db.pool.clone();
        async move { store::connection(&pool, c.id).await.unwrap().health }
    };
    let settled = common::until(
        "the listener's health",
        common::poll(|| async {
            let health = health(&db).await;
            matches!(health, Health::Connected | Health::ReceiverConflict).then_some(health)
        }),
    )
    .await;
    // Outlast the 3 s listen settle, after which a listener is marked Connected.
    common::never_polled(
        "the listener's health to change",
        Duration::from_secs(4),
        || async {
            let health = health(&db).await;
            (health != settled).then_some(health)
        },
    )
    .await;
    settled
}

#[tokio::test]
async fn a_bot_whose_updates_go_elsewhere_shows_the_conflict_instead_of_connected() {
    assert_eq!(health_after_listening(true).await, Health::ReceiverConflict);
}

#[tokio::test]
async fn a_listener_that_stays_open_is_connected() {
    assert_eq!(health_after_listening(false).await, Health::Connected);
}
