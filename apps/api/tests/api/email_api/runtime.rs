//! The account runtime: watchers, rejections, retried transport effects, and
//! encrypted backup.

use super::*;

#[tokio::test]
async fn quiet_watchers_survive_bot_commands_and_never_queue_empty_turns() {
    let transport = Arc::new(QuietInbox::default());
    let h = recorder::harness(common::TestApp::builder().mail(transport.clone())).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let (turn, _) = h.running(&bot.to_string(), GATE).await;
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let manager = h.app.state.connectors.clone();
    manager.changed(account.id).await;
    common::until("the mail watcher to be ready", transport.ready.notified()).await;
    for _ in 0..3 {
        manager
            .execute(account.id, bot, turn, None, MailCommand::Folders)
            .await
            .unwrap();
    }
    assert_eq!(transport.scans.load(Ordering::SeqCst), 1);
    assert_eq!(transport.waits.load(Ordering::SeqCst), 1);
    let events: i64 = sqlx::query_scalar("SELECT count(*) FROM connector_events")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(events, 0);
}

#[tokio::test]
async fn a_scan_that_began_before_a_rejection_never_reports_connected() {
    for already_rejected in [false, true] {
        scan_then_rejection(already_rejected).await;
    }
}

async fn scan_then_rejection(already_rejected: bool) {
    let inbox = Arc::new(QuietInbox::default());
    inbox.hold_scan.store(true, Ordering::SeqCst);
    let h = recorder::harness(common::TestApp::builder().mail(inbox.clone())).await;
    let account = account(&h.app).await;
    let reject = || async {
        store::set_health(
            &h.app.db.pool,
            account.id,
            account.generation,
            &Health::CredentialRejected,
        )
        .await
        .unwrap()
    };
    if already_rejected {
        assert!(reject().await);
    }
    let manager = h.app.state.connectors.clone();
    manager.changed(account.id).await;
    tokio::time::timeout(common::SETTLE, inbox.scan_held.notified())
        .await
        .expect("the watch signs in and scans");
    assert_eq!(reject().await, !already_rejected);
    inbox.release_scan.notify_one();
    tokio::time::timeout(common::SETTLE, inbox.ready.notified())
        .await
        .expect("the held scan finishes and the watch waits");
    assert_eq!(
        store::account(&h.app.db.pool, account.id)
            .await
            .unwrap()
            .unwrap()
            .health,
        Health::CredentialRejected,
        "a sign-in older than the rejection cannot clear it (already rejected: {already_rejected})"
    );

    manager.changed(account.id).await;
    health_becomes(&h.app, account.id, Health::Connected).await;
}

#[tokio::test]
async fn duplicate_and_uncertain_send_retries_never_repeat_transport_effects() {
    for uncertain in [false, true] {
        let h = recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default())))
            .await;
        let bot: Uuid = h.app.main_bot().await.parse().unwrap();
        let (turn, _) = h.running(&bot.to_string(), GATE).await;
        let account = account(&h.app).await;
        store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
            .await
            .unwrap();
        let transport = Arc::new(Transport {
            sends: AtomicUsize::new(0),
            uncertain,
            recovers: None,
        });
        let manager = Arc::new(ConnectorManager::new(
            h.app.db.pool.clone(),
            h.app.state.secret_key.clone(),
            h.app.scheduler.clone(),
            h.app.state.gate.clone(),
            transport.clone(),
        ));
        let action = Uuid::new_v4();
        let command = MailCommand::Send {
            draft: Draft {
                to: vec!["owner@example.com".into()],
                cc: vec![],
                bcc: vec![],
                subject: "Report".into(),
                body: "Done".into(),
                attachments: vec![],
                reply_to: None,
                message_id: None,
            },
        };
        let first = manager
            .execute(account.id, bot, turn, Some(action), command.clone())
            .await
            .unwrap();
        let again = manager
            .execute(account.id, bot, turn, Some(action), command)
            .await
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(transport.sends.load(Ordering::SeqCst), 1);
        assert_eq!(
            first["status"],
            if uncertain { "uncertain" } else { "succeeded" }
        );
        let conflict = manager
            .execute(
                account.id,
                bot,
                turn,
                Some(action),
                MailCommand::CreateFolder {
                    folder: "other".into(),
                },
            )
            .await;
        assert!(conflict.is_err());
        store::set_permissions(&h.app.db.pool, account.id, bot, &Permissions::default())
            .await
            .unwrap();
        assert!(
            manager
                .execute(account.id, bot, turn, None, MailCommand::Folders)
                .await
                .is_err()
        );
        manager.forget(account.id).await;
    }
}

#[tokio::test]
async fn an_action_recovery_settled_first_reports_the_recorded_receipt() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let (turn, _) = h.running(&bot.to_string(), GATE).await;
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let manager = Arc::new(ConnectorManager::new(
        h.app.db.pool.clone(),
        h.app.state.secret_key.clone(),
        h.app.scheduler.clone(),
        h.app.state.gate.clone(),
        Arc::new(Transport {
            sends: AtomicUsize::new(0),
            uncertain: false,
            recovers: Some(h.app.db.pool.clone()),
        }),
    ));
    let action = Uuid::new_v4();
    let receipt = manager
        .execute(
            account.id,
            bot,
            turn,
            Some(action),
            MailCommand::CreateFolder {
                folder: "Receipts".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(receipt, json!({"action_id": action, "status": "uncertain"}));
    assert_eq!(
        store::ACTIONS
            .receipt(&h.app.db.pool, account.id, bot, action)
            .await
            .unwrap(),
        Some(receipt)
    );
    manager.forget(account.id).await;
}

#[tokio::test]
async fn store_refusals_reach_the_bot_under_their_own_codes() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let (turn, _) = h.running(&bot.to_string(), GATE).await;
    let account = account(&h.app).await;
    let pool = &h.app.db.pool;
    store::set_permissions(pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let manager = Arc::new(ConnectorManager::new(
        pool.clone(),
        h.app.state.secret_key.clone(),
        h.app.scheduler.clone(),
        h.app.state.gate.clone(),
        Arc::new(Transport {
            sends: AtomicUsize::new(0),
            uncertain: false,
            recovers: None,
        }),
    ));
    let folder = |name: &str| MailCommand::CreateFolder {
        folder: name.into(),
    };
    let code = |turn: Uuid, action: Option<Uuid>, command: MailCommand| {
        let manager = manager.clone();
        async move {
            manager
                .execute(account.id, bot, turn, action, command)
                .await
                .unwrap_err()
                .code
                .as_str()
        }
    };
    let action = Uuid::new_v4();
    manager
        .execute(account.id, bot, turn, Some(action), folder("One"))
        .await
        .unwrap();
    assert_eq!(
        code(turn, Some(action), folder("Two")).await,
        "mail_action_conflict"
    );
    store::set_active(pool, account.id, false).await.unwrap();
    assert_eq!(
        code(turn, None, MailCommand::Folders).await,
        "mail_account_paused"
    );
    store::set_active(pool, account.id, true).await.unwrap();
    store::set_permissions(pool, account.id, bot, &Permissions::default())
        .await
        .unwrap();
    assert_eq!(
        code(turn, None, MailCommand::Folders).await,
        "mail_forbidden"
    );
    store::set_permissions(pool, account.id, bot, &permissions())
        .await
        .unwrap();
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
    assert_eq!(
        code(turn, None, MailCommand::Folders).await,
        "mail_not_running"
    );
    manager.forget(account.id).await;
}

#[tokio::test]
async fn encrypted_backup_preserves_credentials_and_restore_cannot_run_queued_mail() {
    use else_api::backup::{
        DatabaseParams,
        capture::BackupConfig,
        restore::{RestoreRequest, restore},
        worker::BackupWorker,
    };
    use std::time::Duration;

    for in_flight in [false, true] {
        let app = TestApp::installed().await;
        let bot: Uuid = app.main_bot().await.parse().unwrap();
        let _closed = app
            .state
            .gate
            .close(common::SETTLE, else_api::maintenance::Pause::backup(None))
            .await
            .unwrap()
            .unwrap();
        let account = account(&app).await;
        store::set_permissions(&app.db.pool, account.id, bot, &permissions())
            .await
            .unwrap();
        let initial = Scan {
            cursor: Cursor {
                uid_validity: 1,
                last_uid: 0,
            },
            arrivals: vec![],
            more: false,
        };
        store::record_scan(&app.db.pool, account.id, account.generation, None, &initial)
            .await
            .unwrap();
        let arrival = Scan {
            cursor: Cursor {
                uid_validity: 1,
                last_uid: 1,
            },
            arrivals: vec![Arrival {
                message: MessageRef {
                    folder: "INBOX".into(),
                    uid_validity: 1,
                    uid: 1,
                },
                envelope: Default::default(),
            }],
            more: false,
        };
        store::record_scan(
            &app.db.pool,
            account.id,
            account.generation,
            Some(&initial.cursor),
            &arrival,
        )
        .await
        .unwrap();
        else_api::connectors::delivery::admit_pending(&app.db.pool, &app.state.registry, 32)
            .await
            .unwrap();
        let mail_turn: Uuid = sqlx::query_scalar(
            "SELECT turn_id FROM connector_deliveries WHERE turn_id IS NOT NULL LIMIT 1",
        )
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        let action_id = Uuid::new_v4();
        if in_flight {
            common::seed::force_status(&app.db.pool, mail_turn, TurnStatus::Running).await;
            store::begin_action(
                &app.db.pool,
                account.id,
                account.generation,
                bot,
                mail_turn,
                action_id,
                &MailCommand::Archive {
                    message: arrival.arrivals[0].message.clone(),
                },
            )
            .await
            .unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let params = DatabaseParams::parse(app.db.url.expose()).unwrap();
        let key = app.state.secret_key.clone();
        let worker = BackupWorker::new(
            app.db.pool.clone(),
            key.clone(),
            BackupConfig {
                dir: dir.path().join("backups"),
                keep: 1,
                max_bytes: 1 << 28,
                cadence: else_api::backup::capture::Cadence::Every(Duration::from_secs(3600)),
            },
        );
        let set = worker.run_once().await.unwrap();
        let target = dir.path().join("restored.sqlite");
        restore(RestoreRequest {
            set_dir: &set.path,
            key: &key,
            server: &params,
            target_database: target.to_str().unwrap(),
            staging_parent: &dir.path().join("staging"),
            max_bytes: 1 << 28,
            computers: None,
        })
        .await
        .unwrap();
        let restored = sqlx::SqlitePool::connect(&format!("sqlite://{}", target.display()))
            .await
            .unwrap();
        let account = store::account(&restored, account.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(account.state, AccountState::Paused);
        match store::credential(&restored, &key, account.id)
            .await
            .unwrap()
        {
            Credential::Password { ref password } => assert_eq!(password, "private-app-password"),
            _ => panic!("wrong credential kind"),
        }
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?1")
            .bind(mail_turn)
            .fetch_one(&restored)
            .await
            .unwrap();
        assert_eq!(
            status,
            if in_flight {
                "uncertain"
            } else {
                "interrupted"
            }
        );
        if in_flight {
            assert_eq!(
                store::ACTIONS
                    .receipt(&restored, account.id, bot, action_id)
                    .await
                    .unwrap()
                    .unwrap()["status"],
                "uncertain"
            );
            let uncertain: bool =
                sqlx::query_scalar("SELECT effect_uncertain FROM turns WHERE id=?1")
                    .bind(mail_turn)
                    .fetch_one(&restored)
                    .await
                    .unwrap();
            assert!(uncertain);
        }
        assert!(
            store::permissions(&restored, account.id, bot)
                .await
                .unwrap()
                .unwrap()
                .wake
        );
        assert_eq!(
            else_api::connectors::delivery::admit_pending(&restored, &app.state.registry, 32)
                .await
                .unwrap(),
            0
        );
    }
}
