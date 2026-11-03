//! POC-017: bot computer lifecycle against PostgreSQL with a recording fake
//! runtime (the live Microsandbox check is in `computers_live.rs`).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use common::TestApp;
use else_api::computers::{
    Acquire, ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, IdleAction,
    RuntimeError, RuntimeState, Spec,
};
use uuid::Uuid;

#[derive(Default)]
struct Fake {
    computers: Mutex<HashMap<String, (RuntimeState, HashMap<String, String>)>>,
    calls: Mutex<Vec<String>>,
    /// Stops fail and leave the computer running.
    fail_stop: std::sync::atomic::AtomicBool,
    /// Listing fails (the runtime cannot be inspected).
    fail_list: std::sync::atomic::AtomicBool,
    /// Creation takes this long.
    create_delay: Mutex<Duration>,
    /// An image upgrade's unpack step fails in the new computer.
    fail_unpack: std::sync::atomic::AtomicBool,
}

impl Fake {
    fn log(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn set(&self, name: &str, state: RuntimeState) -> Result<(), RuntimeError> {
        let mut computers = self.computers.lock().unwrap();
        let entry = computers
            .get_mut(name)
            .ok_or_else(|| RuntimeError::Failed {
                operation: "control".into(),
                message: format!("no sandbox {name}"),
            })?;
        entry.0 = state;
        Ok(())
    }
}

#[async_trait]
impl ComputerRuntime for Fake {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        self.log("list".into());
        if self.fail_list.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RuntimeError::Unavailable("listing failed".into()));
        }
        Ok(self
            .computers
            .lock()
            .unwrap()
            .iter()
            .map(|(n, (s, _))| (n.clone(), *s))
            .collect())
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.log(format!(
            "create {} {}cpu {}MiB",
            spec.name, spec.cpus, spec.memory_mib
        ));
        let delay = *self.create_delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let mut computers = self.computers.lock().unwrap();
        if computers.contains_key(&spec.name) {
            return Err(RuntimeError::Failed {
                operation: "create".into(),
                message: "exists".into(),
            });
        }
        computers.insert(spec.name.clone(), (RuntimeState::Running, HashMap::new()));
        Ok(())
    }
    async fn copy_out(
        &self,
        name: &str,
        guest: &str,
        host: &std::path::Path,
    ) -> Result<(), RuntimeError> {
        let computers = self.computers.lock().unwrap();
        let content = computers
            .get(name)
            .and_then(|(_, f)| f.get(guest))
            .cloned()
            .unwrap_or_default();
        std::fs::write(host, content).map_err(|e| RuntimeError::Unavailable(e.to_string()))
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.log(format!("start {name}"));
        self.set(name, RuntimeState::Running)
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        self.log(format!("stop {name}"));
        if self.fail_stop.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RuntimeError::Failed {
                operation: "stop".into(),
                message: "timed out".into(),
            });
        }
        self.set(name, RuntimeState::Stopped)
    }
    async fn pause(&self, name: &str) -> Result<(), RuntimeError> {
        self.log(format!("pause {name}"));
        self.set(name, RuntimeState::Paused)
    }
    async fn resume(&self, name: &str) -> Result<(), RuntimeError> {
        self.log(format!("resume {name}"));
        self.set(name, RuntimeState::Running)
    }
    async fn remove(&self, name: &str) -> Result<(), RuntimeError> {
        self.log(format!("remove {name}"));
        let mut computers = self.computers.lock().unwrap();
        match computers.get(name) {
            Some((RuntimeState::Stopped, _)) => {
                computers.remove(name);
                Ok(())
            }
            _ => Err(RuntimeError::Failed {
                operation: "remove".into(),
                message: "not stopped".into(),
            }),
        }
    }
    async fn exec(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let mut computers = self.computers.lock().unwrap();
        let (state, files) = computers.get_mut(name).expect("exists");
        assert_eq!(
            *state,
            RuntimeState::Running,
            "exec only on a running computer"
        );
        let stdout = match program {
            "write" => {
                files.insert(args[0].into(), args[1].into());
                String::new()
            }
            "read" => files.get(args[0]).cloned().unwrap_or_default(),
            // An image upgrade's archive and unpack steps.
            "sh" if args[1].contains("tar --numeric-owner -cpf") => {
                files.insert("/var/tmp/else-upgrade.tar".into(), "DATA".into());
                use sha2::Digest;
                format!("4\n{}\n1000\n", hex::encode(sha2::Sha256::digest(b"DATA")))
            }
            "sh" if args[1].contains("sha256sum -c") => {
                if self.fail_unpack.load(std::sync::atomic::Ordering::SeqCst) {
                    return Ok(ExecOutput {
                        status: Some(4),
                        stdout: String::new(),
                        stderr: "the new image's elsebot user ID is not 1000".into(),
                        truncated: false,
                    });
                }
                "{\"image\":\"new\"}".into()
            }
            _ => String::new(),
        };
        Ok(ExecOutput {
            status: Some(0),
            stdout,
            stderr: String::new(),
            truncated: false,
        })
    }
}

fn config(max_vms: u64) -> ComputerConfig {
    ComputerConfig {
        max_cpus: max_vms as u32,
        max_memory_mib: max_vms * (2048 + 256),
        idle_after: Duration::ZERO,
        ..ComputerConfig::default()
    }
}

async fn manager(app: &TestApp, fake: &Arc<Fake>, config: ComputerConfig) -> ComputerManager {
    let manager = ComputerManager::new(app.db.pool.clone(), fake.clone(), config);
    manager.reconcile_startup().await.unwrap();
    manager
}

fn never() -> tokio_util::sync::CancellationToken {
    tokio_util::sync::CancellationToken::new()
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

async fn observed(app: &TestApp, bot: Uuid) -> String {
    sqlx::query_scalar("SELECT observed_state FROM bot_computers WHERE bot_id = $1")
        .bind(bot)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn computers_are_created_on_first_use_reused_and_kept_apart() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let a: Uuid = app.create_bot("A", "a").await.parse().unwrap();
    let b: Uuid = app.create_bot("B", "b").await.parse().unwrap();
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
    // A retry by the same holder keeps one lease; a second use reuses the computer.
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
        fake.calls().iter().any(|c| c.contains("1cpu 2048MiB")),
        "1 vCPU and 2 GiB"
    );
    let read = |lease| manager.exec(lease, "read", &["/root/note"], second);
    assert_eq!(read(lease_a).await.unwrap().stdout, "A-only");
    assert_eq!(read(lease_b).await.unwrap().stdout, "B-only");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_stop_prevents_implicit_wake_until_resume_and_chat_still_works() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = Arc::new(manager(&app, &fake, config(2)).await);
    let bot = app.create_bot("Stopped", "stopped").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let (_, name) = ready(&manager, bot_id, "test:x").await;
    manager.owner_stop(bot_id).await.unwrap();
    assert_eq!(observed(&app, bot_id).await, "stopped");
    assert!(fake.calls().contains(&format!("stop {name}")));
    assert_eq!(
        manager.acquire(bot_id, "test:y").await.unwrap(),
        Acquire::OwnerStopped
    );
    // Chat and memory need no computer.
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
    // Resume clears the stop; the next use starts it (its disk is kept).
    manager.owner_resume(bot_id).await.unwrap();
    assert_eq!(
        observed(&app, bot_id).await,
        "stopped",
        "resume starts nothing"
    );
    ready(&manager, bot_id, "test:z").await;
    assert!(fake.calls().contains(&format!("start {name}")));
    // Owner routes reach the same manager.
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
    let a: Uuid = app.create_bot("A", "a").await.parse().unwrap();
    let b: Uuid = app.create_bot("B", "b").await.parse().unwrap();
    let (lease, _) = ready(&manager, a, "test:a").await;
    assert!(matches!(
        manager.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    // Idle pause keeps A's memory reserved.
    manager.release(lease).await.unwrap();
    manager.tick(&never()).await.unwrap();
    assert_eq!(observed(&app, a).await, "paused");
    assert!(matches!(
        manager.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    // Resuming A needs no new capacity; stopping it releases capacity for B.
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
    let bot = app.create_bot("Idle", "idle").await;
    let bot_id: Uuid = bot.parse().unwrap();
    // A running turn holds the computer.
    let sent = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let (lease, _) = ready(&manager, bot_id, &format!("turn:{turn}")).await;
    sqlx::query(
        "UPDATE computer_leases SET expires_at = now() - interval '1 second' WHERE id = $1",
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
        sqlx::query_scalar("SELECT released_at IS NULL FROM computer_leases WHERE id = $1")
            .bind(lease)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert!(live, "the expired lease was renewed for live work");
    // When the turn ends, the expired lease is released and the idle
    // computer stops (its worker goes away).
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(serde_json::json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    app.wait_turn(&turn, &["interrupted"]).await;
    sqlx::query(
        "UPDATE computer_leases SET expires_at = now() - interval '1 second' WHERE id = $1",
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
    let bot: Uuid = app.create_bot("Race", "race").await.parse().unwrap();
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
    let kept: Uuid = app.create_bot("Kept", "kept").await.parse().unwrap();
    let lost: Uuid = app.create_bot("Lost", "lost").await.parse().unwrap();
    let paused: Uuid = app.create_bot("Paused", "paused").await.parse().unwrap();
    let (lease, kept_name) = ready(&manager, kept, "test:k").await;
    manager.release(lease).await.unwrap();
    let (_, lost_name) = ready(&manager, lost, "test:l").await;
    let (_, paused_name) = ready(&manager, paused, "test:p").await;
    // The runtime lost one computer and another was paused meanwhile; the
    // API also crashed mid-stop for the first.
    fake.computers.lock().unwrap().remove(&lost_name);
    fake.set(&paused_name, RuntimeState::Paused).unwrap();
    fake.set(&kept_name, RuntimeState::Stopped).unwrap();
    sqlx::query("UPDATE bot_computers SET observed_state = 'stopping' WHERE bot_id = $1")
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
    let bot = app.create_bot("Gone", "gone").await;
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
    let a: Uuid = app.create_bot("A", "a").await.parse().unwrap();
    let b: Uuid = app.create_bot("B", "b").await.parse().unwrap();
    ready(&manager, a, "test:a").await;
    fake.fail_stop
        .store(true, std::sync::atomic::Ordering::SeqCst);
    manager.owner_stop(a).await.unwrap();
    // The runtime still runs it: recorded as running, budget still held.
    assert_eq!(observed(&app, a).await, "running");
    assert!(matches!(
        manager.acquire(b, "test:b").await.unwrap(),
        Acquire::Queued { .. }
    ));
    // Maintenance retries the owner's stop until it is confirmed.
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
    *fake.create_delay.lock().unwrap() = Duration::from_millis(400);
    let manager = Arc::new(manager(&app, &fake, config(2)).await);
    let bot = app.create_bot("Short", "short").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let m = manager.clone();
    let acquiring = tokio::spawn(async move { m.acquire(bot_id, "test:s").await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;
    app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(acquiring.await.unwrap(), Acquire::BotInactive);
    assert_eq!(observed(&app, bot_id).await, "stopped");
    let leases: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM computer_leases WHERE bot_id = $1 AND released_at IS NULL",
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
    let bot: Uuid = app.create_bot("Lease", "lease").await.parse().unwrap();
    let (lease, _) = ready(&manager, bot, "test:l").await;
    sqlx::query(
        "UPDATE computer_leases SET expires_at = now() - interval '1 second' WHERE id = $1",
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
    // After an owner stop the old lease cannot run anything (the fake
    // panics if exec reaches a stopped computer).
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
    let bot: Uuid = app.create_bot("Wait", "wait").await.parse().unwrap();
    assert!(matches!(
        manager.acquire(bot, "test:w").await.unwrap(),
        Acquire::Queued { .. }
    ));
    fake.fail_list
        .store(false, std::sync::atomic::Ordering::SeqCst);
    manager.tick(&never()).await.unwrap();
    ready(&manager, bot, "test:w").await;
}

async fn computer(app: &TestApp, bot: Uuid) -> (String, String, String) {
    sqlx::query_as(
        "SELECT runtime_name, image, observed_state FROM bot_computers WHERE bot_id = $1",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

/// POC-025: a failed image upgrade leaves the bot on its old computer and
/// removes only the replacement it created; an interrupted one's leftover
/// replacement is removed by the next attempt; a held computer is refused;
/// a completed upgrade keeps the old computer for a rollback.
#[tokio::test(flavor = "multi_thread")]
async fn image_upgrades_fail_cleanly_and_keep_the_old_computer() {
    let app = TestApp::installed().await;
    let fake = Arc::new(Fake::default());
    let manager = manager(&app, &fake, config(2)).await;
    let bot: Uuid = app.create_bot("Old", "upgrade").await.parse().unwrap();
    let (lease, old_name) = ready(&manager, bot, "turn:a").await;

    // Held by a lease: refused, nothing changes.
    let held = manager.upgrade_image(bot, "img:new").await;
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

    // The unpack fails in the replacement.
    fake.fail_unpack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let failed = manager.upgrade_image(bot, "img:new").await;
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
        fake.computers.lock().unwrap()[&old_name].0,
        RuntimeState::Stopped
    );
    let (state, error): (String, String) =
        sqlx::query_as("SELECT state, error FROM computer_upgrades WHERE bot_id = $1")
            .bind(bot)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(state, "failed");
    assert!(error.contains("user ID"));

    // An interrupted attempt left a replacement nobody uses.
    fake.fail_unpack
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let leftover = format!("{old_name}-leftover");
    fake.computers
        .lock()
        .unwrap()
        .insert(leftover.clone(), (RuntimeState::Running, HashMap::new()));
    sqlx::query(
        "INSERT INTO computer_upgrades (id, bot_id, from_runtime_name, from_image, to_runtime_name, to_image, state)
         VALUES ($1, $2, $3, 'python:3.12-slim', $4, 'img:new', 'started')",
    )
    .bind(Uuid::new_v4())
    .bind(bot)
    .bind(&old_name)
    .bind(&leftover)
    .execute(&app.db.pool)
    .await
    .unwrap();
    let report = manager.upgrade_image(bot, "img:new").await.unwrap();
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
        fake.computers.lock().unwrap()[&old_name].0,
        RuntimeState::Stopped,
        "old kept"
    );
    assert_eq!(
        fake.computers.lock().unwrap()[&report.to_runtime_name].0,
        RuntimeState::Stopped
    );
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT state, count(*) FROM computer_upgrades WHERE bot_id = $1 GROUP BY state ORDER BY state",
    )
    .bind(bot)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(counts, vec![("completed".into(), 1), ("failed".into(), 2)]);

    // The upgraded computer is used normally, then rolled back.
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
