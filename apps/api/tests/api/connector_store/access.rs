//! Which bots may read an account's mail, and what revoking access withdraws.

use super::*;

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
    let bot = seed::bot(&db.pool, "Temporary").await.id;
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
async fn explicit_permissions_restrict_bots_and_revocation_withdraws_pending_mail() {
    let db = TestDb::migrated().await;
    let (account, generation) = watched(&db, "inbox@example.test").await;
    let (waker, reader) = (
        seed::bot(&db.pool, "Waker").await.id,
        seed::bot(&db.pool, "Reader").await.id,
    );
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

    let running = seed::turn(reader)
        .status(TurnStatus::Running)
        .insert(&db.pool)
        .await;
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
    let stranger = seed::bot(&db.pool, "Stranger").await.id;
    let stranger_turn = seed::turn(stranger)
        .status(TurnStatus::Running)
        .insert(&db.pool)
        .await;
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
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry(), 8)
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
        inbound::admit_pending::<delivery::Mail>(&db.pool, &registry(), 8)
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
