//! Restoring computers, their projects, and older set schemas.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_set_with_computers_needs_a_new_installation_to_restore() {
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
    let summary = worker(&s, common::SETTLE).run_once().await.unwrap();
    let target = Target::new(&s.app);
    let result = restore(RestoreRequest {
        set_dir: &summary.path,
        key: &key(),
        server: &target.params,
        target_database: &target.name,
        staging_parent: &s.dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: None,
    })
    .await;
    assert!(matches!(result, Err(BackupError::Refused(_))), "{result:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_restore_stops_what_it_started_or_keeps_the_installation() {
    let s = setup().await;
    let a: Uuid = s.app.create_bot("Alpha").await.parse().unwrap();
    let name = used(&s, a).await;
    let set = worker(&s, common::SETTLE).run_once().await.unwrap();
    for stop_fails in [false, true] {
        let fresh = Arc::new(Fake::default());
        fresh.restore_fails_running.store(true, SeqCst);
        fresh.stop_fails.store(stop_fails, SeqCst);
        let new_dir = s.dir.path().join(format!("new-{stop_fails}"));
        std::fs::create_dir(&new_dir).unwrap();
        let target = Target::new(&s.app);
        let result = restore(RestoreRequest {
            set_dir: &set.path,
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
        .await;
        let state = fresh.computers.lock().unwrap().get(&name).map(|c| c.0);
        if stop_fails {
            assert!(
                matches!(result, Err(BackupError::Leftover(_))),
                "{result:?}"
            );
        } else {
            assert!(
                matches!(result, Err(BackupError::Producer(_))),
                "{result:?}"
            );
            assert_eq!(
                state,
                Some(RuntimeState::Stopped),
                "what it started was stopped"
            );
        }
        let exists = target
            .params
            .with_database(&target.name)
            .options
            .get_filename()
            .exists();
        assert!(!exists, "the new database was dropped");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn project_directories_are_backed_up_and_restored_with_their_mounts() {
    let s = setup_with(true).await;
    let bots = s.dir.path().join("bots");
    let a = s.app.create_bot("Alpha").await;
    let b = s.app.create_bot("Beta").await;
    let (a_id, b_id): (Uuid, Uuid) = (a.parse().unwrap(), b.parse().unwrap());
    chat(&s.app, &a, "Alpha's chat").await;
    let Acquire::Ready {
        lease_id,
        runtime_name,
        ..
    } = s.manager.acquire(a_id, "test:use").await.unwrap()
    else {
        panic!()
    };
    s.fake.write(&runtime_name, "alpha disk");
    s.manager.release(lease_id).await.unwrap();
    let pa = bots.join(a_id.to_string());
    std::fs::create_dir_all(pa.join("sub")).unwrap();
    std::fs::write(pa.join("notes.md"), "alpha notes").unwrap();
    std::fs::write(pa.join("sub/data.csv"), "a,b\n").unwrap();
    std::os::unix::fs::symlink("sub/data.csv", pa.join("latest")).unwrap();
    std::fs::write(pa.join("store.js"), "export {}").unwrap();
    std::fs::hard_link(pa.join("store.js"), pa.join("sub/linked.js")).unwrap();
    let pb = bots.join(b_id.to_string());
    std::fs::create_dir_all(&pb).unwrap();
    std::fs::write(pb.join("sheet.csv"), "item,qty\n").unwrap();

    let summary = worker(&s, common::SETTLE).run_once().await.unwrap();
    let manifest = &summary.manifest;
    assert!(manifest.coverage.whole_assistant, "{:?}", manifest.coverage);
    assert_eq!(manifest.coverage.projects, Covered::Included);
    let entry = |id: Uuid| manifest.bots.iter().find(|e| e.id == id).unwrap().clone();
    let (ea, eb) = (entry(a_id), entry(b_id));
    assert_eq!(
        (ea.project, eb.project),
        (ProjectState::Included, ProjectState::Included)
    );
    assert_eq!((ea.vm, eb.vm), (VmState::Included, VmState::None));
    let main = manifest
        .bots
        .iter()
        .find(|e| e.name != "Alpha" && e.name != "Beta")
        .unwrap();
    assert_eq!(
        main.project,
        ProjectState::None,
        "no directory, nothing to include"
    );
    let mount = ea.computer.as_ref().unwrap().project_mount.clone().unwrap();
    assert_eq!(
        (mount.guest.as_str(), mount.uid),
        ("/home/elsebot/work", 1000),
        "a desktop computer's elsebot user owns the project"
    );
    for bot in [a_id, b_id] {
        let object = backup::projects::Format::TarZstd.object(bot);
        assert!(
            manifest
                .artifacts
                .iter()
                .any(|x| x.object == object && x.kind == ArtifactKind::Project)
        );
    }
    for bytes in stored_chunks(&summary.path) {
        assert!(bytes.starts_with(b"age-encryption.org/v1"));
        assert!(!bytes.windows(11).any(|w| w == b"alpha notes"));
    }

    let fresh = Arc::new(Fake::default());
    let new_dir = s.dir.path().join("restored-installation");
    std::fs::create_dir(&new_dir).unwrap();
    let target = Target::new(&s.app);
    restore(RestoreRequest {
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
    let projects = else_api::backup::restore::projects_dir(&new_dir);
    let ra = projects.join(a_id.to_string());
    assert_eq!(
        std::fs::read_to_string(ra.join("notes.md")).unwrap(),
        "alpha notes"
    );
    assert_eq!(
        std::fs::read_to_string(ra.join("sub/data.csv")).unwrap(),
        "a,b\n"
    );
    assert_eq!(
        std::fs::read_link(ra.join("latest")).unwrap(),
        Path::new("sub/data.csv")
    );
    use std::os::unix::fs::MetadataExt;
    assert_eq!(
        std::fs::metadata(ra.join("store.js")).unwrap().ino(),
        std::fs::metadata(ra.join("sub/linked.js")).unwrap().ino(),
        "hard links restore as one file"
    );
    assert_eq!(
        std::fs::read_to_string(projects.join(b_id.to_string()).join("sheet.csv")).unwrap(),
        "item,qty\n"
    );
    let spec = fresh.restored.lock().unwrap()[0].clone();
    assert_eq!(
        spec.mounts,
        vec![Mount {
            host: ra.canonicalize().unwrap(),
            guest: "/home/elsebot/work".into(),
            uid: 1000,
        }],
        "the restored computer binds the restored project"
    );

    // An entry that cannot be read is left out, and the set says so.
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        use std::os::unix::fs::PermissionsExt;
        let locked = pb.join("locked.csv");
        std::fs::write(&locked, "secret").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let partial = worker(&s, common::SETTLE).run_once().await.unwrap();
        let manifest = &partial.manifest;
        assert_eq!(manifest.coverage.projects, Covered::Partial);
        assert!(!manifest.coverage.whole_assistant);
        assert_eq!(
            manifest.bots.iter().find(|e| e.id == b_id).unwrap().project,
            ProjectState::Partial
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
}

/// Rewrite a set's manifest (and its completion record) with `edit`.
fn rewrite_manifest(set: &Path, edit: impl FnOnce(&mut serde_json::Value, &age::x25519::Identity)) {
    let (mut completion, manifest, identity) = backup::restore::open_set(set, &key()).unwrap();
    let mut value = serde_json::to_value(manifest).unwrap();
    edit(&mut value, &identity);
    completion.schema_version = value["schema_version"].as_u64().unwrap() as u32;
    let bytes = serde_json::to_vec(&value).unwrap();
    let manifest_path = set.join(backup::MANIFEST);
    std::fs::remove_file(&manifest_path).unwrap();
    let sealed = backup::seal(
        &manifest_path,
        &identity,
        &mut bytes.as_slice(),
        backup::MAX_MANIFEST_BYTES,
    )
    .unwrap();
    completion.manifest.ciphertext_bytes = sealed.ciphertext_bytes;
    completion.manifest.ciphertext_sha256 = sealed.ciphertext_sha256;
    std::fs::write(
        set.join(backup::COMPLETE),
        serde_json::to_vec(&completion).unwrap(),
    )
    .unwrap();
}

async fn computers_with_projects(s: &Setup) -> (Uuid, Uuid) {
    let a: Uuid = s.app.create_bot("Alpha").await.parse().unwrap();
    let b: Uuid = s.app.create_bot("Beta").await.parse().unwrap();
    for (bot, disk) in [(a, "alpha disk"), (b, "beta disk")] {
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
        let project = s.dir.path().join("bots").join(bot.to_string());
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("notes.md"), format!("{disk} notes")).unwrap();
    }
    (a, b)
}

async fn restore_fresh(s: &Setup, set: &Path, name: &str) -> (Arc<Fake>, std::path::PathBuf) {
    let fresh = Arc::new(Fake::default());
    let new_dir = s.dir.path().join(name);
    std::fs::create_dir(&new_dir).unwrap();
    let target = Target::new(&s.app);
    restore(RestoreRequest {
        set_dir: set,
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
    (fresh, new_dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn schema_2_sets_with_plain_project_tars_still_restore() {
    let s = setup_with(true).await;
    let (a, b) = computers_with_projects(&s).await;
    let summary = worker(&s, common::SETTLE).run_once().await.unwrap();
    let (_whole, whole) = common::rebuilt_set(&summary.path, &key());
    rewrite_manifest(&whole, |manifest, identity| {
        manifest["schema_version"] = json!(2);
        for bot in manifest["bots"].as_array_mut().unwrap() {
            if let Some(computer) = bot["computer"].as_object_mut() {
                computer.remove("carries_image");
            }
        }
        for artifact in manifest["artifacts"].as_array_mut().unwrap() {
            if artifact["kind"] != "project" {
                continue;
            }
            let object = artifact["object"].as_str().unwrap().to_owned();
            let cipher = std::fs::read(whole.join(&object)).unwrap();
            let compressed = backup::open_small(&cipher, identity, 1 << 20).unwrap();
            let plain = zstd::stream::decode_all(compressed.as_slice()).unwrap();
            std::fs::remove_file(whole.join(&object)).unwrap();
            let bot: Uuid = artifact["bot_id"].as_str().unwrap().parse().unwrap();
            let legacy = backup::projects::Format::Tar.object(bot);
            let sealed = backup::seal(
                &whole.join(&legacy),
                identity,
                &mut plain.as_slice(),
                1 << 20,
            )
            .unwrap();
            artifact["object"] = json!(legacy);
            artifact["ciphertext_bytes"] = json!(sealed.ciphertext_bytes);
            artifact["ciphertext_sha256"] = json!(sealed.ciphertext_sha256);
            artifact["plaintext_bytes"] = json!(sealed.plaintext_bytes);
            artifact["plaintext_sha256"] = json!(sealed.plaintext_sha256);
        }
    });
    let (fresh, new_dir) = restore_fresh(&s, &whole, "restored-v2").await;
    let projects = else_api::backup::restore::projects_dir(&new_dir);
    for (bot, disk) in [(a, "alpha disk"), (b, "beta disk")] {
        assert_eq!(
            fresh.disk(&ComputerManager::runtime_name(bot)).unwrap(),
            disk
        );
        assert_eq!(
            std::fs::read_to_string(projects.join(bot.to_string()).join("notes.md")).unwrap(),
            format!("{disk} notes")
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_computer_archive_without_its_image_must_follow_one_that_has_it() {
    let s = setup_with(true).await;
    computers_with_projects(&s).await;
    let summary = worker(&s, common::SETTLE).run_once().await.unwrap();
    let (_whole, whole) = common::rebuilt_set(&summary.path, &key());
    rewrite_manifest(&whole, |manifest, _| {
        for bot in manifest["bots"].as_array_mut().unwrap() {
            if let Some(computer) = bot["computer"].as_object_mut() {
                computer.insert("carries_image".into(), json!(false));
            }
        }
    });
    let Err(err) = backup::restore::open_set(&whole, &key()) else {
        panic!("a set whose archives all lack their image opened");
    };
    assert!(err.to_string().contains("without its image"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn computers_from_a_tag_keep_their_own_image() {
    let s = setup_image(true, "python:3.12-slim").await;
    let (a, b) = computers_with_projects(&s).await;
    let summary = worker(&s, common::SETTLE).run_once().await.unwrap();
    let carried: Vec<bool> = summary
        .manifest
        .bots
        .iter()
        .filter_map(|e| e.computer.as_ref().map(|c| c.carries_image))
        .collect();
    assert_eq!(
        carried,
        [true, true],
        "a tag can name different images, so each archive keeps its own"
    );
    let (fresh, _) = restore_fresh(&s, &summary.path, "restored-tag").await;
    for (bot, disk) in [(a, "alpha disk"), (b, "beta disk")] {
        assert_eq!(
            fresh.disk(&ComputerManager::runtime_name(bot)).unwrap(),
            disk
        );
    }
}
