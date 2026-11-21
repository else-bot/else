//! Desktop computers: boot settings, growing for the screen, and startup
//! failures.

use super::*;

/// Each boot sets the guest's clock to the owner's time zone, so the
/// browser the owner takes over reports the zone they live in.
#[tokio::test(flavor = "multi_thread")]
async fn desktop_boots_use_the_owners_time_zone() {
    let app = TestApp::installed().await;
    sqlx::query("UPDATE installation SET owner_timezone = 'Indian/Reunion'")
        .execute(&app.db.pool)
        .await
        .unwrap();
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let desktop = ComputerConfig {
        desktop: true,
        state_dir: dir.path().to_owned(),
        ..config(2)
    };
    let manager = manager(&app, &fake, desktop).await;
    let bot: Uuid = app.create_bot("Desk").await.parse().unwrap();
    let (_lease, name) = ready(&manager, bot, "test:zone").await;
    let calls = fake.calls();
    assert!(
        calls.contains(&format!("init {name} Indian/Reunion")),
        "{calls:?}"
    );
}

/// A desktop computer boots at its small size for commands. Its screen
/// starts, and it grows to the desktop size, only when something needs the
/// desktop, once per boot. Growth is admitted against the budget and held
/// until the computer stops; without room the screen runs at the boot size
/// and grows once room frees. Its next start boots small again.
#[tokio::test(flavor = "multi_thread")]
async fn desktop_computers_boot_small_and_grow_only_for_the_screen() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let base = ComputerConfig::default();
    let reserved = |cpus: u32, memory: u32| (cpus, memory + base.overhead_mib);
    let (small_cpus, small_memory) = reserved(base.cpus, base.memory_mib);
    let (big_cpus, big_memory) = reserved(base.desktop_cpus, base.desktop_memory_mib);
    let manager = manager(
        &app,
        &fake,
        ComputerConfig {
            desktop: true,
            state_dir: dir.path().to_owned(),
            max_cpus: big_cpus + small_cpus,
            max_memory_mib: (big_memory + small_memory) as u64,
            ..config(2)
        },
    )
    .await;
    let a: Uuid = app.create_bot("Screen").await.parse().unwrap();
    let b: Uuid = app.create_bot("Scripts").await.parse().unwrap();
    let count = |prefix: &str| {
        fake.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    };

    let (lease_a, name_a) = ready(&manager, a, "test:a").await;
    let (lease_b, name_b) = ready(&manager, b, "test:b").await;
    assert_eq!(count("desktop"), 0, "commands start no desktop");
    assert_eq!(count("init"), 2, "each boot runs the guest's setup");
    let spec = fake.specs.lock().unwrap()[0].clone();
    assert_eq!(
        (
            spec.cpus,
            spec.memory_mib,
            spec.max_cpus,
            spec.max_memory_mib
        ),
        (1, 1024, 2, 2048),
        "created small, able to grow"
    );

    let shown = manager.acquire_desktop(a, "test:a").await.unwrap();
    assert!(matches!(shown, Acquire::Ready { lease_id, .. } if lease_id == lease_a));
    let calls = fake.calls();
    let grew = calls
        .iter()
        .position(|c| c == &format!("grow {name_a} 2cpu 2048MiB"));
    let started = calls.iter().position(|c| c == &format!("desktop {name_a}"));
    assert!(
        grew.is_some() && grew < started,
        "grown before its screen starts: {calls:?}"
    );
    manager.acquire_desktop(a, "test:a").await.unwrap();
    assert_eq!((count("grow"), count("desktop")), (1, 1), "once per boot");

    let small = manager.acquire_desktop(b, "test:b").await.unwrap();
    assert!(matches!(small, Acquire::Ready { .. }), "{small:?}");
    assert_eq!(
        (
            count(&format!("grow {name_b}")),
            count(&format!("desktop {name_b}"))
        ),
        (0, 1),
        "a screen at the boot size without room to grow"
    );
    manager
        .exec(
            lease_b,
            "write",
            &["/root/note", "still works"],
            Duration::from_secs(1),
        )
        .await
        .unwrap();

    manager.release(lease_a).await.unwrap();
    manager.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, a).await, "stopped");
    assert!(matches!(
        manager.acquire_desktop(b, "test:b").await.unwrap(),
        Acquire::Ready { .. }
    ));
    assert_eq!(
        (
            count(&format!("grow {name_b}")),
            count(&format!("desktop {name_b}"))
        ),
        (1, 1),
        "grown once room freed, its screen kept"
    );

    ready(&manager, a, "test:a2").await;
    let calls = fake.calls();
    let configured = calls
        .iter()
        .rposition(|c| c == &format!("configure {name_a} 1cpu 1024MiB up to 2cpu 2048MiB"));
    let restarted = calls.iter().rposition(|c| c == &format!("start {name_a}"));
    assert!(
        configured.is_some() && configured < restarted,
        "boots small again: {calls:?}"
    );
    assert_eq!(
        count(&format!("desktop {name_a}")),
        1,
        "a restart starts no desktop"
    );
}

/// A computer can grow to the desktop size however its row was first
/// recorded: by a reserved background job or an owner stop as much as by
/// a first use.
#[tokio::test(flavor = "multi_thread")]
async fn computers_first_recorded_by_a_job_or_an_owner_stop_can_grow() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let manager = manager(
        &app,
        &fake,
        ComputerConfig {
            desktop: true,
            state_dir: dir.path().to_owned(),
            ..config(4)
        },
    )
    .await;
    let job: Uuid = app.create_bot("Job").await.parse().unwrap();
    let stopped: Uuid = app.create_bot("Stopped").await.parse().unwrap();
    let mut conn = app.db.pool.acquire().await.unwrap();
    manager
        .reserve_job(
            &mut conn,
            job,
            None,
            "true",
            None,
            Duration::from_secs(60),
            &[],
            None,
        )
        .await
        .unwrap()
        .expect("reserved");
    drop(conn);
    manager.owner_stop(stopped).await.unwrap();
    manager.owner_resume(stopped).await.unwrap();

    ready(&manager, job, "test:job").await;
    ready(&manager, stopped, "test:stopped").await;
    let limits: Vec<(String, u32, u32)> = fake
        .specs
        .lock()
        .unwrap()
        .iter()
        .map(|spec| (spec.name.clone(), spec.max_cpus, spec.max_memory_mib))
        .collect();
    assert_eq!(
        limits,
        vec![
            (ComputerManager::runtime_name(job), 2, 2048),
            (ComputerManager::runtime_name(stopped), 2, 2048),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_desktop_startup_is_not_reported_ready() {
    use std::sync::atomic::Ordering::SeqCst;
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let desktop = ComputerConfig {
        desktop: true,
        state_dir: dir.path().to_owned(),
        ..config(2)
    };
    let manager = manager(&app, &fake, desktop).await;
    let bot: Uuid = app.create_bot("Blank").await.parse().unwrap();
    let (_lease, name) = ready(&manager, bot, "test:screen").await;

    fake.fail_desktop.store(true, SeqCst);
    let refused = manager.acquire_desktop(bot, "test:screen").await.unwrap();
    assert!(
        matches!(&refused, Acquire::DesktopFailed { reason } if reason.contains("Xvfb did not start")),
        "{refused:?}"
    );
    let input = manager
        .run_input(
            bot,
            "test:screen",
            "true",
            Duration::from_secs(1),
            &else_api::db::task_secrets::Env::default(),
        )
        .await
        .unwrap();
    assert!(
        matches!(input, Err(Acquire::DesktopFailed { .. })),
        "{input:?}"
    );
    let error: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM bot_computers WHERE bot_id = ?1")
            .bind(bot)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert!(
        error.is_some_and(|e| e.contains("Xvfb did not start")),
        "the failure is recorded"
    );

    fake.fail_desktop.store(false, SeqCst);
    let started = manager.acquire_desktop(bot, "test:screen").await.unwrap();
    assert!(matches!(started, Acquire::Ready { .. }), "{started:?}");
    let starts = fake
        .calls()
        .iter()
        .filter(|c| **c == format!("desktop {name}"))
        .count();
    assert_eq!(starts, 3, "each use retries until the desktop starts");
}

/// An owner or viewer request refused because the desktop did not start
/// holds no lease, so it neither blocks the bot's input as an open owner
/// session nor keeps the computer awake; both work once the desktop starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_desktop_request_refused_at_startup_holds_no_lease() {
    use std::sync::atomic::Ordering::SeqCst;
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let desktop = ComputerConfig {
        desktop: true,
        state_dir: dir.path().to_owned(),
        ..config(2)
    };
    let manager = manager(&app, &fake, desktop).await;
    let bot: Uuid = app.create_bot("Blank").await.parse().unwrap();
    ready(&manager, bot, "test:screen").await;
    let held = |holder: String| {
        let pool = app.db.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM computer_leases
                 WHERE bot_id = ?1 AND holder = ?2 AND released_at IS NULL",
            )
            .bind(bot)
            .bind(holder)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    fake.fail_desktop.store(true, SeqCst);
    assert!(manager.desktop(bot).await.unwrap().is_err());
    assert_eq!(held(format!("desktop:{bot}")).await, 0, "owner lease");
    let viewer = manager.viewer(bot, "stream:test", true).await.unwrap();
    assert!(viewer.is_err(), "{viewer:?}");
    assert_eq!(held("stream:test".into()).await, 0, "viewer lease");

    fake.fail_desktop.store(false, SeqCst);
    let input = manager
        .run_input(
            bot,
            "test:screen",
            "true",
            Duration::from_secs(1),
            &else_api::db::task_secrets::Env::default(),
        )
        .await;
    assert!(matches!(input, Ok(Ok(_))), "{input:?}");
    assert!(manager.desktop(bot).await.unwrap().is_ok());
    assert!(
        manager
            .viewer(bot, "stream:test", true)
            .await
            .unwrap()
            .is_ok()
    );
}

/// An owner request whose desktop failed to start releases the shared
/// `desktop:` lease before the next owner request can reuse it, so a request
/// admitted after it keeps its lease (the input exclusion) live.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_owner_request_never_releases_a_later_requests_lease() {
    use std::sync::atomic::Ordering::SeqCst;
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let desktop = ComputerConfig {
        desktop: true,
        state_dir: dir.path().to_owned(),
        ..config(2)
    };
    let manager = Arc::new(manager(&app, &fake, desktop).await);
    let bot: Uuid = app.create_bot("Blank").await.parse().unwrap();
    ready(&manager, bot, "test:screen").await;

    fake.fail_desktop.store(true, SeqCst);
    fake.hold_desktop.store(true, SeqCst);
    let failing = tokio::spawn({
        let manager = manager.clone();
        async move { manager.desktop(bot).await.unwrap() }
    });
    common::until(
        "the first desktop request to start",
        fake.desktop_entered.notified(),
    )
    .await;
    fake.fail_desktop.store(false, SeqCst);
    let mut later = tokio::spawn({
        let manager = manager.clone();
        async move { manager.desktop(bot).await.unwrap() }
    });
    fake.desktop_resume.notify_one();
    common::until("the later desktop request to start", async {
        tokio::select! {
            () = fake.desktop_entered.notified() => {}
            refused = &mut later => panic!("the later request never started the desktop: {refused:?}"),
        }
    })
    .await;
    let failed = failing.await.unwrap();
    assert!(
        matches!(failed, Err(Acquire::DesktopFailed { .. })),
        "{failed:?}"
    );
    fake.hold_desktop.store(false, SeqCst);
    fake.desktop_resume.notify_one();
    let admitted = later.await.unwrap().expect("the later request is admitted");
    let live: bool =
        sqlx::query_scalar("SELECT released_at IS NULL FROM computer_leases WHERE id = ?1")
            .bind(admitted.lease_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert!(live, "the admitted owner request still holds its lease");
}

/// A computer that grew is counted at its maximum until a reset of its boot
/// size succeeds: through a failed stop, and on a restart whose reset failed,
/// which waits for room for its maximum.
#[tokio::test(flavor = "multi_thread")]
async fn grown_computers_stay_counted_until_their_size_is_reset() {
    use std::sync::atomic::Ordering::SeqCst;
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let dir = tempfile::tempdir().unwrap();
    let base = ComputerConfig::default();
    let (small_cpus, small_memory) = (base.cpus, base.memory_mib + base.overhead_mib);
    let (big_cpus, big_memory) = (
        base.desktop_cpus,
        base.desktop_memory_mib + base.overhead_mib,
    );
    let manager = manager(
        &app,
        &fake,
        ComputerConfig {
            desktop: true,
            state_dir: dir.path().to_owned(),
            max_cpus: big_cpus + small_cpus,
            max_memory_mib: (big_memory + small_memory) as u64,
            ..config(2)
        },
    )
    .await;
    let a: Uuid = app.create_bot("Grown").await.parse().unwrap();
    let b: Uuid = app.create_bot("Other").await.parse().unwrap();
    let grows = |name: &str| {
        fake.calls()
            .iter()
            .filter(|c| c.starts_with(&format!("grow {name}")))
            .count()
    };
    let (_, name_a) = ready(&manager, a, "test:a").await;
    manager.acquire_desktop(a, "test:a").await.unwrap();
    assert_eq!(grows(&name_a), 1);

    fake.fail_stop.store(true, SeqCst);
    let _ = manager.owner_stop(a).await;
    assert_ne!(observed(&app, a).await, "stopped", "the stop failed");
    fake.fail_stop.store(false, SeqCst);
    let (_, name_b) = ready(&manager, b, "test:b").await;
    manager.acquire_desktop(b, "test:b").await.unwrap();
    assert_eq!(
        grows(&name_b),
        0,
        "a failed stop still holds the grown size"
    );

    manager.owner_stop(a).await.unwrap();
    manager.owner_resume(a).await.unwrap();
    manager.acquire_desktop(b, "test:b").await.unwrap();
    assert_eq!(grows(&name_b), 1, "room once it stopped");

    fake.fail_configure.store(true, SeqCst);
    let waiting = manager.acquire(a, "test:a2").await.unwrap();
    assert!(
        matches!(&waiting, Acquire::Queued { .. }),
        "an unreset restart needs room for its maximum: {waiting:?}"
    );
    fake.fail_configure.store(false, SeqCst);
    let (_, _) = ready(&manager, a, "test:a2").await;
}
