//! Dispatching an action once, and a restored database that stays passive.

use super::*;

#[tokio::test]
async fn an_action_is_dispatched_once_and_never_replayed_after_a_crash() {
    let db = TestDb::migrated().await;
    let (account, generation) = watched(&db, "act@example.test").await;
    let bot = seed::bot(&db.pool, "Actor").await.id;
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
    let running = seed::turn(bot)
        .status(TurnStatus::Running)
        .insert(&db.pool)
        .await;
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
        Start::Dispatch
    );
    assert_eq!(
        begin(action, archive.clone()).await.unwrap(),
        Start::Existing(json!({ "action_id": action, "status": "dispatching" })),
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
    assert_eq!(
        store::ACTIONS
            .finish(
                &db.pool,
                account,
                action,
                Outcome::Succeeded,
                json!({ "result": { "moved": true } })
            )
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(
        store::ACTIONS
            .finish(&db.pool, account, action, Outcome::Failed, json!({}))
            .await
            .unwrap(),
        receipt,
        "a settled action keeps its first receipt"
    );
    assert_eq!(
        begin(action, archive.clone()).await.unwrap(),
        Start::Existing(receipt.clone())
    );
    assert_eq!(
        store::ACTIONS
            .receipt(&db.pool, account, bot, action)
            .await
            .unwrap(),
        Some(receipt)
    );

    let crashed = Uuid::new_v4();
    assert_eq!(
        begin(crashed, archive.clone()).await.unwrap(),
        Start::Dispatch
    );
    assert_eq!(store::recover(&db.pool).await.unwrap(), 1);
    assert_eq!(
        begin(crashed, archive.clone()).await.unwrap(),
        Start::Existing(json!({ "action_id": crashed, "status": "uncertain" }))
    );
    assert_eq!(
        store::ACTIONS
            .finish(&db.pool, account, crashed, Outcome::Succeeded, json!({}))
            .await
            .unwrap(),
        json!({ "action_id": crashed, "status": "uncertain" }),
        "settling after recovery reports what recovery recorded"
    );

    let draft = Draft {
        to: vec!["x@example.test".into()],
        cc: vec![],
        bcc: vec![],
        subject: "s".into(),
        body: "b".into(),
        attachments: vec![],
        reply_to: None,
        message_id: None,
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
        Err(StoreError::Changed)
    ));
    sqlx::query("UPDATE bots SET cancel_pending = 1 WHERE id = ?1")
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(
        matches!(
            begin(Uuid::new_v4(), archive.clone()).await,
            Err(StoreError::NotRunning)
        ),
        "a bot the owner is stopping uses no more tools"
    );
    sqlx::query("UPDATE bots SET cancel_pending = 0 WHERE id = ?1")
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    seed::force_status(&db.pool, running, TurnStatus::Completed).await;
    assert!(matches!(
        begin(Uuid::new_v4(), archive.clone()).await,
        Err(StoreError::NotRunning)
    ));
}

#[tokio::test]
async fn a_restored_database_stays_passive_until_resumed() {
    let db = TestDb::migrated().await;
    let (account, generation) = watched(&db, "restore@example.test").await;
    let bot = seed::bot(&db.pool, "Waker").await.id;
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
    let running = seed::turn(bot)
        .status(TurnStatus::Running)
        .insert(&db.pool)
        .await;
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
        (restored.state, restored.generation),
        (AccountState::Paused, generation + 1)
    );
    assert_eq!(store::cursor(&db.pool, account).await.unwrap(), None);
    assert_eq!(deliveries(&db, bot).await, vec![(11, "cancelled".into())]);
    assert_eq!(
        store::ACTIONS
            .receipt(&db.pool, account, bot, action)
            .await
            .unwrap(),
        Some(json!({ "action_id": action, "status": "uncertain" }))
    );
    assert_eq!(
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry(), 8)
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
