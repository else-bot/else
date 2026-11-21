//! Connection state across repeats, restarts, and upgrades, and what an
//! approved send carries.

use super::*;

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
    let c = account(db, "shared").await;
    destinations(db, c.id).await;
    let (sender, asker) = (bot(db).await, bot(db).await);
    let (sending, _) = h.running(&sender.to_string(), GATE).await;
    let (asking, _) = h.running(&asker.to_string(), GATE).await;
    let setup = store::request_setup(&db.pool, asking, Provider::Telegram, "Post updates")
        .await
        .unwrap();
    fake.blocked.store(true, Ordering::SeqCst);
    let action = Uuid::new_v4();
    let running = app.state.socials.clone();
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
                    files: Vec::new(),
                },
            )
            .await
    });
    common::until("the send to start", fake.started.notified()).await;
    let (status, body) = http(
        &router,
        "POST",
        &format!("/v1/connectors/socials/setups/{}/complete", setup.id),
        common::TOKEN,
        json!({ "connection_id": c.id }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{body}");
    assert_eq!(body["setup"]["status"], "connected");
    assert_eq!(body["setup"]["connection_id"], json!(c.id));
    assert_eq!(body["setup"]["bot_id"], json!(setup.bot_id));
    assert_eq!(body["setup"]["turn_id"], json!(setup.turn_id));
    assert_eq!(body["setup"]["note"], Value::Null);
    fake.release.notify_one();
    let sent = common::until("the other bot's send", send).await.unwrap();
    assert!(sent.is_ok(), "the other bot's send survives: {sent:?}");
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn repeating_the_current_state_keeps_pending_deliveries_and_generation() {
    let db = TestDb::migrated().await;
    let bot = bot(&db).await;
    let c = account(&db, "same-state").await;
    destinations(&db, c.id).await;
    let trigger = trigger_row(&db, c.id, bot, Some("100"), "all").await;
    sqlx::query("INSERT INTO social_deliveries(connection_id,event_id,bot_id,trigger_id,destination,message_id) VALUES(?1,'pending',?2,?3,'100','1')")
        .bind(c.id).bind(bot).bind(trigger).execute(&db.pool).await.unwrap();
    let before = store::connection(&db.pool, c.id).await.unwrap();
    let store::StateChange::Unchanged(returned) =
        store::set_state(&db.pool, c.id, ConnectionState::Active)
            .await
            .unwrap()
    else {
        panic!("an active connection stays as it is");
    };
    let stored = store::connection(&db.pool, c.id).await.unwrap();
    assert_eq!(stored.generation, before.generation);
    assert_eq!(
        (returned.generation, returned.health),
        (stored.generation, stored.health)
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM social_deliveries WHERE event_id='pending'")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn resuming_reports_the_stored_connection() {
    let db = TestDb::migrated().await;
    let c = account(&db, "resume").await;
    store::set_state(&db.pool, c.id, ConnectionState::Paused)
        .await
        .unwrap();
    let store::StateChange::Changed(returned) =
        store::set_state(&db.pool, c.id, ConnectionState::Active)
            .await
            .unwrap()
    else {
        panic!("a paused connection resumes");
    };
    let stored = store::connection(&db.pool, c.id).await.unwrap();
    assert_eq!(
        WireConnection::from(&returned),
        WireConnection::from(&stored)
    );
}

#[tokio::test]
async fn finishing_an_action_after_recovery_returns_the_stored_receipt() {
    let db = TestDb::migrated().await;
    let c = account(&db, "settled").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, ItemKind::User).await;
    let id = Uuid::new_v4();
    let payload = json!({"operation":"delete","destination":"100","message_id":"1"});
    store::begin_action(
        &db.pool,
        &c,
        store::Action {
            bot,
            turn,
            id,
            destination: Some("100"),
            payload: &payload,
            send: false,
        },
    )
    .await
    .unwrap();
    store::uncertain(&db.pool, c.id, Some(id)).await.unwrap();
    let receipt = store::finish_action(&db.pool, c.id, id, &Ok(json!({"deleted":true})))
        .await
        .unwrap();
    assert_eq!(receipt, json!({"action_id": id, "status": "uncertain"}));
}

#[test]
fn connection_state_transitions() {
    use ConnectionState::*;
    assert_eq!(Active.transition(Paused).unwrap(), Some(Paused));
    assert_eq!(Paused.transition(Active).unwrap(), Some(Active));
    assert_eq!(Paused.transition(Removed).unwrap(), Some(Removed));
    assert_eq!(Active.transition(Active).unwrap(), None);
    assert_eq!(Paused.transition(Paused).unwrap(), None);
    assert!(matches!(Removed.transition(Active), Err(Error::NotFound)));
    assert!(matches!(Removed.transition(Removed), Err(Error::NotFound)));
}

#[tokio::test]
async fn an_approved_message_sends_exactly_its_approved_picture() {
    let fake = Arc::new(Fake::default());
    let root = tempfile::tempdir().unwrap();
    let h = harness(
        TestApp::builder()
            .db(Arc::new(TestDb::migrated().await))
            .social(fake.clone())
            .projects(root.path()),
    )
    .await;
    let app = &h.app;
    let db = &app.db;
    let router = app.router.clone();
    let bot = bot(db).await;
    let (turn, grant) = h.running(&bot.to_string(), GATE_EVERY_ATTEMPT).await;
    let dir = else_api::providers::host::project_dir(root.path(), bot).unwrap();
    let mut flyer = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(3, 2, image::Rgb([0, 90, 200]))
        .write_to(&mut flyer, image::ImageFormat::Png)
        .unwrap();
    let flyer = flyer.into_inner();
    std::fs::write(dir.join("flyer.png"), &flyer).unwrap();

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
    destinations(db, connection.parse().unwrap()).await;
    let proposed = social_result(
        tool(
            app,
            &grant,
            "draft_message",
            json!({"platform":"telegram","account_id":connection,"destination":"100",
               "body":"Rue aux enfants samedi !","attachments":[{"path":"flyer.png"}]}),
        )
        .await,
    );
    let draft = proposed["draft"]["draft_id"].as_str().unwrap().to_owned();
    let file = proposed["draft"]["content"]["attachments"][0].clone();
    assert_eq!(file["media_type"], "image/png", "{proposed}");
    assert!(
        proposed["draft"].get("attachment_paths").is_none(),
        "a connected draft is never sent by the bot's own tools"
    );
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let (status, approved) = http(
        &router,
        "POST",
        &format!("/v1/bots/{bot}/outgoing-drafts/{draft}/approve"),
        common::TOKEN,
        json!({"version":1,"attachments":[file["id"]]}),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{approved}");
    let grant = h.continued(turn, &grant).await;
    std::fs::write(dir.join("flyer.png"), b"changed after approval").unwrap();
    let result = social_result(tool(app, &grant, "send_draft", json!({"draft_id":draft})).await);
    assert_eq!(result["status"], "sent", "{result}");
    assert_eq!(
        *fake.sent_files.lock().unwrap(),
        vec![("flyer.png".to_owned(), "image/png".to_owned(), flyer)],
        "the snapshot the owner approved, not the file as it is now"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pending_setup_shows_in_the_sidebar_as_a_request_until_it_is_settled() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, _) = h
        .running(
            &bot,
            &format!("{GATE_AFTER_OPENING} post the update to the team chat."),
        )
        .await;
    tokio::time::timeout(common::SETTLE, async {
        loop {
            let snapshot = app
                .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
                .await;
            if snapshot.body["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i["kind"] == "assistant")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the reply started");
    let setup = store::request_setup(
        &app.db.pool,
        turn,
        Provider::Telegram,
        "Post updates to the team chat.",
    )
    .await
    .unwrap();

    let listed = app.owner("GET", "/v1/bots", None).await;
    let shown = listed.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == bot)
        .unwrap()
        .clone();
    let status = &shown["sidebar_status"];
    assert_eq!(status["kind"], "needs_credentials", "{status}");
    assert_eq!(status["preview"], "Connect Telegram");
    assert!(
        status["since"]
            .as_str()
            .is_some_and(|at| at.parse::<chrono::DateTime<chrono::Utc>>().is_ok()),
        "{status}"
    );
    assert_eq!(shown["read_state"]["unread_replies"], 1, "{shown}");

    store::settle_setup(&app.db.pool, setup.id, None, None)
        .await
        .unwrap();
    let listed = app.owner("GET", "/v1/bots", None).await;
    let shown = listed.body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == bot)
        .unwrap();
    assert_ne!(shown["sidebar_status"]["kind"], "needs_credentials");
}

#[tokio::test]
async fn upgrading_keeps_recorded_social_receipts_readable() {
    use std::borrow::Cow;
    let db = TestDb::empty().await;
    let before = sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            else_api::db::MIGRATOR
                .migrations
                .iter()
                .filter(|m| m.version < 83)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    before.run(&db.pool).await.unwrap();
    let (bot, turn, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let (sent, unsure, open) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut conn = db.pool.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await
        .unwrap();
    for (action, status, result) in [
        (sent, "succeeded", Some(r#"{"id":"5"}"#)),
        (unsure, "uncertain", Some("{}")),
        (open, "dispatching", None),
    ] {
        sqlx::query("INSERT INTO social_actions(connection_id,action_id,bot_id,turn_id,fingerprint,destination,status,result) VALUES(?1,?2,?3,?4,x'00','100',?5,?6)")
            .bind(c).bind(action).bind(bot).bind(turn).bind(status).bind(result)
            .execute(&mut *conn).await.unwrap();
    }
    drop(conn);

    else_api::db::MIGRATOR.run(&db.pool).await.unwrap();

    let receipt = |action| store::ACTIONS.receipt(&db.pool, c, bot, action);
    assert_eq!(
        receipt(sent).await.unwrap(),
        Some(json!({"action_id": sent, "status": "succeeded", "result": {"id": "5"}}))
    );
    assert_eq!(
        receipt(unsure).await.unwrap(),
        Some(json!({"action_id": unsure, "status": "uncertain", "result": {}}))
    );
    assert_eq!(
        receipt(open).await.unwrap(),
        Some(json!({"action_id": open, "status": "dispatching"}))
    );
}
