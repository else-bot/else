//! Restored, cancelled, and expired connections, and the bots a message
//! wakes.

use super::*;

#[tokio::test]
async fn restored_connections_are_passive_and_credentials_are_removed_on_disconnect() {
    let db = TestDb::migrated().await;
    let c = account(&db, "restore").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let trigger = trigger_row(&db, c.id, bot, Some("100"), "all").await;
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,trigger_id,destination,message_id) VALUES(?1,'pending',?2,?3,'100','1')")
        .bind(c.id).bind(bot).bind(trigger).execute(&db.pool).await.unwrap();
    let turn = turn(&db, bot, ItemKind::User).await;
    let action = Uuid::new_v4();
    let payload = json!({"operation":"delete","destination":"100","message_id":"1"});
    store::begin_action(
        &db.pool,
        &c,
        store::Action {
            bot,
            turn,
            id: action,
            destination: Some("100"),
            payload: &payload,
            send: false,
        },
    )
    .await
    .unwrap();
    store::restore_passive(&mut db.pool.acquire().await.unwrap())
        .await
        .unwrap();
    let restored = store::connection(&db.pool, c.id).await.unwrap();
    assert_eq!(restored.state, ConnectionState::Paused);
    assert!(restored.generation > c.generation);
    let delivery: (String, bool) = sqlx::query_as(
        "SELECT status, settled_at IS NOT NULL FROM social_deliveries WHERE event_id='pending'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(delivery, ("cancelled".into(), true));
    assert_eq!(
        store::ACTIONS
            .receipt(&db.pool, c.id, bot, action)
            .await
            .unwrap(),
        Some(json!({"action_id": action, "status": "uncertain"}))
    );
    store::set_state(&db.pool, c.id, ConnectionState::Removed)
        .await
        .unwrap();
    let secret: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT credential FROM social_connections WHERE id=?1")
            .bind(c.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(secret.is_none());
}

#[tokio::test]
async fn owner_credentials_task_authority_and_approved_native_send_stay_separate() {
    let fake = Arc::new(Fake::default());
    let h = harness(
        TestApp::builder()
            .db(Arc::new(TestDb::migrated().await))
            .social(fake.clone()),
    )
    .await;
    let app = &h.app;
    let db = &app.db;
    let router = app.router.clone();
    let bot = bot(db).await;
    let (turn, grant) = h.running(&bot.to_string(), GATE_EVERY_ATTEMPT).await;

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
    destinations(db, connection).await;
    let discovery = social_result(tool(app, &grant, "social_connections", json!({})).await);
    assert_eq!(
        discovery["connections"][0]["destinations"][0]["id"], "100",
        "any bot working for the owner sees the connection's chats: {discovery}"
    );
    let proposed = social_result(
        tool(
            app,
            &grant,
            "draft_message",
            json!({ "platform": "telegram", "account_id": connection, "destination": "100",
                    "body": "Original draft", "reply_to": "10" }),
        )
        .await,
    );
    let draft = proposed["draft"]["draft_id"].as_str().unwrap();
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let (status, approved) = http(
        &router,
        "POST",
        &format!("/v1/bots/{bot}/outgoing-drafts/{draft}/approve"),
        common::TOKEN,
        json!({"version":1,"body":"The owner's exact approved revision"}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{approved}");
    let grant = h.continued(turn, &grant).await;
    let result = social_result(tool(app, &grant, "send_draft", json!({"draft_id":draft})).await);
    assert_eq!(result["status"], "sent", "{result}");
    assert_eq!(
        social_result(tool(app, &grant, "send_draft", json!({"draft_id":draft})).await)["status"],
        "sent"
    );
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
    let body: String = sqlx::query_scalar(
        "SELECT json_extract(result,'$.result.body') FROM social_actions WHERE connection_id=?1",
    )
    .bind(connection)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(body, "The owner's exact approved revision");
}

#[tokio::test]
async fn a_public_telegram_username_works_wherever_a_chat_id_does() {
    let fake = Arc::new(Fake::default());
    let h = harness(
        TestApp::builder()
            .db(Arc::new(TestDb::migrated().await))
            .social(fake.clone()),
    )
    .await;
    let app = &h.app;
    let db = &app.db;
    let router = app.router.clone();
    let bot = bot(db).await;
    let (turn, grant) = h.running(&bot.to_string(), GATE_EVERY_ATTEMPT).await;

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

    let set = social_result(
        tool(
            app,
            &grant,
            "social_trigger_set",
            json!({ "connection_id": connection, "destination": "@team_alerts",
                    "wake": "mentions", "instructions": "Summarize alerts for me." }),
        )
        .await,
    );
    assert_eq!(
        (
            set["trigger"]["destination"].as_str(),
            set["trigger"]["destination_name"].as_str()
        ),
        (Some("-100777"), Some("Team alerts")),
        "a trigger keys on the chat ID incoming messages carry: {set}"
    );
    let discovery = social_result(tool(app, &grant, "social_connections", json!({})).await);
    assert_eq!(
        discovery["connections"][0]["destinations"][0],
        json!({"id":"-100777","name":"Team alerts","kind":"group","can_send":true}),
        "a looked-up chat is listed before it posts anything: {discovery}"
    );

    let proposed = social_result(
        tool(
            app,
            &grant,
            "draft_message",
            json!({ "platform": "telegram", "account_id": connection,
                    "destination": "https://t.me/team_alerts", "body": "Test" }),
        )
        .await,
    );
    let draft = proposed["draft"]["draft_id"].as_str().unwrap();
    assert_eq!(
        proposed["draft"]["content"]["destination"], "-100777",
        "the owner approves the chat the username named when drafted: {proposed}"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let (status, approved) = http(
        &router,
        "POST",
        &format!("/v1/bots/{bot}/outgoing-drafts/{draft}/approve"),
        common::TOKEN,
        json!({"version":1}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{approved}");
    let grant = h.continued(turn, &grant).await;
    let result = social_result(tool(app, &grant, "send_draft", json!({"draft_id":draft})).await);
    assert_eq!(result["status"], "sent", "{result}");
    assert_eq!(*fake.sent_to.lock().unwrap(), vec!["-100777".to_owned()]);

    let (status, stopped) = http(
        &router,
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        common::TOKEN,
        json!({"request_id":Uuid::new_v4()}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::ACCEPTED, "{stopped}");
    app.wait_turn(turn, &["interrupted"]).await;
    let woken = insert_turn(db, bot, ItemKind::Connector, TurnStatus::Queued, GATE).await;
    app.scheduler.wake();
    h.started(woken).await;
    let grant = h.grant_with(woken, vec![Tool::Social]).await;
    let refused = tool(
        app,
        &grant,
        "social_action",
        json!({"connection_id":connection,"command":{"operation":"chat","destination":"@other_channel"}}),
    )
    .await;
    assert!(refused.error, "{}", refused.body);
    assert!(
        refused.result.to_string().contains("no permission"),
        "a woken turn cannot look up another chat: {}",
        refused.body
    );
}

#[tokio::test]
async fn cancelled_setup_cannot_complete_or_touch_pending_deliveries() {
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, ItemKind::User).await;
    let c = account(&db, "cancelled-setup").await;
    destinations(&db, c.id).await;
    let trigger = trigger_row(&db, c.id, bot, Some("100"), "all").await;
    let c = store::connection(&db.pool, c.id).await.unwrap();
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,trigger_id,destination,message_id) VALUES(?1,'pending',?2,?3,'100','1')")
        .bind(c.id).bind(bot).bind(trigger).execute(&db.pool).await.unwrap();
    let request = store::request_setup(&db.pool, turn, Provider::Telegram, "Connect again")
        .await
        .unwrap();
    store::settle_setup(&db.pool, request.id, None, None)
        .await
        .unwrap();
    assert!(
        store::settle_setup(&db.pool, request.id, Some(c.id), None)
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
        else_api::connector_core::inbound::admit_pending::<else_api::socials::delivery::Social>(
            &db.pool,
            &unavailable,
            1
        )
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
        replay: false,
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
        else_api::connector_core::inbound::admit_pending::<else_api::socials::delivery::Social>(
            &db.pool, &registry, 1
        )
        .await
        .unwrap(),
        1
    );
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
