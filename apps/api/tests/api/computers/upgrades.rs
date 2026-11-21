//! Image upgrades and disk operations.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn image_upgrades_fail_cleanly_and_keep_the_old_computer() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let bot: Uuid = app.create_bot("Old").await.parse().unwrap();
    let (lease, old_name) = ready(&manager, bot, "turn:a").await;

    let held = manager.upgrade_image(bot, "img:new", None).await;
    assert!(held.is_err(), "{held:?}");
    assert_eq!(
        computer(&app, bot).await,
        (
            old_name.clone(),
            "python:3.12-slim".into(),
            "running".into()
        )
    );
    manager.release(lease).await.unwrap();

    fake.fail_unpack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let failed = manager.upgrade_image(bot, "img:new", None).await;
    assert!(
        failed
            .as_ref()
            .is_err_and(|e| e.to_string().contains("user ID")),
        "{failed:?}"
    );
    assert_eq!(
        computer(&app, bot).await,
        (
            old_name.clone(),
            "python:3.12-slim".into(),
            "stopped".into()
        )
    );
    let names: Vec<String> = fake.computers.lock().unwrap().keys().cloned().collect();
    assert_eq!(
        names,
        vec![old_name.clone()],
        "only the old computer is left"
    );
    assert_eq!(
        fake.computers.lock().unwrap()[&old_name].state,
        RuntimeState::Stopped
    );
    let (state, error): (String, String) =
        sqlx::query_as("SELECT state, error FROM computer_upgrades WHERE bot_id = ?1")
            .bind(bot)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(state, "failed");
    assert!(error.contains("user ID"));

    fake.fail_unpack
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let leftover = format!("{old_name}-leftover");
    fake.computers.lock().unwrap().insert(
        leftover.clone(),
        Computer {
            state: RuntimeState::Running,
            files: HashMap::new(),
        },
    );
    sqlx::query(
        "INSERT INTO computer_upgrades (id, bot_id, from_runtime_name, from_image, to_runtime_name, to_image, state)
         VALUES (?1, ?2, ?3, 'python:3.12-slim', ?4, 'img:new', 'started')",
    )
    .bind(Uuid::new_v4())
    .bind(bot)
    .bind(&old_name)
    .bind(&leftover)
    .execute(&app.db.pool)
    .await
    .unwrap();
    let report = manager.upgrade_image(bot, "img:new", None).await.unwrap();
    assert!(
        !fake.computers.lock().unwrap().contains_key(&leftover),
        "leftover removed"
    );
    assert_eq!(
        computer(&app, bot).await,
        (
            report.to_runtime_name.clone(),
            "img:new".into(),
            "stopped".into()
        )
    );
    assert_eq!(report.data_bytes, 4);
    assert_eq!(
        fake.computers.lock().unwrap()[&old_name].state,
        RuntimeState::Stopped,
        "old kept"
    );
    assert_eq!(
        fake.computers.lock().unwrap()[&report.to_runtime_name].state,
        RuntimeState::Stopped
    );
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT state, count(*) FROM computer_upgrades WHERE bot_id = ?1 GROUP BY state ORDER BY state",
    )
    .bind(bot)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(counts, vec![("completed".into(), 1), ("failed".into(), 2)]);

    let (lease, name) = ready(&manager, bot, "turn:b").await;
    assert_eq!(name, report.to_runtime_name);
    manager.release(lease).await.unwrap();
    let back = manager.rollback_upgrade(bot).await.unwrap();
    assert_eq!(back.to_runtime_name, old_name);
    assert_eq!(
        computer(&app, bot).await,
        (
            old_name.clone(),
            "python:3.12-slim".into(),
            "stopped".into()
        )
    );
    assert!(
        fake.computers
            .lock()
            .unwrap()
            .contains_key(&report.to_runtime_name),
        "newer kept"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn image_upgrades_keep_network_respect_budget_and_settle_offline() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let isolated = ComputerConfig {
        network: "none".into(),
        ..config(1)
    };
    let first = manager(&app, &fake, isolated).await;
    let bot: Uuid = app.create_bot("Offline").await.parse().unwrap();
    let (lease, old_name) = ready(&first, bot, "turn:gone").await;
    drop(first);
    let _ = lease;

    let manager = manager(&app, &fake, config(1)).await;
    let other: Uuid = app.create_bot("Other").await.parse().unwrap();
    assert!(manager.upgrade_image(bot, "img:new", None).await.is_err());
    manager.settle_offline(bot).await.unwrap();
    wait_for_idle_stop(&app, &manager, bot).await;
    let (other_lease, _) = ready(&manager, other, "turn:other").await;
    let busy = manager.upgrade_image(bot, "img:new", None).await;
    assert!(
        busy.as_ref()
            .is_err_and(|e| e.to_string().contains("budget")),
        "{busy:?}"
    );
    assert_eq!(
        fake.computers.lock().unwrap()[&old_name].state,
        RuntimeState::Stopped,
        "not booted"
    );
    manager.release(other_lease).await.unwrap();
    manager.tick(&no_cancel()).await.unwrap();

    let report = manager.upgrade_image(bot, "img:new", None).await.unwrap();
    let spec = fake.specs.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (spec.name.as_str(), spec.network.as_str()),
        (report.to_runtime_name.as_str(), "none")
    );
    let network: Option<String> =
        sqlx::query_scalar("SELECT network FROM bot_computers WHERE bot_id = ?1")
            .bind(bot)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(network.as_deref(), Some("none"));

    sqlx::query("UPDATE bot_computers SET network = NULL WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let unknown = manager.upgrade_image(bot, "img:third", None).await;
    assert!(
        unknown
            .as_ref()
            .is_err_and(|e| e.to_string().contains("--network")),
        "{unknown:?}"
    );
    let third = manager
        .upgrade_image(bot, "img:third", Some("public"))
        .await
        .unwrap();
    assert_eq!(fake.specs.lock().unwrap().last().unwrap().network, "public");
    manager.rollback_upgrade(bot).await.unwrap();
    let network: Option<String> =
        sqlx::query_scalar("SELECT network FROM bot_computers WHERE bot_id = ?1")
            .bind(bot)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(network, None);

    let leftover = format!("{}-leftover", third.to_runtime_name);
    fake.computers.lock().unwrap().insert(
        leftover.clone(),
        Computer {
            state: RuntimeState::Running,
            files: HashMap::new(),
        },
    );
    sqlx::query(
        "INSERT INTO computer_upgrades (id, bot_id, from_runtime_name, from_image, to_runtime_name, to_image, state)
         VALUES (?1, ?2, 'x', 'y', ?3, 'z', 'started')",
    )
    .bind(Uuid::new_v4())
    .bind(bot)
    .bind(&leftover)
    .execute(&app.db.pool)
    .await
    .unwrap();
    drop(manager);
    let _restarted = self::manager(&app, &fake, config(1)).await;
    assert!(
        !fake.computers.lock().unwrap().contains_key(&leftover),
        "startup removes an interrupted upgrade's replacement"
    );
    let started: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM computer_upgrades WHERE bot_id = ?1 AND state = 'started'",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(started, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn disk_operations_end_the_leases_of_finished_turns_and_jobs_only() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let bot = app.create_bot("After").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let refused = |r: &Result<_, else_api::computers::ComputerError>| r.is_err();

    let sent = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let (live_turn, _) = ready(&manager, bot_id, &format!("turn:{turn}")).await;
    let (live_job, done_job) = (Uuid::new_v4(), Uuid::new_v4());
    for (job, status) in [(live_job, "running"), (done_job, "completed")] {
        sqlx::query(
            "INSERT INTO guest_jobs (id, bot_id, command, status, deadline)
             VALUES (?1, ?2, 'sleep 60', ?3, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+1 hours'))",
        )
        .bind(job)
        .bind(bot_id)
        .bind(status)
        .execute(&app.db.pool)
        .await
        .unwrap();
    }
    let (live_job_lease, _) = ready(&manager, bot_id, &format!("job:{live_job}")).await;
    let (done_job_lease, _) = ready(&manager, bot_id, &format!("job:{done_job}")).await;
    let (finished, _) = ready(&manager, bot_id, &format!("turn:{}", Uuid::new_v4())).await;
    let (desktop, _) = ready(&manager, bot_id, &format!("desktop:{}", Uuid::new_v4())).await;

    let ended = |lease: Uuid| {
        let pool = app.db.pool.clone();
        async move {
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT end_reason FROM computer_leases WHERE id = ?1",
            )
            .bind(lease)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    assert!(refused(
        &manager.upgrade_image(bot_id, "img:new", None).await
    ));
    for lease in [finished, done_job_lease, live_turn, live_job_lease, desktop] {
        assert_eq!(ended(lease).await, None);
    }
    sqlx::query("UPDATE guest_jobs SET status = 'completed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1")
        .bind(live_job)
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert!(refused(
        &manager.upgrade_image(bot_id, "img:new", None).await
    ));
    for lease in [finished, done_job_lease, live_job_lease] {
        assert_eq!(ended(lease).await.as_deref(), Some("expired"));
    }
    for lease in [live_turn, desktop] {
        assert_eq!(ended(lease).await, None, "a live holder keeps its lease");
    }
    manager.release(desktop).await.unwrap();
    assert!(refused(
        &manager.upgrade_image(bot_id, "img:new", None).await
    ));
    assert_eq!(ended(live_turn).await, None, "the running turn still holds");
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(serde_json::json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    app.wait_turn(&turn, &["interrupted"]).await;
    manager
        .upgrade_image(bot_id, "img:new", None)
        .await
        .unwrap();
    assert_eq!(ended(live_turn).await.as_deref(), Some("expired"));
}

/// Images before else-guest:3 stop Chromium with SIGTERM, which drops
/// cookies set in its last 30 seconds. A desktop upgrade closes Chromium with
/// SIGINT first, only when nothing holds the computer, resumes a paused one
/// to do so, and refuses the upgrade, leaving the computer as it was, when
/// the close fails.
#[tokio::test(flavor = "multi_thread")]
async fn desktop_upgrades_close_chromium_with_sigint_before_the_stop() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let desktop = ComputerConfig {
        desktop: true,
        state_dir: dir.path().to_owned(),
        ..config(2)
    };
    let manager = manager(&app, &fake, desktop).await;
    let bot: Uuid = app.create_bot("Desk").await.parse().unwrap();
    let (lease, old_name) = ready(&manager, bot, &format!("desktop:{}", Uuid::new_v4())).await;
    let closes = |fake: &Fake| {
        fake.calls()
            .iter()
            .filter(|c| c.starts_with("close-browser"))
            .count()
    };

    assert!(manager.upgrade_image(bot, "img:new", None).await.is_err());
    assert_eq!(closes(&fake), 0);
    manager.release(lease).await.unwrap();

    fake.fail_close
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let failed = manager.upgrade_image(bot, "img:new", None).await;
    assert!(
        failed
            .as_ref()
            .is_err_and(|e| e.to_string().contains("closing Chromium")),
        "{failed:?}"
    );
    assert_eq!(observed(&app, bot).await, "running");
    assert_eq!(
        fake.computers.lock().unwrap()[&old_name].state,
        RuntimeState::Running
    );
    let attempts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM computer_upgrades WHERE bot_id = ?1")
            .bind(bot)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(attempts, 0);
    fake.fail_close
        .store(false, std::sync::atomic::Ordering::SeqCst);

    fake.set(&old_name, RuntimeState::Paused).unwrap();
    sqlx::query("UPDATE bot_computers SET observed_state = 'paused' WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    fake.calls.lock().unwrap().clear();
    manager.upgrade_image(bot, "img:new", None).await.unwrap();
    let calls = fake.calls();
    let at = |step: &str| {
        let call = format!("{step} {old_name}");
        calls
            .iter()
            .position(|c| *c == call)
            .unwrap_or_else(|| panic!("{call} in {calls:?}"))
    };
    assert!(
        at("resume") < at("close-browser")
            && at("close-browser") < at("checkpoint")
            && at("checkpoint") < at("stop"),
        "{calls:?}"
    );
    assert_eq!(closes(&fake), 1, "only the old computer's browser closes");
}
