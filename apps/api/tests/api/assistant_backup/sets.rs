//! What a backup set captures, and when one is taken.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn one_set_holds_the_database_and_every_computer_at_one_checkpoint() {
    let s = setup().await;
    let a = s.app.create_bot("Alpha").await;
    let b = s.app.create_bot("Beta").await;
    let (a_id, b_id): (Uuid, Uuid) = (a.parse().unwrap(), b.parse().unwrap());
    chat(&s.app, &a, "Alpha's chat").await;
    chat(&s.app, &b, "Beta's chat").await;
    for (bot, disk) in [
        (a_id, "alpha files + browser profile"),
        (b_id, "beta files"),
    ] {
        let Acquire::Ready {
            lease_id,
            runtime_name,
            ..
        } = s.manager.acquire(bot, "test:use").await.unwrap()
        else {
            panic!()
        };
        s.fake.write(&runtime_name, disk);
        s.manager.release(lease_id).await.unwrap();
    }
    s.manager.owner_stop(b_id).await.unwrap();
    let starts = s.fake.starts.load(SeqCst);
    let checkpoints_before = s.fake.execs.lock().unwrap().len();

    let summary = worker(&s, common::SETTLE).run_once().await.unwrap();
    let manifest = &summary.manifest;
    assert!(manifest.coverage.whole_assistant, "{:?}", manifest.coverage);
    assert_eq!(manifest.coverage.vm, Covered::Included);
    let entry = |id: Uuid| manifest.bots.iter().find(|b| b.id == id).unwrap().clone();
    let (ea, eb) = (entry(a_id), entry(b_id));
    assert_eq!((ea.vm, eb.vm), (VmState::Included, VmState::Included));
    let (ca, cb) = (ea.computer.unwrap(), eb.computer.unwrap());
    assert!(ca.was_running && !cb.was_running);
    assert_eq!(
        cb.desired_state,
        else_api::computers::Desired::Stopped,
        "the owner's stop is recorded"
    );
    assert!(ca.desktop_port.is_some() && ca.desktop_password.is_some());
    assert!(
        !ea.provider_sessions.is_empty(),
        "sessions recorded, not included"
    );
    assert_eq!(ea.provider_state, ProviderState::OmittedReauthRequired);
    let main = manifest
        .bots
        .iter()
        .find(|b| b.name != "Alpha" && b.name != "Beta")
        .unwrap();
    assert_eq!(main.vm, VmState::None);
    assert_eq!(
        manifest.artifacts.len(),
        4,
        "database, two disk archives and the shared MCP library"
    );
    assert_eq!(ca.image, cb.image);
    let vms: Vec<bool> = manifest
        .artifacts
        .iter()
        .filter(|x| x.kind == ArtifactKind::Vm)
        .map(|x| {
            let bot = x.bot_id.unwrap();
            entry(bot).computer.unwrap().carries_image
        })
        .collect();
    assert_eq!(
        vms,
        [true, false],
        "the shared image is stored once, in the first archive"
    );
    assert_eq!(
        s.fake.starts.load(SeqCst),
        starts,
        "no start for the backup"
    );
    let execs = s.fake.execs.lock().unwrap()[checkpoints_before..].to_vec();
    assert!(
        !execs.is_empty() && execs.iter().all(|e| e == "/usr/local/bin/else-checkpoint"),
        "{execs:?}"
    );
    let observed: Vec<String> =
        sqlx::query_scalar("SELECT observed_state FROM bot_computers ORDER BY bot_id")
            .fetch_all(&s.app.db.pool)
            .await
            .unwrap();
    assert!(observed.iter().all(|o| o == "stopped"));
    for bytes in stored_chunks(&summary.path) {
        assert!(bytes.starts_with(b"age-encryption.org/v1"));
    }
    assert!(
        !std::fs::read_dir(&summary.path).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".msb"))
    );

    let fresh = Arc::new(Fake::default());
    let new_dir = s.dir.path().join("restored-installation");
    std::fs::create_dir(&new_dir).unwrap();
    let target = Target::new(&s.app);
    let report = restore(RestoreRequest {
        set_dir: &summary.path,
        key: &key(),
        server: &target.params,
        target_database: &target.name,
        staging_parent: &s.dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: Some(ComputerTarget {
            runtime: Some(fresh.clone()),
            data_dir: &new_dir,
        }),
    })
    .await
    .unwrap();
    assert!(report.manifest.coverage.whole_assistant);
    let name = |id: Uuid| ComputerManager::runtime_name(id);
    assert_eq!(
        fresh.disk(&name(a_id)).unwrap(),
        "alpha files + browser profile"
    );
    assert_eq!(fresh.disk(&name(b_id)).unwrap(), "beta files");
    let specs = fresh.restored.lock().unwrap().clone();
    let spec_a = specs.iter().find(|s| s.name == name(a_id)).unwrap();
    assert_eq!(
        spec_a.network, "public",
        "the network policy is restored explicitly"
    );
    assert_eq!(spec_a.ports, vec![(ca.desktop_port.unwrap(), 6080)]);
    let password =
        std::fs::read_to_string(new_dir.join(format!("computers/{a_id}/vnc.pass"))).unwrap();
    assert_eq!(password.trim(), ca.desktop_password.unwrap());
    let mut conn = sqlx::SqliteConnection::connect(&target.url(&target.name))
        .await
        .unwrap();
    let computers: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT bot_id, observed_state, desired_state FROM bot_computers ORDER BY bot_id",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    for (bot, observed, desired) in &computers {
        assert_eq!(observed, "stopped");
        assert_eq!(desired, if *bot == b_id { "stopped" } else { "auto" });
    }
    let active: i64 =
        sqlx::query_scalar("SELECT count(*) FROM provider_sessions WHERE status = 'active'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
    assert_eq!(
        active, 0,
        "native sessions retired: context is rebuilt from SQLite"
    );
    let chats: Vec<String> =
        sqlx::query_scalar("SELECT text FROM items WHERE kind = 'user' ORDER BY text")
            .fetch_all(&mut conn)
            .await
            .unwrap();
    assert_eq!(chats, ["Alpha's chat", "Beta's chat"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_work_defers_the_set_and_a_failed_component_publishes_nothing() {
    let s = setup().await;
    let a = s.app.create_bot("Alpha").await;
    let a_id: Uuid = a.parse().unwrap();
    let backups = s.dir.path().join("backups");
    let reply = s
        .app
        .send(
            &a,
            Uuid::new_v4(),
            "busy [synthetic:hang]",
            Some("synthetic"),
        )
        .await;
    let hung = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    s.app.wait_turn(&hung, &["running"]).await;
    let result = worker(&s, Duration::from_millis(300)).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Deferred(_))),
        "{result:?}"
    );
    assert!(backup::completed_sets(&backups).unwrap().is_empty());
    assert!(!s.app.state.gate.is_closed(), "the gate reopens");
    s.app
        .owner(
            "POST",
            &format!("/v1/bots/{a}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    s.app.wait_turn(&hung, &["interrupted"]).await;

    let Acquire::Ready { lease_id, .. } = s.manager.acquire(a_id, "desktop:x").await.unwrap()
    else {
        panic!()
    };
    let result = worker(&s, common::SETTLE).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Deferred(ref m)) if m.contains("in use")),
        "{result:?}"
    );
    s.manager.release(lease_id).await.unwrap();

    s.fake.fail_export.store(true, SeqCst);
    let result = worker(&s, common::SETTLE).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Producer(ref m)) if m.contains("disk full")),
        "{result:?}"
    );
    assert!(backup::completed_sets(&backups).unwrap().is_empty());
    let leftovers = std::fs::read_dir(&backups)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().file_name() != ".lock")
        .count();
    assert_eq!(leftovers, 0);
    s.fake.fail_export.store(false, SeqCst);
    let set = worker(&s, common::SETTLE).run_once().await.unwrap();
    assert!(set.manifest.coverage.whole_assistant);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_gate_refuses_changes_and_desktop_wakes_while_closed() {
    let s = setup().await;
    let bot = s.app.main_bot().await;
    let closed = s
        .app
        .state
        .gate
        .close(
            Duration::from_secs(1),
            else_api::maintenance::Pause::backup(None),
        )
        .await
        .unwrap()
        .unwrap();
    let send = s
        .app
        .send(&bot, Uuid::new_v4(), "during backup", Some("synthetic"))
        .await;
    assert_eq!(
        send.status,
        axum::http::StatusCode::ACCEPTED,
        "{:?}",
        send.body
    );
    assert_eq!(
        send.body["turn"]["status"], "queued",
        "a message waits for the backup"
    );
    let queued = send.body["turn"]["id"].as_str().unwrap().to_owned();
    let rename = s
        .app
        .owner(
            "PATCH",
            &format!("/v1/bots/{bot}"),
            Some(json!({ "name": "Renamed during backup" })),
        )
        .await;
    assert_eq!(
        rename.body["error"]["code"], "maintenance",
        "{:?}",
        rename.body
    );
    let desktop = s
        .app
        .owner("GET", &format!("/v1/bots/{bot}/desktop"), None)
        .await;
    assert_eq!(desktop.body["error"]["code"], "maintenance");
    let read = s.app.owner("GET", "/v1/bots", None).await;
    assert!(read.status.is_success());
    let stop = s
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "Stop work stays available");
    drop(closed);
    let send = s
        .app
        .send(&bot, Uuid::new_v4(), "after backup", Some("synthetic"))
        .await;
    assert!(send.status.is_success());
    assert_eq!(
        s.app
            .wait_turn(&queued, &["interrupted", "cancelled"])
            .await,
        "interrupted",
        "the newer message supersedes the queued one"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_sent_during_a_backup_queues_behind_the_running_turn() {
    let h = recorder::harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (first, _) = h
        .running(&bot, &format!("{GATE} a long piece of work"))
        .await;
    let gate = app.state.gate.clone();
    let closed = gate
        .close(
            Duration::from_secs(1),
            else_api::maintenance::Pause::backup(None),
        )
        .await
        .unwrap()
        .unwrap();
    let second = app
        .send(&bot, Uuid::new_v4(), "a follow-up", Some("synthetic"))
        .await;
    assert_eq!(
        second.status,
        axum::http::StatusCode::ACCEPTED,
        "{:?}",
        second.body
    );
    let second = second.body["turn"]["id"].as_str().unwrap().to_owned();
    h.release(first).await;
    assert_eq!(
        app.wait_turn(first, &["completed", "cancelling", "interrupted"])
            .await,
        "completed",
        "the running turn finishes instead of being cancelled"
    );
    common::never_polled(
        "the follow-up leaves the queue while the gate is closed",
        Duration::from_millis(300),
        || async {
            let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
                .bind(second.parse::<Uuid>().unwrap())
                .fetch_one(&app.db.pool)
                .await
                .unwrap();
            (status != "queued").then_some(status)
        },
    )
    .await;
    drop(closed);
    app.scheduler.wake();
    assert_eq!(app.wait_turn(&second, &["completed"]).await, "completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn retention_keeps_every_artifact_a_kept_set_needs() {
    let s = setup().await;
    let a = s.app.create_bot("Alpha").await;
    let Acquire::Ready { lease_id, .. } = s
        .manager
        .acquire(a.parse().unwrap(), "test:use")
        .await
        .unwrap()
    else {
        panic!()
    };
    s.manager.release(lease_id).await.unwrap();
    let mut w = worker(&s, common::SETTLE);
    w.config.keep = 1;
    let first = w.run_once().await.unwrap();
    let second = w.run_once().await.unwrap();
    assert!(!first.path.exists(), "the older set is removed whole");
    let (_, index) = backup::store::open_index(&second.path, &key()).unwrap();
    let keys = backup::chunks::ChunkKeys::derive(&key()).unwrap();
    let store = backup::store::Store::open(second.path.parent().unwrap(), &keys);
    let named: std::collections::HashSet<&str> = index.chunk_ids().collect();
    for chunk in index.artifacts.iter().flat_map(|a| &a.chunks) {
        store.get(&keys, chunk).unwrap();
    }
    assert_eq!(
        stored_chunks(&second.path).len(),
        named.len(),
        "chunks only the removed set used are swept"
    );
    assert_eq!(index.manifest.artifacts.len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_checkpoints_and_desktop_secrets_are_required() {
    let s = setup().await;
    let a: Uuid = s.app.create_bot("Alpha").await.parse().unwrap();
    let name = used(&s, a).await;
    s.fake.checkpoint_fails.store(true, SeqCst);
    let result = worker(&s, common::SETTLE).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Deferred(ref m)) if m.contains("checkpoint")),
        "{result:?}"
    );
    assert_eq!(
        s.fake.state(&name).await.unwrap(),
        Some(RuntimeState::Running)
    );
    s.fake.checkpoint_fails.store(false, SeqCst);
    s.fake.set(&name, RuntimeState::Paused).unwrap();
    sqlx::query("UPDATE bot_computers SET observed_state = 'paused' WHERE bot_id = ?1")
        .bind(a)
        .execute(&s.app.db.pool)
        .await
        .unwrap();
    let before = s.fake.execs.lock().unwrap().len();
    let set = worker(&s, common::SETTLE).run_once().await.unwrap();
    assert!(
        s.fake.execs.lock().unwrap()[before..]
            .iter()
            .any(|e| e.ends_with("else-checkpoint"))
    );
    let entry = set.manifest.bots.iter().find(|b| b.id == a).unwrap();
    assert_eq!(
        entry
            .computer
            .as_ref()
            .unwrap()
            .browser_checkpoint
            .as_deref(),
        Some("ok")
    );
    assert!(set.manifest.coverage.whole_assistant);
    std::fs::remove_file(s.manager.desktop_password_file(a)).unwrap();
    let result = worker(&s, common::SETTLE).run_once().await;
    assert!(
        matches!(result, Err(BackupError::Producer(ref m)) if m.contains("password")),
        "{result:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn host_artifacts_are_captured_and_a_partial_set_never_replaces_a_whole_one() {
    let s = setup().await;
    let a: Uuid = s.app.create_bot("Alpha").await.parse().unwrap();
    used(&s, a).await;
    let id = Uuid::new_v4();
    let store = s.dir.path().join("state/artifacts").join(a.to_string());
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join(id.to_string()), b"REPORT-BYTES").unwrap();
    use sha2::Digest;
    sqlx::query(
        "INSERT INTO artifacts (id, bot_id, guest_path, size_bytes, sha256) VALUES (?1, ?2, '/r.txt', 12, ?3)",
    )
    .bind(id)
    .bind(a)
    .bind(sha2::Sha256::digest(b"REPORT-BYTES").to_vec())
    .execute(&s.app.db.pool)
    .await
    .unwrap();
    let mut w = worker(&s, common::SETTLE);
    w.config.keep = 1;
    let whole = w.run_once().await.unwrap();
    assert!(whole.manifest.coverage.whole_assistant);
    assert!(
        whole
            .manifest
            .artifacts
            .iter()
            .any(|x| x.object == format!("file-{id}.age"))
    );
    let fresh = Arc::new(Fake::default());
    let new_dir = s.dir.path().join("new");
    std::fs::create_dir(&new_dir).unwrap();
    let target = Target::new(&s.app);
    restore(RestoreRequest {
        set_dir: &whole.path,
        key: &key(),
        server: &target.params,
        target_database: &target.name,
        staging_parent: &s.dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: Some(ComputerTarget {
            runtime: Some(fresh),
            data_dir: &new_dir,
        }),
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(new_dir.join(format!("artifacts/{a}/{id}"))).unwrap(),
        b"REPORT-BYTES"
    );
    std::fs::remove_file(store.join(id.to_string())).unwrap();
    let partial = w.run_once().await.unwrap();
    assert!(!partial.manifest.coverage.whole_assistant);
    assert!(
        whole.path.join("complete.json").exists(),
        "the last whole set is kept"
    );
    assert!(partial.path.join("complete.json").exists());
}
