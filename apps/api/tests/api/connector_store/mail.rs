//! Scanning mail and admitting it to bots.

use super::*;

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
    let blocked = seed::bot(&db.pool, "Blocked").await.id;
    let ready = seed::bot(&db.pool, "Ready").await.id;
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
        Arc::new(else_api::providers::codex::Codex::detect(None, None, None).await),
        Arc::new(Synthetic::new(Duration::from_millis(5))),
    ]);
    assert_eq!(
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry, 1)
            .await
            .unwrap(),
        1
    );
    assert_eq!(connector_turns(&db, ready).await.len(), 1);
    assert!(connector_turns(&db, blocked).await.is_empty());
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
    let bot = seed::bot(&db.pool, "Waker").await.id;
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
        envelope: Default::default(),
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
    assert_eq!((paused.state, paused.generation), (AccountState::Paused, 2));
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
    let bot = seed::bot(&db.pool, "Waker").await.id;
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
        inbound::admit_pending::<delivery::Mail>(&db.pool, &Registry::new(vec![]), 8)
            .await
            .unwrap(),
        0,
        "no provider"
    );
    assert_eq!(
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry(), 8)
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
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        0,
        "waits for the unfinished turn"
    );
    assert_eq!(
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        0,
        "and does not repeat"
    );
    seed::force_status(&db.pool, turns[0].0, TurnStatus::Completed).await;

    let mut queued = Vec::new();
    for _ in 0..4 {
        queued.push(
            seed::turn(bot)
                .status(TurnStatus::Queued)
                .insert(&db.pool)
                .await,
        );
    }
    assert_eq!(
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry(), 8)
            .await
            .unwrap(),
        0,
        "queue full"
    );
    for id in queued {
        seed::force_status(&db.pool, id, TurnStatus::Completed).await;
    }
    store::set_active(&db.pool, account, false).await.unwrap();
    assert_eq!(
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry(), 8)
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
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry(), 8)
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
