//! POC-017 against the real Microsandbox runtime (`msb` v0.7.13, KVM). It
//! boots small VMs, so it runs only with `ELSE_LIVE_VM=1`:
//!
//! ELSE_LIVE_VM=1 cargo test -p else-api --test computers_live -- --nocapture
//!
//! It uses a private runtime home in a temporary directory and removes its
//! own computers at the end; the owner's ~/.microsandbox is not touched.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::TestApp;
use else_api::computers::runtime::Msb;
use else_api::computers::{Acquire, ComputerConfig, ComputerManager, IdleAction, RuntimeState};
use uuid::Uuid;

fn live() -> bool {
    std::env::var("ELSE_LIVE_VM").as_deref() == Ok("1")
}

async fn ready(manager: &ComputerManager, bot: Uuid, holder: &str) -> (Uuid, String) {
    match manager.acquire(bot, holder).await.unwrap() {
        Acquire::Ready {
            lease_id,
            runtime_name,
            ..
        } => (lease_id, runtime_name),
        other => panic!("not ready: {other:?}"),
    }
}

async fn sh(manager: &ComputerManager, name: &str, script: &str) -> String {
    let out = manager
        .runtime()
        .exec(name, "sh", &["-c", script], Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    out.stdout.trim().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_named_computers_keep_their_files_across_stop_pause_and_restart() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_VM=1 to boot real Microsandbox VMs");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let app = TestApp::installed().await;
    let config = ComputerConfig {
        idle_after: Duration::ZERO,
        max_cpus: 2,
        max_memory_mib: 2 * (2048 + 256),
        ..ComputerConfig::default()
    };
    let msb = || Arc::new(Msb::detect(dir.path(), None).expect("msb installed"));
    let manager = ComputerManager::new(app.db.pool.clone(), msb(), config.clone());
    let a: Uuid = app.create_bot("Alpha", "alpha").await.parse().unwrap();
    let b: Uuid = app.create_bot("Beta", "beta").await.parse().unwrap();

    let started = std::time::Instant::now();
    let (lease_a, name_a) = ready(&manager, a, "test:a").await;
    eprintln!(
        "first create (with image pull) took {:?}",
        started.elapsed()
    );
    let (lease_b, name_b) = ready(&manager, b, "test:b").await;
    sh(&manager, &name_a, "echo ALPHA-FILE > /root/marker").await;
    sh(&manager, &name_b, "echo BETA-FILE > /root/marker").await;
    assert_eq!(
        sh(&manager, &name_a, "cat /root/marker").await,
        "ALPHA-FILE"
    );
    assert_eq!(sh(&manager, &name_b, "cat /root/marker").await, "BETA-FILE");
    assert_eq!(sh(&manager, &name_a, "nproc").await, "1");

    // Idle: both stop once released; their workers are gone.
    manager.release(lease_a).await.unwrap();
    manager.release(lease_b).await.unwrap();
    manager.tick().await.unwrap();
    let states = manager.runtime().list().await.unwrap();
    for name in [&name_a, &name_b] {
        assert!(
            states.contains(&(name.clone(), RuntimeState::Stopped)),
            "{states:?}"
        );
    }

    // An API restart only inspects: nothing wakes.
    let restarted = ComputerManager::new(app.db.pool.clone(), msb(), config.clone());
    restarted.reconcile_startup().await.unwrap();
    let states = restarted.runtime().list().await.unwrap();
    assert!(
        states.iter().all(|(_, s)| *s == RuntimeState::Stopped),
        "{states:?}"
    );

    // Reuse after restart: the same computer with its file.
    let (lease_a, _) = ready(&restarted, a, "test:a2").await;
    assert_eq!(
        sh(&restarted, &name_a, "cat /root/marker").await,
        "ALPHA-FILE"
    );
    restarted.release(lease_a).await.unwrap();

    // Owner stop: no implicit wake.
    restarted.owner_stop(a).await.unwrap();
    assert_eq!(
        restarted.acquire(a, "test:a3").await.unwrap(),
        Acquire::OwnerStopped
    );
    assert_eq!(
        restarted.runtime().state(&name_a).await.unwrap(),
        Some(RuntimeState::Stopped)
    );

    // Idle pause keeps the process; resume continues with the file.
    let pausing = ComputerManager::new(
        app.db.pool.clone(),
        msb(),
        ComputerConfig {
            idle_action: IdleAction::Pause,
            ..config
        },
    );
    let (lease_b, _) = ready(&pausing, b, "test:b2").await;
    sh(
        &pausing,
        &name_b,
        "sleep 600 >/dev/null 2>&1 & echo $! > /root/pid",
    )
    .await;
    let pid = sh(&pausing, &name_b, "cat /root/pid").await;
    pausing.release(lease_b).await.unwrap();
    pausing.tick().await.unwrap();
    assert_eq!(
        pausing.runtime().state(&name_b).await.unwrap(),
        Some(RuntimeState::Paused)
    );
    let (lease_b, _) = ready(&pausing, b, "test:b3").await;
    assert_eq!(
        sh(&pausing, &name_b, &format!("kill -0 {pid} && echo ALIVE")).await,
        "ALIVE"
    );
    assert_eq!(sh(&pausing, &name_b, "cat /root/marker").await, "BETA-FILE");
    pausing.release(lease_b).await.unwrap();

    // Cleanup of this test's own runtime home only.
    for name in [&name_a, &name_b] {
        let _ = std::process::Command::new(
            std::env::var("HOME")
                .map(|h| format!("{h}/.microsandbox/bin/msb"))
                .unwrap(),
        )
        .env("MSB_HOME", dir.path().join("microsandbox"))
        .args(["stop", name])
        .output();
        let _ = std::process::Command::new(
            std::env::var("HOME")
                .map(|h| format!("{h}/.microsandbox/bin/msb"))
                .unwrap(),
        )
        .env("MSB_HOME", dir.path().join("microsandbox"))
        .args(["rm", name])
        .output();
    }
}
