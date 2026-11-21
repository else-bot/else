//! Creating, leasing, stopping, and inspecting computers.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn computers_are_created_on_first_use_reused_and_kept_apart() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let a: Uuid = app.create_bot("A").await.parse().unwrap();
    let b: Uuid = app.create_bot("B").await.parse().unwrap();
    assert_eq!(
        fake.calls(),
        vec!["list".to_owned()],
        "bot creation starts no computer"
    );

    let (lease_a, name_a) = ready(&manager, a, "test:a").await;
    let (lease_b, name_b) = ready(&manager, b, "test:b").await;
    assert_ne!(name_a, name_b);
    let second = Duration::from_secs(1);
    manager
        .exec(lease_a, "write", &["/root/note", "A-only"], second)
        .await
        .unwrap();
    manager
        .exec(lease_b, "write", &["/root/note", "B-only"], second)
        .await
        .unwrap();
    let (again, _) = ready(&manager, a, "test:a").await;
    assert_eq!(again, lease_a);
    manager.release(lease_a).await.unwrap();
    let (lease_a, _) = ready(&manager, a, "test:a2").await;
    let creates = fake
        .calls()
        .iter()
        .filter(|c| c.starts_with("create"))
        .count();
    assert_eq!(creates, 2, "{:?}", fake.calls());
    assert!(
        fake.calls().iter().any(|c| c.contains("1cpu 1024MiB")),
        "1 vCPU and 1 GiB"
    );
    let read = |lease| manager.exec(lease, "read", &["/root/note"], second);
    assert_eq!(read(lease_a).await.unwrap().stdout, "A-only");
    assert_eq!(read(lease_b).await.unwrap().stdout, "B-only");
}

#[tokio::test(flavor = "multi_thread")]
async fn computers_mount_their_bots_project_directory() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let linked = dir.path().join("linked");
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    let manager = manager(
        &app,
        &fake,
        ComputerConfig {
            projects_dir: Some(linked.join("bots")),
            ..config(2)
        },
    )
    .await;
    let a: Uuid = app.create_bot("A").await.parse().unwrap();
    let b: Uuid = app.create_bot("B").await.parse().unwrap();
    ready(&manager, a, "test:a").await;
    ready(&manager, b, "test:b").await;
    let specs = fake.specs.lock().unwrap().clone();
    let resolved = real.canonicalize().unwrap().join("bots");
    for (spec, bot) in specs.iter().zip([a, b]) {
        assert_eq!(
            spec.mounts,
            vec![else_api::computers::Mount {
                host: resolved.join(bot.to_string()),
                guest: else_api::computers::ROOT_PROJECT.to_owned(),
                uid: 0,
            }]
        );
        let meta = std::fs::metadata(resolved.join(bot.to_string())).unwrap();
        assert!(meta.is_dir());
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
            0o700
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_stop_prevents_implicit_wake_until_resume_and_chat_still_works() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(manager(&app, &fake, config(2)).await);
    let bot = app.create_bot("Stopped").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let (_, name) = ready(&manager, bot_id, "test:x").await;
    manager.owner_stop(bot_id).await.unwrap();
    assert_eq!(observed(&app, bot_id).await, "stopped");
    assert!(fake.calls().contains(&format!("stop {name}")));
    assert_eq!(
        manager.acquire(bot_id, "test:y").await.unwrap(),
        Acquire::OwnerStopped
    );
    let reply = app
        .send(
            &bot,
            Uuid::new_v4(),
            "hello while stopped",
            Some("synthetic"),
        )
        .await;
    app.wait_turn(reply.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    manager.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, bot_id).await, "stopped");
    assert!(
        !fake.calls().iter().any(|c| c == &format!("start {name}")),
        "nothing woke it"
    );
    manager.owner_resume(bot_id).await.unwrap();
    assert_eq!(
        observed(&app, bot_id).await,
        "stopped",
        "resume starts nothing"
    );
    ready(&manager, bot_id, "test:z").await;
    assert!(fake.calls().contains(&format!("start {name}")));
    let app_state = app.state.clone();
    let _ = app_state;
    let shown = app
        .owner("GET", &format!("/v1/bots/{bot}/computer"), None)
        .await;
    assert_eq!(shown.status, StatusCode::OK);
    assert_eq!(shown.body["state"], "running");
}

#[tokio::test(flavor = "multi_thread")]
async fn admission_queues_beyond_the_budget_and_paused_computers_keep_their_reservation() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let pausing = ComputerConfig {
        idle_action: IdleAction::Pause,
        ..config(1)
    };
    let manager = manager(&app, &fake, pausing).await;
    let a: Uuid = app.create_bot("A").await.parse().unwrap();
    let b: Uuid = app.create_bot("B").await.parse().unwrap();
    let (lease, _) = ready(&manager, a, "test:a").await;
    assert!(matches!(
        manager.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    manager.release(lease).await.unwrap();
    manager.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, a).await, "paused");
    assert!(matches!(
        manager.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    let (lease, _) = ready(&manager, a, "test:a").await;
    manager.release(lease).await.unwrap();
    manager.owner_stop(a).await.unwrap();
    ready(&manager, b, "test:b").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_stop_waits_for_leases_and_expired_leases_of_live_work_are_kept() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let bot = app.create_bot("Idle").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let sent = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let (lease, _) = ready(&manager, bot_id, &format!("turn:{turn}")).await;
    sqlx::query(
        "UPDATE computer_leases SET expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-1 seconds') WHERE id = ?1",
    )
    .bind(lease)
    .execute(&app.db.pool)
    .await
    .unwrap();
    manager.tick(&never()).await.unwrap();
    assert_eq!(
        observed(&app, bot_id).await,
        "running",
        "live work keeps its lease"
    );
    let live: bool =
        sqlx::query_scalar("SELECT released_at IS NULL FROM computer_leases WHERE id = ?1")
            .bind(lease)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert!(live, "the expired lease was renewed for live work");
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(serde_json::json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    app.wait_turn(&turn, &["interrupted"]).await;
    sqlx::query(
        "UPDATE computer_leases SET expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-1 seconds') WHERE id = ?1",
    )
    .bind(lease)
    .execute(&app.db.pool)
    .await
    .unwrap();
    manager.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, bot_id).await, "stopped");
}

#[tokio::test(flavor = "multi_thread")]
async fn acquisition_racing_idle_stop_never_leaves_a_lease_on_a_stopped_computer() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(manager(&app, &fake, config(2)).await);
    let bot: Uuid = app.create_bot("Race").await.parse().unwrap();
    for round in 0..20 {
        let m = manager.clone();
        let acquire =
            tokio::spawn(async move { m.acquire(bot, &format!("test:{round}")).await.unwrap() });
        let m = manager.clone();
        let tick = tokio::spawn(async move { m.tick(&never()).await.unwrap() });
        let acquired = acquire.await.unwrap();
        tick.await.unwrap();
        if let Acquire::Ready { lease_id, .. } = acquired {
            let state = observed(&app, bot).await;
            assert_eq!(
                state, "running",
                "round {round}: a held computer stays running"
            );
            manager.release(lease_id).await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_inspects_without_waking_recreating_or_removing() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(3)).await;
    let kept: Uuid = app.create_bot("Kept").await.parse().unwrap();
    let lost: Uuid = app.create_bot("Lost").await.parse().unwrap();
    let paused: Uuid = app.create_bot("Paused").await.parse().unwrap();
    let (lease, kept_name) = ready(&manager, kept, "test:k").await;
    manager.release(lease).await.unwrap();
    let (_, lost_name) = ready(&manager, lost, "test:l").await;
    let (_, paused_name) = ready(&manager, paused, "test:p").await;
    fake.computers.lock().unwrap().remove(&lost_name);
    fake.set(&paused_name, RuntimeState::Paused).unwrap();
    fake.set(&kept_name, RuntimeState::Stopped).unwrap();
    sqlx::query("UPDATE bot_computers SET observed_state = 'stopping' WHERE bot_id = ?1")
        .bind(kept)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let before = fake.calls().len();
    let restarted = ComputerManager::new(app.db.pool.clone(), fake.clone(), config(3));
    restarted.reconcile_startup().await.unwrap();
    let after: Vec<String> = fake.calls()[before..].to_vec();
    assert_eq!(after, vec!["list".to_owned()], "startup only lists");
    assert_eq!(observed(&app, kept).await, "stopped");
    assert_eq!(observed(&app, lost).await, "missing");
    assert_eq!(observed(&app, paused).await, "paused");
    assert!(matches!(
        restarted.acquire(lost, "test:x").await.unwrap(),
        Acquire::Unavailable { .. }
    ));
    let creates = fake
        .calls()
        .iter()
        .filter(|c| c.starts_with(&format!("create {lost_name}")))
        .count();
    assert_eq!(creates, 1, "a missing computer is never recreated");
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_bot_stops_its_computer_and_keeps_it() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let bot = app.create_bot("Gone").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let (_, name) = ready(&manager, bot_id, "test:g").await;
    app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    manager.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, bot_id).await, "stopped");
    assert!(
        fake.computers.lock().unwrap().contains_key(&name),
        "the disk is kept"
    );
    assert_eq!(
        manager.acquire(bot_id, "test:h").await.unwrap(),
        Acquire::BotInactive
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_stop_keeps_its_reservation_and_is_retried() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(1)).await;
    let a: Uuid = app.create_bot("A").await.parse().unwrap();
    let b: Uuid = app.create_bot("B").await.parse().unwrap();
    ready(&manager, a, "test:a").await;
    fake.fail_stop
        .store(true, std::sync::atomic::Ordering::SeqCst);
    manager.owner_stop(a).await.unwrap();
    assert_eq!(observed(&app, a).await, "running");
    assert!(matches!(
        manager.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    fake.fail_stop
        .store(false, std::sync::atomic::Ordering::SeqCst);
    manager.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, a).await, "stopped");
    ready(&manager, b, "test:b").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_deleted_while_its_computer_starts_gets_no_lease_and_is_stopped() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    *fake.create_hold.lock().unwrap() = Some(hold.clone());
    let manager = Arc::new(manager(&app, &fake, config(2)).await);
    let bot = app.create_bot("Short").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let m = manager.clone();
    let acquiring = tokio::spawn(async move { m.acquire(bot_id, "test:s").await.unwrap() });
    common::until(
        "the guest runtime to start creating the computer",
        common::poll(|| std::future::ready((!fake.specs.lock().unwrap().is_empty()).then_some(()))),
    )
    .await;
    app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    hold.add_permits(1);
    assert_eq!(acquiring.await.unwrap(), Acquire::BotInactive);
    assert_eq!(observed(&app, bot_id).await, "stopped");
    let leases: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM computer_leases WHERE bot_id = ?1 AND released_at IS NULL",
    )
    .bind(bot_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(leases, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renewal_wins_over_expiry_and_exec_never_wakes_a_stopped_computer() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let bot: Uuid = app.create_bot("Lease").await.parse().unwrap();
    let (lease, _) = ready(&manager, bot, "test:l").await;
    sqlx::query(
        "UPDATE computer_leases SET expires_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-1 seconds') WHERE id = ?1",
    )
    .bind(lease)
    .execute(&app.db.pool)
    .await
    .unwrap();
    assert!(manager.renew(lease).await.unwrap());
    manager.tick(&never()).await.unwrap();
    assert_eq!(
        observed(&app, bot).await,
        "running",
        "the renewed lease still holds it"
    );
    manager.owner_stop(bot).await.unwrap();
    assert!(matches!(
        manager
            .exec(lease, "read", &["/x"], Duration::from_secs(1))
            .await,
        Err(else_api::computers::ComputerError::LeaseEnded)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_used_until_the_runtime_has_been_inspected() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    fake.fail_list
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let manager = ComputerManager::new(app.db.pool.clone(), fake.clone(), config(2));
    manager.reconcile_startup().await.unwrap();
    let bot: Uuid = app.create_bot("Wait").await.parse().unwrap();
    assert!(matches!(
        manager.acquire(bot, "test:w").await.unwrap(),
        Acquire::Queued { .. }
    ));
    fake.fail_list
        .store(false, std::sync::atomic::Ordering::SeqCst);
    manager.tick(&never()).await.unwrap();
    ready(&manager, bot, "test:w").await;
}

/// A desktop whose startup fails is not admitted: the use is told it
/// failed, the failure is recorded, and the next use starts it again.
/// A create that fails leaves the computer absent rather than missing, so
/// the next use creates it instead of reporting it lost.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_create_leaves_the_computer_absent_and_the_next_use_creates_it() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let bot: Uuid = app.create_bot("Create").await.parse().unwrap();

    fake.fail_create
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let failed = manager.acquire(bot, "test:a").await.unwrap();
    assert!(matches!(failed, Acquire::Unavailable { .. }), "{failed:?}");
    assert_eq!(observed(&app, bot).await, "absent");

    fake.fail_create
        .store(false, std::sync::atomic::Ordering::SeqCst);
    ready(&manager, bot, "test:a").await;
}

/// Startup inspection never applies a listing older than the computer's
/// last transition: an owner stop that finishes while the runtime is being
/// listed keeps its `stopped`.
#[tokio::test(flavor = "multi_thread")]
async fn startup_inspection_never_applies_a_listing_older_than_a_stop() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(manager(&app, &fake, config(2)).await);
    let bot: Uuid = app.create_bot("Race").await.parse().unwrap();
    let (lease, _) = ready(&manager, bot, "test:a").await;
    manager.release(lease).await.unwrap();

    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    *fake.list_hold.lock().unwrap() = Some(hold.clone());
    let inspection = tokio::spawn({
        let manager = manager.clone();
        async move { manager.reconcile_startup().await }
    });
    common::until(
        "runtime inspection to list computers",
        fake.list_entered.notified(),
    )
    .await;
    manager.owner_stop(bot).await.unwrap();
    assert_eq!(observed(&app, bot).await, "stopped");
    hold.add_permits(1);
    inspection.await.unwrap().unwrap();
    assert_eq!(observed(&app, bot).await, "stopped");
}

/// A stop only starts from running, paused, or error: an owner stop of a
/// computer already stopped, or missing from the runtime, asks the runtime
/// for nothing and leaves its state as recorded.
#[tokio::test(flavor = "multi_thread")]
async fn an_owner_stop_leaves_a_stopped_or_missing_computer_alone() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let bot: Uuid = app.create_bot("Stop").await.parse().unwrap();
    let (lease, _) = ready(&manager, bot, "test:a").await;
    manager.release(lease).await.unwrap();
    let stops = || {
        fake.calls()
            .iter()
            .filter(|c| c.starts_with("stop"))
            .count()
    };

    manager.owner_stop(bot).await.unwrap();
    assert_eq!(
        (stops(), observed(&app, bot).await.as_str()),
        (1, "stopped")
    );
    manager.owner_stop(bot).await.unwrap();
    assert_eq!(
        (stops(), observed(&app, bot).await.as_str()),
        (1, "stopped")
    );

    sqlx::query("UPDATE bot_computers SET observed_state = 'missing' WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    manager.owner_stop(bot).await.unwrap();
    assert_eq!(
        (stops(), observed(&app, bot).await.as_str()),
        (1, "missing")
    );
}
