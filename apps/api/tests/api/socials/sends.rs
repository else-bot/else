//! Action receipts, incoming deliveries, and sends that stop before they
//! settle.

use super::*;

#[tokio::test]
async fn action_receipts_reject_changed_retries_and_survive_crash_as_uncertain() {
    let db = TestDb::migrated().await;
    let c = account(&db, "actions").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, ItemKind::User).await;
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
                destination: Some("100"),
                payload: &payload,
                send: false
            }
        )
        .await
        .unwrap()
            == Start::Dispatch
    );
    assert_eq!(
        store::begin_action(
            &db.pool,
            &c,
            store::Action {
                bot,
                turn,
                id,
                destination: Some("100"),
                payload: &payload,
                send: false
            }
        )
        .await
        .unwrap(),
        Start::Existing(json!({"action_id": id, "status": "dispatching"}))
    );
    assert!(matches!(
        store::begin_action(
            &db.pool,
            &c,
            store::Action {
                bot,
                turn,
                id,
                destination: Some("100"),
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
                destination: Some("100"),
                payload: &payload,
                send: false
            }
        )
        .await
        .unwrap(),
        Start::Existing(json!({"action_id": id, "status": "uncertain"}))
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
            replay: false,
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

#[tokio::test]
async fn changing_generation_cancels_inflight_sdk_wait_and_records_uncertainty() {
    let db = TestDb::migrated().await;
    let c = account(&db, "cancel").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, ItemKind::User).await;
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
                    files: Vec::new(),
                },
            )
            .await
    });
    common::until("the send to start", fake.started.notified()).await;
    store::set_state(&db.pool, c.id, ConnectionState::Paused)
        .await
        .unwrap();
    manager.changed(c.id).await;
    let sent = common::until("the cancelled send", task).await.unwrap();
    assert!(matches!(sent, Err(Error::Inactive)), "{sent:?}");
    common::until(
        "the cancelled send to be dropped",
        fake.abandoned.notified(),
    )
    .await;
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
    let status: String = sqlx::query_scalar("SELECT status FROM social_actions WHERE action_id=?1")
        .bind(id)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "uncertain");
}

#[tokio::test]
async fn stopping_work_cancels_an_inflight_send_and_records_uncertainty() {
    let fake = Arc::new(Fake::default());
    let db = Arc::new(TestDb::migrated().await);
    let h = harness(TestApp::builder().db(db.clone()).social(fake.clone())).await;
    let app = &h.app;
    let c = account(&db, "stop").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let (turn, _) = h.running(&bot.to_string(), GATE).await;
    fake.blocked.store(true, Ordering::SeqCst);
    let id = Uuid::new_v4();
    let running = app.state.socials.clone();
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
                    files: Vec::new(),
                },
            )
            .await
    });
    tokio::time::timeout(common::SETTLE, fake.started.notified())
        .await
        .unwrap();
    let (status, body) = http(
        &app.router,
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        common::TOKEN,
        json!({ "request_id": Uuid::new_v4() }),
    )
    .await;
    assert!(status.is_success(), "{body}");
    let sent = tokio::time::timeout(common::SETTLE, task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(sent, Err(Error::Inactive)), "{sent:?}");
    common::until(
        "the cancelled send to be dropped",
        fake.abandoned.notified(),
    )
    .await;
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
    assert_eq!(
        store::ACTIONS
            .receipt(&db.pool, c.id, bot, id)
            .await
            .unwrap(),
        Some(json!({"action_id": id, "status": "uncertain"}))
    );
}

#[tokio::test]
async fn a_send_whose_caller_gave_up_stops_and_records_uncertainty() {
    let db = TestDb::migrated().await;
    let c = account(&db, "gave-up").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let turn = turn(&db, bot, ItemKind::User).await;
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
                    body: "body".into(),
                    reply_to: None,
                    files: Vec::new(),
                },
            )
            .await
    });
    tokio::time::timeout(common::SETTLE, fake.started.notified())
        .await
        .unwrap();
    task.abort();
    tokio::time::timeout(common::SETTLE, async {
        while store::ACTIONS
            .receipt(&db.pool, c.id, bot, id)
            .await
            .unwrap()
            != Some(json!({"action_id": id, "status": "uncertain"}))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the abandoned send is recorded uncertain");
    common::until(
        "the cancelled send to be dropped",
        fake.abandoned.notified(),
    )
    .await;
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
}

/// A Telegram message to chat 100 drafted for connection `connection` by a
/// running request and approved by the owner; the request continues.
async fn approved_social_draft(
    h: &common::recorder::Harness,
    bot: Uuid,
    connection: Uuid,
) -> (Uuid, Uuid) {
    let app = &h.app;
    let (turn, grant) = h.running(&bot.to_string(), GATE_EVERY_ATTEMPT).await;
    let proposed = social_result(
        tool(
            app,
            &grant,
            "draft_message",
            json!({ "platform": "telegram", "account_id": connection, "destination": "100",
                    "body": "Approved update" }),
        )
        .await,
    );
    let draft: Uuid = proposed["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let (status, approved) = http(
        &app.router,
        "POST",
        &format!("/v1/bots/{bot}/outgoing-drafts/{draft}/approve"),
        common::TOKEN,
        json!({ "version": 1 }),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{approved}");
    h.continued(turn, &grant).await;
    (turn, draft)
}

const SOCIAL: Channels = Channels {
    mail: false,
    social: true,
};

/// A send that gives up before the connection answers, as its wait does.
/// With `admit`, the real worker first starts the same action for `bot` in
/// `turn` and holds it in the transport.
struct GaveUp {
    manager: Arc<Manager>,
    fake: Arc<Fake>,
    bot: Uuid,
    turn: Uuid,
    admit: bool,
}

#[async_trait]
impl Connected for GaveUp {
    async fn send_mail(&self, _: Uuid, _: Uuid, _: MailCommand) -> Result<(), MailError> {
        unreachable!("these drafts are messages")
    }

    async fn find_sent(&self, _: Uuid, _: String) -> Result<bool, MailError> {
        unreachable!("these drafts are messages")
    }

    async fn send_social(
        &self,
        account_id: Uuid,
        action_id: Uuid,
        _: Platform,
        message: SendMessage,
    ) -> Result<(), Error> {
        if self.admit {
            let (manager, bot, turn) = (self.manager.clone(), self.bot, self.turn);
            tokio::spawn(async move {
                manager
                    .send(account_id, bot, turn, action_id, message)
                    .await
            });
            tokio::time::timeout(common::SETTLE, self.fake.started.notified())
                .await
                .expect("the worker admits the send");
        }
        Err(Error::Uncertain)
    }
}

#[tokio::test]
async fn a_send_given_up_before_its_worker_admits_it_fails_and_is_never_delivered() {
    let fake = Arc::new(Fake::default());
    let db = Arc::new(TestDb::migrated().await);
    let h = harness(TestApp::builder().db(db.clone()).social(fake.clone())).await;
    let app = &h.app;
    let c = account(&db, "withdrawn").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let (turn, draft) = approved_social_draft(&h, bot, c.id).await;
    let via = GaveUp {
        manager: app.state.socials.clone(),
        fake: fake.clone(),
        bot,
        turn,
        admit: false,
    };

    let failed = send_connected(&db.pool, turn, draft, SOCIAL, &via)
        .await
        .unwrap();
    assert_eq!(failed.status.as_str(), "failed");
    let queued = app
        .state
        .socials
        .send(
            c.id,
            bot,
            turn,
            failed.claim_action_id.unwrap(),
            SendMessage {
                destination: "100".into(),
                body: "Approved update".into(),
                reply_to: None,
                files: Vec::new(),
            },
        )
        .await;
    assert!(matches!(queued, Err(Error::Conflict)), "{queued:?}");
    assert_eq!(fake.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_stopped_request_fails_a_message_its_worker_never_started() {
    let fake = Arc::new(Fake::default());
    let db = Arc::new(TestDb::migrated().await);
    let h = harness(TestApp::builder().db(db.clone()).social(fake.clone())).await;
    let c = account(&db, "stopped").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let (turn, draft) = approved_social_draft(&h, bot, c.id).await;
    else_api::db::outgoing_drafts::claim_connected(&db.pool, turn, draft, SOCIAL)
        .await
        .unwrap();
    // The owner's stop, whose trigger fails a send no worker started.
    common::seed::force_status(&db.pool, turn, TurnStatus::Cancelling).await;
    let (status, detail): (String, String) =
        sqlx::query_as("SELECT status, result_detail FROM outgoing_drafts WHERE id = ?1")
            .bind(draft)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert_eq!(
        detail,
        "The request ended before the connection started sending it. Nothing was sent."
    );
}

#[tokio::test]
async fn a_send_its_worker_admitted_before_the_caller_gave_up_settles_by_its_receipt() {
    let fake = Arc::new(Fake::default());
    fake.blocked.store(true, Ordering::SeqCst);
    let db = Arc::new(TestDb::migrated().await);
    let h = harness(TestApp::builder().db(db.clone()).social(fake.clone())).await;
    let app = &h.app;
    let c = account(&db, "admitted").await;
    destinations(&db, c.id).await;
    let bot = bot(&db).await;
    let (turn, draft) = approved_social_draft(&h, bot, c.id).await;
    let via = GaveUp {
        manager: app.state.socials.clone(),
        fake: fake.clone(),
        bot,
        turn,
        admit: true,
    };

    let pending = send_connected(&db.pool, turn, draft, SOCIAL, &via)
        .await
        .unwrap();
    assert_eq!(pending.status.as_str(), "sending");
    fake.release.notify_one();
    tokio::time::timeout(common::SETTLE, async {
        loop {
            let status: String =
                sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
                    .bind(draft)
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            if status == "sent" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the admitted send settles the draft as sent");
    assert_eq!(fake.sends.load(Ordering::SeqCst), 1);
}
