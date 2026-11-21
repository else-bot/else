//! The computer list and the upgrades the owner starts from the API.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn owners_upgrade_from_the_api_while_turns_queue() {
    let db = common::TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        fake.clone(),
        ComputerConfig {
            image: "img:new".into(),
            ..config(1)
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry: common::Providers = vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )];
    let app = TestApp::builder()
        .db(Arc::new(db))
        .providers(registry)
        .computers(manager.clone())
        .start()
        .await;
    let bot: Uuid = app.create_bot("Upgrade").await.parse().unwrap();
    let path = format!("/v1/bots/{bot}/computer");

    let (turn, _) = ready(&manager, bot, "turn:gone").await;
    manager.release(turn).await.unwrap();
    wait_for_idle_stop(&app, &manager, bot).await;
    sqlx::query("UPDATE bot_computers SET image = 'img:old' WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    let shown = app.owner("GET", &path, None).await.body;
    assert_eq!(
        (&shown["image"], &shown["latest_image"], &shown["upgrade"]),
        (
            &serde_json::json!("img:old"),
            &serde_json::json!("img:new"),
            &serde_json::Value::Null
        )
    );

    let (held, _) = ready(&manager, bot, "cli:held").await;
    let (desktop, _) = ready(&manager, bot, &format!("desktop:{bot}")).await;
    let upgrade = format!("{path}/upgrade");
    assert_eq!(
        app.owner("POST", &upgrade, None).await.status,
        StatusCode::ACCEPTED
    );
    let failed = settled(&app, &path).await;
    assert_eq!(failed["upgrade"]["state"], "failed", "{failed}");
    assert!(
        failed["upgrade"]["error"]
            .as_str()
            .is_some_and(|e| e.contains("idle")),
        "{failed}"
    );
    assert_eq!(failed["image"], "img:old");

    manager.release(held).await.unwrap();
    *fake.create_delay.lock().unwrap() = Duration::from_millis(500);
    assert_eq!(
        app.owner("POST", &upgrade, None).await.status,
        StatusCode::ACCEPTED
    );
    let again = app.owner("POST", &upgrade, None).await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{:?}", again.body);
    assert_eq!(
        app.owner("GET", &path, None).await.body["upgrade"]["state"],
        "running"
    );
    assert!(
        matches!(
            manager.acquire(bot, "turn:during").await.unwrap(),
            Acquire::Queued { .. }
        ),
        "a turn queues instead of waiting for the upgrade"
    );
    assert!(
        app.state
            .gate
            .close(
                Duration::from_millis(50),
                else_api::maintenance::Pause::backup(None)
            )
            .await
            .is_err(),
        "a backup or an update waits for the upgrade"
    );
    let other: Uuid = app.create_bot("Neighbour").await.parse().unwrap();
    assert!(
        matches!(
            manager.acquire(other, "turn:neighbour").await.unwrap(),
            Acquire::Queued { .. }
        ),
        "the upgrade's computers hold the budget"
    );

    let done = settled(&app, &path).await;
    assert_eq!(
        (&done["image"], &done["upgrade"]),
        (&serde_json::json!("img:new"), &serde_json::Value::Null),
        "{done}"
    );
    let desktop_released: bool = sqlx::query_scalar(
        "SELECT released_at IS NOT NULL AND end_reason = 'upgrade' FROM computer_leases WHERE id = ?1",
    )
    .bind(desktop)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert!(desktop_released, "the owner's desktop session ended");
    let latest = app.owner("POST", &upgrade, None).await;
    assert_eq!(latest.status, StatusCode::CONFLICT, "{:?}", latest.body);
}

/// Bots have root in their computer, so the old computer reports the APT
/// packages the bot installed and the new one installs them again by name.
/// A package it cannot install is reported, without failing the upgrade.
#[tokio::test(flavor = "multi_thread")]
async fn upgrades_reinstall_the_bots_packages_and_report_the_ones_that_fail() {
    let db = common::TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        fake.clone(),
        ComputerConfig {
            image: "img:new".into(),
            ..config(1)
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry: common::Providers = vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )];
    let app = TestApp::builder()
        .db(Arc::new(db))
        .providers(registry)
        .computers(manager.clone())
        .start()
        .await;
    let bot: Uuid = app.create_bot("Root").await.parse().unwrap();
    let path = format!("/v1/bots/{bot}/computer");
    let (turn, _) = ready(&manager, bot, "turn:gone").await;
    manager.release(turn).await.unwrap();
    wait_for_idle_stop(&app, &manager, bot).await;
    sqlx::query("UPDATE bot_computers SET image = 'img:old' WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();

    *fake.packages.lock().unwrap() = vec!["ffmpeg".into(), "gone-tool:amd64".into()];
    let report = manager.upgrade_image(bot, "img:new", None).await.unwrap();
    assert_eq!(
        *fake.reinstalled.lock().unwrap(),
        vec!["ffmpeg".to_owned(), "gone-tool:amd64".to_owned()],
        "the new computer installs the bot's packages again"
    );
    assert_eq!(report.unrestored_packages, vec!["gone-tool:amd64"]);
    assert_eq!(report.image_description, "{\"image\":\"new\"}");
    assert_eq!(
        app.owner("GET", &path, None).await.body["upgrade"],
        serde_json::json!({ "state": "completed", "unrestored_packages": ["gone-tool:amd64"] }),
        "the owner sees what was not installed again"
    );

    manager.rollback_upgrade(bot).await.unwrap();
    assert_eq!(
        app.owner("GET", &path, None).await.body["upgrade"],
        serde_json::Value::Null,
        "the report belongs to the computer that upgrade made"
    );

    *fake.packages.lock().unwrap() = vec!["--allow-unauthenticated".into()];
    let refused = manager.upgrade_image(bot, "img:new", None).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.to_string().contains("unreadable")),
        "a name apt-get would read as an option is refused: {refused:?}"
    );

    *fake.packages.lock().unwrap() = vec!["ffmpeg".into()];
    let report = manager.upgrade_image(bot, "img:new", None).await.unwrap();
    assert!(report.unrestored_packages.is_empty());
    assert_eq!(
        app.owner("GET", &path, None).await.body["upgrade"],
        serde_json::Value::Null,
        "nothing to report when every package came back"
    );
}

async fn settled(app: &TestApp, path: &str) -> serde_json::Value {
    common::until(
        "the computer upgrade to finish",
        common::poll(|| async {
            let body = app.owner("GET", path, None).await.body;
            (body["upgrade"]["state"] != "running").then_some(body)
        }),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_interrupts_an_upgrade_and_the_next_start_removes_its_replacement() {
    let db = common::TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let fake = Arc::new(Fake::default());
    let config = || ComputerConfig {
        image: "img:new".into(),
        ..config(1)
    };
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        fake.clone(),
        config(),
    ));
    manager.reconcile_startup().await.unwrap();
    let registry: common::Providers = vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )];
    let app = TestApp::builder()
        .db(Arc::new(db))
        .providers(registry)
        .computers(manager.clone())
        .start()
        .await;
    let bot: Uuid = app.create_bot("Shutdown").await.parse().unwrap();
    let (turn, old_name) = ready(&manager, bot, "turn:gone").await;
    manager.release(turn).await.unwrap();
    wait_for_idle_stop(&app, &manager, bot).await;
    sqlx::query("UPDATE bot_computers SET image = 'img:old' WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();

    *fake.create_hold.lock().unwrap() = Some(Arc::new(tokio::sync::Semaphore::new(0)));
    let path = format!("/v1/bots/{bot}/computer");
    assert_eq!(
        app.owner("POST", &format!("{path}/upgrade"), None)
            .await
            .status,
        StatusCode::ACCEPTED
    );
    let replacement: String = common::until(
        "the upgrade replacement to be recorded",
        common::poll(|| async {
            sqlx::query_scalar(
            "SELECT to_runtime_name FROM computer_upgrades WHERE bot_id = ?1 AND state = 'started'",
        )
        .bind(bot)
        .fetch_optional(&app.db.pool)
        .await
        .unwrap()
        }),
    )
    .await;
    common::until(
        "the upgrade to be interrupted on shutdown",
        manager.stop_upgrades(),
    )
    .await;
    assert!(
        matches!(
            manager.acquire(bot, "turn:after").await.unwrap(),
            Acquire::Queued { .. }
        ),
        "the old computer stays down while the replacement may still run"
    );
    let image: String = sqlx::query_scalar("SELECT image FROM bot_computers WHERE bot_id = ?1")
        .bind(bot)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(image, "img:old", "the bot stays on its old computer");

    *fake.create_hold.lock().unwrap() = None;
    fake.computers.lock().unwrap().insert(
        replacement.clone(),
        Computer {
            state: RuntimeState::Running,
            files: HashMap::new(),
        },
    );
    let _restarted = self::manager(&app, &fake, config()).await;
    assert!(
        !fake.computers.lock().unwrap().contains_key(&replacement),
        "the next start removes the interrupted replacement"
    );
    assert!(fake.computers.lock().unwrap().contains_key(&old_name));
    let failed = app.owner("GET", &path, None).await.body;
    assert_eq!(failed["upgrade"]["state"], "failed", "{failed}");
}

/// An app whose computers use `img:new`, with three bots on `img:old`
/// computers, all stopped, and a fourth bot that never used one.
async fn outdated_fleet(
    config: ComputerConfig,
) -> (TestApp, Arc<Fake>, Arc<ComputerManager>, Vec<Uuid>) {
    let db = common::TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        fake.clone(),
        ComputerConfig {
            image: "img:new".into(),
            upgrade_retry: Duration::from_millis(100),
            ..config
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry: common::Providers = vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )];
    let app = TestApp::builder()
        .db(Arc::new(db))
        .providers(registry)
        .computers(manager.clone())
        .start()
        .await;
    let mut bots = Vec::new();
    for name in ["One", "Two", "Three"] {
        let bot: Uuid = app.create_bot(name).await.parse().unwrap();
        let (lease, _) = ready(&manager, bot, "turn:gone").await;
        manager.release(lease).await.unwrap();
        wait_for_idle_stop(&app, &manager, bot).await;
        bots.push(bot);
    }
    app.create_bot("Never").await;
    sqlx::query("UPDATE bot_computers SET image = 'img:old'")
        .execute(&app.db.pool)
        .await
        .unwrap();
    (app, fake, manager, bots)
}

/// `GET /v1/computers` once `done` holds for its computers, keyed by bot.
async fn computers_when(
    app: &TestApp,
    done: impl Fn(&HashMap<Uuid, serde_json::Value>) -> bool,
) -> (serde_json::Value, HashMap<Uuid, serde_json::Value>) {
    let deadline = tokio::time::Instant::now() + common::SETTLE;
    loop {
        let response = app.owner("GET", "/v1/computers", None).await;
        assert_eq!(response.status, StatusCode::OK, "{:?}", response.body);
        let by_bot: HashMap<Uuid, serde_json::Value> = response.body["computers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|computer| {
                (
                    computer["bot_id"].as_str().unwrap().parse().unwrap(),
                    computer.clone(),
                )
            })
            .collect();
        if done(&by_bot) {
            return (response.body, by_bot);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "computers never settled: {}",
            response.body
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn upgrade_state(computer: &serde_json::Value) -> &str {
    computer["upgrade"]["state"].as_str().unwrap_or("none")
}

#[tokio::test(flavor = "multi_thread")]
async fn computers_list_every_created_computer_with_its_use() {
    let (app, _fake, manager, bots) = outdated_fleet(config(1)).await;
    let (body, by_bot) = computers_when(&app, |by_bot| {
        by_bot
            .values()
            .all(|computer| computer["disk_used_bytes"].is_u64())
    })
    .await;
    assert_eq!(body["latest_image"], "img:new");
    assert_eq!(body["rollback_bytes"], serde_json::Value::Null);
    let listed: Vec<&str> = body["computers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|computer| computer["bot_id"].as_str().unwrap())
        .collect();
    let expected: Vec<String> = bots.iter().map(Uuid::to_string).collect();
    assert_eq!(
        listed, expected,
        "oldest bot first; a bot without a computer is left out"
    );
    let stopped = &by_bot[&bots[0]];
    assert_eq!(
        (
            &stopped["state"],
            &stopped["image"],
            &stopped["upgrade"],
            &stopped["cpu_percent"],
            &stopped["memory_used_bytes"],
        ),
        (
            &serde_json::json!("stopped"),
            &serde_json::json!("img:old"),
            &serde_json::Value::Null,
            &serde_json::Value::Null,
            &serde_json::Value::Null,
        ),
        "{stopped}"
    );
    assert_eq!(stopped["memory_limit_bytes"], 1024 * 1024 * 1024u64);
    assert_eq!(stopped["disk_used_bytes"], 1u64 << 20, "host blocks");
    assert_eq!(stopped["disk_size_bytes"], serde_json::Value::Null);
    assert!(stopped["last_used_at"].is_string(), "{stopped}");

    let (_lease, _) = ready(&manager, bots[0], "cli:held").await;
    let (_, by_bot) = computers_when(&app, |by_bot| by_bot[&bots[0]]["state"] == "running").await;
    let running = &by_bot[&bots[0]];
    assert_eq!(running["cpus"], 2);
    assert_eq!(running["cpu_percent"], 75.0, "a share of its own vCPUs");
    assert_eq!(running["memory_used_bytes"], 1u64 << 30);
    assert_eq!(running["memory_limit_bytes"], 2u64 << 30);
    assert_eq!(
        (&running["disk_used_bytes"], &running["disk_size_bytes"]),
        (
            &serde_json::json!(3u64 << 20),
            &serde_json::json!(8u64 << 30)
        ),
        "as the guest reports its disk"
    );
}

/// Upgrade all puts off a computer in use and the ones the budget has no
/// room for, retries them after the others, and upgrades each once it can.
#[tokio::test(flavor = "multi_thread")]
async fn upgrade_all_waits_for_busy_computers_and_the_budget_then_upgrades_each() {
    let (app, _fake, manager, bots) = outdated_fleet(config(1)).await;
    let (held, _) = ready(&manager, bots[2], "cli:held").await;
    let queued = app.owner("POST", "/v1/computers/upgrade-all", None).await;
    assert_eq!(queued.status, StatusCode::ACCEPTED, "{:?}", queued.body);
    let expected: Vec<String> = bots.iter().map(Uuid::to_string).collect();
    assert_eq!(queued.body["queued"], serde_json::json!(expected));

    let (_, by_bot) = computers_when(&app, |by_bot| {
        bots.iter()
            .all(|bot| upgrade_state(&by_bot[bot]) == "waiting")
    })
    .await;
    assert_eq!(
        by_bot[&bots[0]]["upgrade"]["reason"],
        "Other computers use the whole budget."
    );
    assert_eq!(
        by_bot[&bots[2]]["upgrade"]["reason"],
        "The bot is using its computer."
    );
    let again = app.owner("POST", "/v1/computers/upgrade-all", None).await;
    assert_eq!(
        again.body["queued"].as_array().map(Vec::len),
        Some(3),
        "queueing again adds nothing: {:?}",
        again.body
    );

    manager.release(held).await.unwrap();
    manager.tick(&no_cancel()).await.unwrap();
    let (body, _) = computers_when(&app, |by_bot| {
        bots.iter()
            .all(|bot| by_bot[bot]["image"] == "img:new" && by_bot[bot]["upgrade"].is_null())
    })
    .await;
    assert!(
        body["rollback_bytes"]
            .as_u64()
            .is_some_and(|bytes| bytes > 0),
        "{body}"
    );
}

/// One failed upgrade doesn't stop the queue, a single upgrade of a bot the
/// queue is upgrading is refused, and cancelling takes out the rest while
/// the running upgrade finishes.
#[tokio::test(flavor = "multi_thread")]
async fn upgrade_all_goes_on_after_a_failure_and_cancels_what_is_left() {
    let (app, fake, _manager, bots) = outdated_fleet(config(3)).await;
    sqlx::query("UPDATE bot_computers SET network = NULL WHERE bot_id = ?1")
        .bind(bots[0])
        .execute(&app.db.pool)
        .await
        .unwrap();
    let hold = Arc::new(tokio::sync::Semaphore::new(0));
    *fake.create_hold.lock().unwrap() = Some(hold.clone());
    let queued = app.owner("POST", "/v1/computers/upgrade-all", None).await;
    assert_eq!(queued.status, StatusCode::ACCEPTED, "{:?}", queued.body);

    let (_, by_bot) =
        computers_when(&app, |by_bot| upgrade_state(&by_bot[&bots[1]]) == "running").await;
    assert_eq!(upgrade_state(&by_bot[&bots[0]]), "failed");
    assert!(
        by_bot[&bots[0]]["upgrade"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("--network")),
        "{}",
        by_bot[&bots[0]]
    );
    assert_eq!(
        by_bot[&bots[2]]["upgrade"],
        serde_json::json!({ "state": "queued", "position": 1 })
    );
    let single = app
        .owner(
            "POST",
            &format!("/v1/bots/{}/computer/upgrade", bots[1]),
            None,
        )
        .await;
    assert_eq!(single.status, StatusCode::CONFLICT, "{:?}", single.body);

    let cancelled = app
        .owner("POST", "/v1/computers/upgrade-all/cancel", None)
        .await;
    assert_eq!(cancelled.status, StatusCode::OK);
    assert_eq!(
        cancelled.body["cancelled"],
        serde_json::json!([bots[2].to_string()])
    );
    hold.add_permits(1);
    let (_, by_bot) = computers_when(&app, |by_bot| by_bot[&bots[1]]["upgrade"].is_null()).await;
    assert_eq!(by_bot[&bots[1]]["image"], "img:new");
    assert_eq!(
        (&by_bot[&bots[2]]["image"], &by_bot[&bots[2]]["upgrade"]),
        (&serde_json::json!("img:old"), &serde_json::Value::Null)
    );
    assert_eq!(upgrade_state(&by_bot[&bots[0]]), "failed");
    let shown = app
        .owner("GET", &format!("/v1/bots/{}/computer", bots[0]), None)
        .await;
    assert_eq!(shown.body["upgrade"]["state"], "failed");
}

/// A backup or an update puts every queued computer off until it ends; a
/// computer whose disk is missing fails instead of waiting; and the disk
/// of the computers kept for rollback is counted after an upgrade and
/// after rolling it back.
#[tokio::test(flavor = "multi_thread")]
async fn upgrade_all_waits_for_maintenance_fails_missing_disks_and_counts_kept_computers() {
    let (app, _fake, manager, bots) = outdated_fleet(config(3)).await;
    sqlx::query("UPDATE bot_computers SET observed_state = 'missing' WHERE bot_id = ?1")
        .bind(bots[2])
        .execute(&app.db.pool)
        .await
        .unwrap();
    let closed = app
        .state
        .gate
        .close(common::SETTLE, else_api::maintenance::Pause::backup(None))
        .await
        .unwrap()
        .expect("closed by this test");
    // The API refuses owner requests while the gate is closed.
    let queued = manager.upgrade_all(app.state.gate.clone()).await.unwrap();
    assert_eq!(queued.len(), 3);
    let deadline = tokio::time::Instant::now() + common::SETTLE;
    loop {
        let statuses = manager.upgrade_statuses().await;
        let waiting = bots.iter().all(|bot| {
            statuses.get(bot)
                == Some(&else_api::computers::upgrade_queue::UpgradeStatus::Waiting(
                    "A backup or an update is running.".into(),
                ))
        });
        if waiting {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "never waiting: {statuses:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    drop(closed);
    let (body, by_bot) = computers_when(&app, |by_bot| {
        upgrade_state(&by_bot[&bots[2]]) == "failed"
            && bots[..2].iter().all(|bot| by_bot[bot]["upgrade"].is_null())
    })
    .await;
    assert!(
        by_bot[&bots[2]]["upgrade"]["error"]
            .as_str()
            .is_some_and(|error| error.contains("missing")),
        "{body}"
    );
    assert_eq!(by_bot[&bots[0]]["image"], "img:new");
    let kept = |body: &serde_json::Value| body["rollback_bytes"].as_u64();
    let (body, _) = computers_when_body(&app, |body| kept(body) == Some(2 << 20)).await;
    assert_eq!(kept(&body), Some(2 << 20), "the two old computers");

    manager.rollback_upgrade(bots[0]).await.unwrap();
    let (body, _) =
        computers_when_body(&app, |body| body["computers"][0]["image"] == "img:old").await;
    assert_eq!(
        kept(&body),
        Some(2 << 20),
        "the rolled-back replacement takes the old computer's place"
    );
}

/// [`computers_when`] on the whole body.
async fn computers_when_body(
    app: &TestApp,
    done: impl Fn(&serde_json::Value) -> bool,
) -> (serde_json::Value, HashMap<Uuid, serde_json::Value>) {
    let deadline = tokio::time::Instant::now() + common::SETTLE;
    loop {
        let (body, by_bot) = computers_when(app, |_| true).await;
        if done(&body) {
            return (body, by_bot);
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "computers never settled: {body}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A computer the runtime no longer has fails its upgrade instead of
/// waiting forever, and the queue goes on.
#[tokio::test(flavor = "multi_thread")]
async fn upgrade_all_fails_a_computer_the_runtime_lost() {
    let (app, fake, _manager, bots) = outdated_fleet(config(3)).await;
    let lost: String =
        sqlx::query_scalar("SELECT runtime_name FROM bot_computers WHERE bot_id = ?1")
            .bind(bots[1])
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    fake.computers.lock().unwrap().remove(&lost);
    let queued = app.owner("POST", "/v1/computers/upgrade-all", None).await;
    assert_eq!(queued.status, StatusCode::ACCEPTED, "{:?}", queued.body);
    let (_, by_bot) = computers_when(&app, |by_bot| {
        upgrade_state(&by_bot[&bots[1]]) == "failed"
            && [bots[0], bots[2]]
                .iter()
                .all(|bot| by_bot[bot]["image"] == "img:new")
    })
    .await;
    assert_eq!(by_bot[&bots[1]]["image"], "img:old");
}
