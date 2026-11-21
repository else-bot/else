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
mod desktop;
mod lifecycle;
mod owner_upgrades;
mod upgrades;

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

fn no_cancel() -> tokio_util::sync::CancellationToken {
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

async fn computer(app: &TestApp, bot: Uuid) -> (String, String, String) {
    sqlx::query_as(
        "SELECT runtime_name, image, observed_state FROM bot_computers WHERE bot_id = ?1",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

async fn wait_for_idle_stop(app: &TestApp, manager: &ComputerManager, bot: Uuid) {
    let cancel = no_cancel();
    common::until(
        &format!("idle computer {bot} to stop"),
        common::poll(|| async {
            manager.tick(&cancel).await.unwrap();
            (observed(app, bot).await == "stopped").then_some(())
        }),
    )
    .await;
}
