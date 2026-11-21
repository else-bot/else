use crate::common;

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

struct Computer {
    state: RuntimeState,
    files: HashMap<String, String>,
}

#[derive(Default)]
struct Fake {
    computers: Mutex<HashMap<String, Computer>>,
    calls: Mutex<Vec<String>>,
    fail_stop: std::sync::atomic::AtomicBool,
    fail_list: std::sync::atomic::AtomicBool,
    create_delay: Mutex<Duration>,
    /// While set, each create waits for a permit.
    create_hold: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// While set, each listing waits for a permit after taking its snapshot.
    list_hold: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    list_entered: tokio::sync::Notify,
    fail_unpack: std::sync::atomic::AtomicBool,
    fail_create: std::sync::atomic::AtomicBool,
    fail_close: std::sync::atomic::AtomicBool,
    fail_configure: std::sync::atomic::AtomicBool,
    fail_desktop: std::sync::atomic::AtomicBool,
    hold_desktop: std::sync::atomic::AtomicBool,
    desktop_entered: tokio::sync::Notify,
    desktop_resume: tokio::sync::Notify,
    specs: Mutex<Vec<Spec>>,
    packages: Mutex<Vec<String>>,
    reinstalled: Mutex<Vec<String>>,
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
        entry.state = state;
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
        let listed = self
            .computers
            .lock()
            .unwrap()
            .iter()
            .map(|(name, computer)| (name.clone(), computer.state))
            .collect();
        let hold = self.list_hold.lock().unwrap().clone();
        if let Some(hold) = hold {
            self.list_entered.notify_one();
            let _ = hold.acquire().await;
        }
        Ok(listed)
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.log(format!(
            "create {} {}cpu {}MiB",
            spec.name, spec.cpus, spec.memory_mib
        ));
        self.specs.lock().unwrap().push(spec.clone());
        if self.fail_create.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(RuntimeError::Failed {
                operation: "create".into(),
                message: "image pull failed".into(),
            });
        }
        let delay = *self.create_delay.lock().unwrap();
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let hold = self.create_hold.lock().unwrap().clone();
        if let Some(hold) = hold {
            let _ = hold.acquire().await;
        }
        let mut computers = self.computers.lock().unwrap();
        if computers.contains_key(&spec.name) {
            return Err(RuntimeError::Failed {
                operation: "create".into(),
                message: "exists".into(),
            });
        }
        computers.insert(
            spec.name.clone(),
            Computer {
                state: RuntimeState::Running,
                files: HashMap::new(),
            },
        );
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
            .and_then(|computer| computer.files.get(guest))
            .cloned()
            .unwrap_or_default();
        std::fs::write(host, content).map_err(|e| RuntimeError::Unavailable(e.to_string()))
    }
    async fn exec_to_file(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        stdout: &std::path::Path,
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        assert_eq!(
            self.computers.lock().unwrap()[name].state,
            RuntimeState::Running
        );
        assert!(program == "/bin/bash" && args[1].contains("tar --numeric-owner"));
        std::fs::write(stdout, "DATA").unwrap();
        use sha2::Digest;
        Ok(ExecOutput {
            status: Some(0),
            stdout: String::new(),
            stderr: format!(
                "{}\nwarn: runtime notice",
                hex::encode(sha2::Sha256::digest(b"DATA"))
            ),
            truncated: false,
        })
    }
    async fn exec_from_file(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        stdin: &std::path::Path,
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        assert_eq!(
            self.computers.lock().unwrap()[name].state,
            RuntimeState::Running
        );
        assert!(program == "/bin/bash" && args[1].contains("tar --numeric-owner"));
        assert_eq!(std::fs::read(stdin).unwrap(), b"DATA");
        if self.fail_unpack.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(ExecOutput {
                status: Some(4),
                stdout: String::new(),
                stderr: "the new image's elsebot user ID is not 1000".into(),
                truncated: false,
            });
        }
        let packages = &args[6..];
        *self.reinstalled.lock().unwrap() = packages.iter().map(|p| (*p).to_owned()).collect();
        let mut out = String::new();
        for package in packages.iter().filter(|p| p.starts_with("gone-")) {
            out += &format!("unrestored {package}\n");
        }
        Ok(ExecOutput {
            status: Some(0),
            stdout: out + "{\"image\":\"new\"}",
            stderr: String::new(),
            truncated: false,
        })
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
    async fn grow(&self, name: &str, cpus: u32, memory_mib: u32) -> Result<(), RuntimeError> {
        self.log(format!("grow {name} {cpus}cpu {memory_mib}MiB"));
        Ok(())
    }
    async fn configure(
        &self,
        name: &str,
        cpus: u32,
        memory_mib: u32,
        max_cpus: u32,
        max_memory_mib: u32,
    ) -> Result<(), RuntimeError> {
        self.log(format!(
            "configure {name} {cpus}cpu {memory_mib}MiB up to {max_cpus}cpu {max_memory_mib}MiB"
        ));
        if self
            .fail_configure
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(RuntimeError::Failed {
                operation: "modify".into(),
                message: "busy".into(),
            });
        }
        Ok(())
    }
    async fn usage(
        &self,
    ) -> Result<HashMap<String, else_api::computers::runtime::Usage>, RuntimeError> {
        Ok(self
            .computers
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, computer)| computer.state == RuntimeState::Running)
            .map(|(name, _)| {
                let usage = else_api::computers::runtime::Usage {
                    cpu_percent: 150.0,
                    cpus: Some(2),
                    memory_bytes: Some(1 << 30),
                    memory_limit_bytes: 2 << 30,
                    disk: Some((3 << 20, 8 << 30)),
                };
                (name.clone(), usage)
            })
            .collect())
    }
    async fn disks(&self) -> Result<HashMap<String, u64>, RuntimeError> {
        Ok(self
            .computers
            .lock()
            .unwrap()
            .keys()
            .map(|name| (name.clone(), 1 << 20))
            .collect())
    }
    async fn remove(&self, name: &str) -> Result<(), RuntimeError> {
        self.log(format!("remove {name}"));
        let mut computers = self.computers.lock().unwrap();
        match computers.get(name) {
            Some(Computer {
                state: RuntimeState::Stopped,
                ..
            }) => {
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
        let close = program == "/bin/sh" && args[1].contains("kill -INT");
        if close {
            self.log(format!("close-browser {name}"));
        } else if program == else_api::computers::GUEST_CHECKPOINT {
            self.log(format!("checkpoint {name}"));
        } else if program == else_api::computers::GUEST_INIT {
            self.log(format!("init {name} {}", args.join(" ")));
        } else if program == "/bin/sh" && args[1] == else_api::computers::GUEST_DESKTOP {
            self.log(format!("desktop {name}"));
        }
        let desktop = program == "/bin/sh" && args[1] == else_api::computers::GUEST_DESKTOP;
        let fail_desktop = desktop && self.fail_desktop.load(std::sync::atomic::Ordering::SeqCst);
        if desktop && self.hold_desktop.load(std::sync::atomic::Ordering::SeqCst) {
            self.desktop_entered.notify_one();
            self.desktop_resume.notified().await;
        }
        let mut computers = self.computers.lock().unwrap();
        let Computer { state, files } = computers.get_mut(name).expect("exists");
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
            "/bin/sh" if args[1].contains("apt-mark showmanual") => {
                let mut report = "1000\n".to_owned();
                for package in self.packages.lock().unwrap().iter() {
                    report += &format!("{package}\n");
                }
                report
            }
            "/bin/sh" if fail_desktop => {
                return Ok(ExecOutput {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "Xvfb did not start".into(),
                    truncated: false,
                });
            }
            "/bin/sh" if close && self.fail_close.load(std::sync::atomic::Ordering::SeqCst) => {
                return Ok(ExecOutput {
                    status: Some(1),
                    stdout: String::new(),
                    stderr: "Chromium did not exit within 10 seconds of SIGINT".into(),
                    truncated: false,
                });
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
    sqlx::query_scalar("SELECT observed_state FROM bot_computers WHERE bot_id = ?1")
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

async fn computer(app: &TestApp, bot: Uuid) -> (String, String, String) {
    sqlx::query_as(
        "SELECT runtime_name, image, observed_state FROM bot_computers WHERE bot_id = ?1",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

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
    manager.tick(&never()).await.unwrap();

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

async fn wait_for_idle_stop(app: &TestApp, manager: &ComputerManager, bot: Uuid) {
    let cancel = never();
    common::until(
        &format!("idle computer {bot} to stop"),
        common::poll(|| async {
            manager.tick(&cancel).await.unwrap();
            (observed(app, bot).await == "stopped").then_some(())
        }),
    )
    .await;
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
    manager.tick(&never()).await.unwrap();
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
