//! Shared MCP packages in a backup set.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn shared_mcp_packages_survive_backup_restore_and_removal() {
    let s = setup().await;
    let library = s.dir.path().join("mcp");
    let package = library.join("t3-code");
    std::fs::create_dir_all(&package).unwrap();
    let files = [
        (
            "manifest.json",
            r#"{"name":"T3 Code","installed_by":"Maple"}"#,
        ),
        ("SKILL.md", "Use T3 Code to work with code and threads."),
        ("README.md", "Saved setup and API documentation."),
        (
            "config.json",
            r#"{"url":"https://example.test/mcp","headers":{"Authorization":"Bearer shared-secret"}}"#,
        ),
    ];
    for (name, contents) in files {
        std::fs::write(package.join(name), contents).unwrap();
    }
    std::fs::create_dir(library.join(".pending-install")).unwrap();
    std::fs::write(library.join(".pending-install/README.md"), "unfinished").unwrap();
    let summary = worker(&s, common::SETTLE).run_once().await.unwrap();
    assert_eq!(summary.manifest.coverage.mcp, Covered::Included);
    assert!(summary.manifest.coverage.whole_assistant);
    let artifact = summary
        .manifest
        .artifacts
        .iter()
        .find(|a| a.kind == ArtifactKind::Mcp)
        .unwrap();
    assert!(
        artifact.bot_id.is_none(),
        "the library is shared by all bots"
    );
    for ciphertext in stored_chunks(&summary.path) {
        assert!(ciphertext.starts_with(b"age-encryption.org/v1"));
        assert!(
            !ciphertext
                .windows(b"shared-secret".len())
                .any(|s| s == b"shared-secret")
        );
    }
    std::fs::rename(&package, library.join(".removed-t3-code")).unwrap();
    let after_removal = worker(&s, common::SETTLE).run_once().await.unwrap();
    for (set, populated) in [(&summary, true), (&after_removal, false)] {
        let target = Target::new(&s.app);
        let new_dir = s.dir.path().join(format!("restored-{}", target.name));
        std::fs::create_dir(&new_dir).unwrap();
        restore(RestoreRequest {
            set_dir: &set.path,
            key: &key(),
            server: &target.params,
            target_database: &target.name,
            staging_parent: &s.dir.path().join("staging"),
            max_bytes: 1 << 30,
            computers: Some(ComputerTarget {
                runtime: None,
                data_dir: &new_dir,
            }),
        })
        .await
        .unwrap();
        let restored = new_dir.join("mcp");
        assert_eq!(
            std::fs::read_dir(&restored).unwrap().count(),
            usize::from(populated)
        );
        if populated {
            for (name, contents) in files {
                assert_eq!(
                    std::fs::read_to_string(restored.join("t3-code").join(name)).unwrap(),
                    contents
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_incomplete_shared_mcp_package_does_not_produce_a_complete_backup() {
    let s = setup().await;
    let library = s.dir.path().join("mcp/t3-code");
    std::fs::create_dir_all(&library).unwrap();
    std::fs::write(library.join("SKILL.md"), "only one file").unwrap();
    let result = worker(&s, common::SETTLE).run_once().await;
    assert!(matches!(result, Err(BackupError::Invalid(_))), "{result:?}");
    assert!(
        backup::completed_sets(&s.dir.path().join("backups"))
            .unwrap()
            .is_empty()
    );
    let main = s.app.main_bot().await;
    chat(&s.app, &main, "the maintenance gate was released").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn backups_from_before_shared_mcp_support_still_restore() {
    let s = setup().await;
    let summary = worker(&s, common::SETTLE)
        .with_coordinator(Coordinator::default())
        .run_once()
        .await
        .unwrap();
    let (_whole, whole) = common::rebuilt_set(&summary.path, &key());
    let (mut completion, manifest, identity) = backup::restore::open_set(&whole, &key()).unwrap();
    let mut old_manifest = serde_json::to_value(manifest).unwrap();
    old_manifest["coverage"]
        .as_object_mut()
        .unwrap()
        .remove("mcp");
    let bytes = serde_json::to_vec(&old_manifest).unwrap();
    let manifest_path = whole.join(backup::MANIFEST);
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
        whole.join(backup::COMPLETE),
        serde_json::to_vec(&completion).unwrap(),
    )
    .unwrap();
    let target = Target::new(&s.app);
    let report = restore(RestoreRequest {
        set_dir: &whole,
        key: &key(),
        server: &target.params,
        target_database: &target.name,
        staging_parent: &s.dir.path().join("staging"),
        max_bytes: 1 << 30,
        computers: None,
    })
    .await
    .unwrap();
    assert_eq!(report.manifest.coverage.mcp, Covered::Omitted);
}
