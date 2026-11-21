//! Set directories, retention, and the chunk store.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_and_retention_touch_only_owned_real_set_directories() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join("backups");
    std::fs::create_dir(&backups).unwrap();
    std::fs::create_dir(backups.join("archive")).unwrap();
    std::fs::write(backups.join("archive/photo.jpg"), "keep").unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("complete.json"), "{}").unwrap();
    std::fs::write(outside.join(backup::MARKER), "").unwrap();
    std::os::unix::fs::symlink(&outside, backups.join("20000101T000000000Z-00000000")).unwrap();
    let mut cfg = config(dir.path());
    cfg.keep = 1;
    let worker = worker(&app, key(1), cfg);
    worker.run_once().await.unwrap();
    let newest = worker.run_once().await.unwrap();
    assert!(
        newest.path.join("complete.json").exists(),
        "the set just made is kept"
    );
    assert_eq!(
        backup::completed_sets(&backups).unwrap(),
        std::slice::from_ref(&newest.set_id)
    );
    assert_eq!(
        std::fs::read_to_string(backups.join("archive/photo.jpg")).unwrap(),
        "keep"
    );
    assert!(
        outside.join("complete.json").exists(),
        "nothing deleted through a symlink"
    );
    assert_eq!(worker.recover().await.unwrap(), 0);
    assert!(backups.join("archive").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_chunk_store_sweeps_only_chunks_no_readable_kept_set_uses() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(dir.path());
    cfg.keep = 2;
    let worker = worker(&app, key(1), cfg);
    let chunk_files = || -> Vec<String> {
        let chunks = dir.path().join("backups/chunks");
        let mut names: Vec<String> = std::fs::read_dir(&chunks)
            .unwrap()
            .flat_map(|k| std::fs::read_dir(k.unwrap().path()).unwrap())
            .map(|c| c.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let named = |set: &Path| -> std::collections::BTreeSet<String> {
        let (_, index) = backup::store::open_index(set, &key(1)).unwrap();
        index.chunk_ids().map(str::to_owned).collect()
    };

    let first = worker.run_once().await.unwrap();
    let after_first = chunk_files();
    let second = worker.run_once().await.unwrap();

    let key_dir = std::fs::read_dir(dir.path().join("backups/chunks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let orphan = "ab".repeat(32);
    std::fs::write(key_dir.join(&orphan), "from an interrupted capture").unwrap();
    std::fs::write(key_dir.join(format!(".{orphan}.x1y2.tmp")), "half written").unwrap();
    let index = second.path.join(backup::chunks::INDEX);
    let sealed = std::fs::read(&index).unwrap();
    let mut flipped = sealed.clone();
    let at = flipped.len() / 2;
    flipped[at] ^= 1;
    std::fs::write(&index, &flipped).unwrap();
    let third = worker.run_once().await.unwrap();
    assert!(!first.path.exists(), "keep 2 removes the oldest set");
    assert!(
        chunk_files().contains(&orphan),
        "nothing is swept while a kept set's index cannot be read"
    );
    assert!(
        after_first.iter().all(|c| chunk_files().contains(c)),
        "the unreadable set's chunks are all kept"
    );

    std::fs::write(&index, &sealed).unwrap();
    let fourth = worker.run_once().await.unwrap();
    assert!(!second.path.exists());
    let kept: std::collections::BTreeSet<String> = named(&third.path)
        .union(&named(&fourth.path))
        .cloned()
        .collect();
    assert_eq!(
        chunk_files(),
        kept.into_iter().collect::<Vec<_>>(),
        "only chunks the kept sets name remain, without temporary files"
    );
    std::fs::write(
        key_dir.join(&orphan),
        "from a capture that ran out of space",
    )
    .unwrap();
    worker.recover().await.unwrap();
    assert!(
        !key_dir.join(&orphan).exists(),
        "recovery sweeps before the next capture needs the space"
    );
    for set in [&third, &fourth] {
        let target = Target::new(&app);
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        restore_into(&app, &set.path, &key(1), &target, &staging)
            .await
            .unwrap();
        assert!(target.exists().await);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn whole_sets_move_into_the_chunk_store_and_a_damaged_one_stays_whole() {
    let _serial = SERIAL.lock().await;
    let app = populated().await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(dir.path());
    let cancel = tokio_util::sync::CancellationToken::new();
    let coordinator = else_api::backup::capture::Coordinator::default();
    let mut whole = Vec::new();
    for _ in 0..2 {
        let set = else_api::backup::capture::create_set(
            &app.db.pool,
            &key(1),
            &cfg,
            &backup::new_set_id(),
            &cancel,
            &coordinator,
        )
        .await
        .unwrap();
        assert!(set.path.join(backup::DATABASE).exists(), "captured whole");
        whole.push(set);
    }
    let damaged = whole[1].path.join(backup::DATABASE);
    let len = std::fs::metadata(&damaged).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&damaged)
        .unwrap()
        .set_len(len - 100)
        .unwrap();

    let names = |set: &Path| -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(set)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    let worker = worker(&app, key(1), cfg);
    let newest = worker.run_once().await.unwrap();
    for set in [&whole[0].path, &newest.path] {
        assert_eq!(
            names(set),
            [".else-backup", "complete.json", "index.json.age"],
            "{} keeps only its index",
            set.display()
        );
    }
    assert!(
        names(&whole[1].path).contains(&backup::MANIFEST.to_owned()),
        "a set whose artifacts do not match its manifest is not converted"
    );

    std::fs::write(
        whole[0].path.join(backup::DATABASE),
        "left by an interrupted move",
    )
    .unwrap();
    worker.run_once().await.unwrap();
    assert_eq!(
        names(&whole[0].path),
        [".else-backup", "complete.json", "index.json.age"]
    );
    let target = Target::new(&app);
    let staging = dir.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    restore_into(&app, &whole[0].path, &key(1), &target, &staging)
        .await
        .unwrap();
    assert!(target.exists().await);
}
