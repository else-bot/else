//! Account credentials, account limits, and the configs the store accepts.

use super::*;

fn other_key() -> SecretKey {
    SecretKey::parse("HwECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap()
}

#[tokio::test]
async fn credentials_are_encrypted_and_bound_to_their_account() {
    let db = TestDb::migrated().await;
    let first = store::create(
        &db.pool,
        &key(),
        &config("one@example.test"),
        &password(PASSWORD),
    )
    .await
    .unwrap();
    let second = store::create(
        &db.pool,
        &key(),
        &config("two@example.test"),
        &password("another secret"),
    )
    .await
    .unwrap();
    assert!(
        !serde_json::to_string(&else_wire::email::EmailAccount::from(&first))
            .unwrap()
            .contains(PASSWORD),
        "an account never carries its credential"
    );
    let opened = store::credential(&db.pool, &key(), first.id).await.unwrap();
    let Credential::Password { password: stored } = &opened else {
        panic!("a password credential");
    };
    assert_eq!(stored, PASSWORD);
    let (salt, cipher): (Vec<u8>, Vec<u8>) =
        sqlx::query_as("SELECT credential_salt, credential FROM connector_accounts WHERE id = ?1")
            .bind(first.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(
        !cipher
            .windows(PASSWORD.len())
            .any(|w| w == PASSWORD.as_bytes()),
        "stored encrypted"
    );
    assert!(matches!(
        store::credential(&db.pool, &other_key(), first.id).await,
        Err(StoreError::Crypto)
    ));

    sqlx::query(
        "UPDATE connector_accounts SET credential_salt = ?2, credential = ?3 WHERE id = ?1",
    )
    .bind(second.id)
    .bind(&salt)
    .bind(&cipher)
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        matches!(
            store::credential(&db.pool, &key(), second.id).await,
            Err(StoreError::Crypto)
        ),
        "a credential copied to another account does not decrypt"
    );
    let err = store::credential(&db.pool, &key(), second.id)
        .await
        .err()
        .unwrap();
    assert!(
        !format!("{err} {err:?}").contains(PASSWORD),
        "errors carry no secret"
    );

    assert!(
        !store::save_credential(&db.pool, &key(), first.id, 2, &password("stale"))
            .await
            .unwrap(),
        "fenced by generation"
    );
    assert!(
        store::save_credential(&db.pool, &key(), first.id, 1, &password("refreshed"))
            .await
            .unwrap()
    );
    let opened = store::credential(&db.pool, &key(), first.id).await.unwrap();
    let Credential::Password {
        password: refreshed,
    } = &opened
    else {
        panic!("a password credential");
    };
    assert_eq!(refreshed, "refreshed");
    sqlx::query(
        "UPDATE connector_accounts SET credential_salt = ?2, credential = ?3 WHERE id = ?1",
    )
    .bind(first.id)
    .bind(&salt)
    .bind(&cipher)
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        matches!(
            store::credential(&db.pool, &key(), first.id).await,
            Err(StoreError::Crypto)
        ),
        "an older credential version does not decrypt in place of the current one"
    );

    assert!(
        store::replace_credential(&db.pool, &key(), first.id, &password("replaced"))
            .await
            .unwrap()
    );
    assert_eq!(
        store::account(&db.pool, first.id)
            .await
            .unwrap()
            .unwrap()
            .generation,
        2,
        "replacement fences old work"
    );

    assert!(store::remove(&db.pool, first.id).await.unwrap());
    assert!(!store::remove(&db.pool, first.id).await.unwrap());
    let erased: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT credential FROM connector_accounts WHERE id = ?1")
            .bind(first.id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(erased.is_none(), "removal erases the credential");
    assert!(matches!(
        store::credential(&db.pool, &key(), first.id).await,
        Err(StoreError::NotFound)
    ));
    assert!(store::account(&db.pool, first.id).await.unwrap().is_none());
}

#[tokio::test]
async fn accounts_and_grants_are_bounded() {
    let db = TestDb::migrated().await;
    let first = store::create(
        &db.pool,
        &key(),
        &config("a0@example.test"),
        &password(PASSWORD),
    )
    .await
    .unwrap();
    assert!(matches!(
        store::create(
            &db.pool,
            &key(),
            &config("A0@example.test"),
            &password(PASSWORD)
        )
        .await,
        Err(StoreError::AddressTaken)
    ));
    for i in 1..32 {
        store::create(
            &db.pool,
            &key(),
            &config(&format!("a{i}@example.test")),
            &password(PASSWORD),
        )
        .await
        .unwrap();
    }
    assert!(matches!(
        store::create(
            &db.pool,
            &key(),
            &config("extra@example.test"),
            &password(PASSWORD)
        )
        .await,
        Err(StoreError::Limit(_))
    ));
    assert!(matches!(
        store::create(&db.pool, &key(), &config("bad"), &password(PASSWORD)).await,
        Err(StoreError::Invalid(_))
    ));
    assert_eq!(store::accounts(&db.pool).await.unwrap().len(), 32);

    let accounts = store::accounts(&db.pool).await.unwrap();
    let mut granted = 0;
    'grant: for i in 0..5 {
        let bot = seed::bot(&db.pool, &format!("Bot {i}")).await.id;
        for account in &accounts {
            if granted == 128 {
                assert!(matches!(
                    store::set_permissions(&db.pool, account.id, bot, &read_only()).await,
                    Err(StoreError::Limit(_))
                ));
                break 'grant;
            }
            assert!(
                store::set_permissions(&db.pool, account.id, bot, &read_only())
                    .await
                    .unwrap()
            );
            granted += 1;
        }
    }
    assert_eq!(granted, 128);
    assert!(store::remove(&db.pool, first.id).await.unwrap());
    assert!(
        store::grants(&db.pool, first.id).await.unwrap().is_empty(),
        "removal drops grants"
    );
}

#[tokio::test]
async fn the_store_accepts_every_config_the_mail_validator_accepts() {
    let db = TestDb::migrated().await;
    let mut long = config("long@example.test");
    long.archive_folder = "a".repeat(300);
    long.validate().unwrap();
    let account = store::create(&db.pool, &key(), &long, &password(PASSWORD))
        .await
        .unwrap();
    let mut moved = account.config.clone();
    moved.trash_folder = "t".repeat(300);
    moved.validate().unwrap();
    let configured = store::configure_folders(&db.pool, account.id, &moved)
        .await
        .unwrap();
    assert_eq!(configured.config.trash_folder, "t".repeat(300));
}
