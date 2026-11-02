//! A thin adapter over the pinned Microsandbox runtime (`msb` v0.7.13).
//!
//! else.bot keeps its own runtime home (`MSB_HOME` under the API data
//! directory), so the owner's own Microsandbox state is never touched. The
//! adapter only creates, starts, stops, pauses, resumes, inspects, and runs
//! commands in named computers. It never replaces, prunes, or removes one:
//! those destroy saved disks and belong to an explicit future operation.
//! `msb exec` starts a stopped computer as a side effect, so callers run
//! commands only after the manager has established the computer is running.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::process::Command;

/// Observed runtime state of a named computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    Running,
    Paused,
    Stopped,
    /// Crashed or otherwise not usable.
    Failed,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("the computer runtime is unavailable: {0}")]
    Unavailable(String),
    #[error("{operation} failed: {message}")]
    Failed { operation: String, message: String },
}

/// Output of a command run in a computer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutput {
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    pub image: String,
    pub cpus: u32,
    pub memory_mib: u32,
}

#[async_trait]
pub trait ComputerRuntime: Send + Sync {
    /// Every computer in this runtime home, without changing any.
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError>;
    /// Create and boot a new computer. Fails if the name exists.
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError>;
    async fn start(&self, name: &str) -> Result<(), RuntimeError>;
    async fn stop(&self, name: &str) -> Result<(), RuntimeError>;
    async fn pause(&self, name: &str) -> Result<(), RuntimeError>;
    async fn resume(&self, name: &str) -> Result<(), RuntimeError>;
    /// Run a command in a running computer.
    async fn exec(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError>;

    async fn state(&self, name: &str) -> Result<Option<RuntimeState>, RuntimeError> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| s))
    }
}

/// `msb` with a private runtime home.
pub struct Msb {
    program: PathBuf,
    home: PathBuf,
    libkrunfw: Option<PathBuf>,
}

const CONTROL_TIMEOUT: Duration = Duration::from_secs(120);

impl Msb {
    /// Locate `msb` (explicit path, `PATH`, or `~/.microsandbox/bin`) and its
    /// firmware, with the runtime home under `data_dir`.
    pub fn detect(data_dir: &Path, explicit: Option<PathBuf>) -> Result<Self, RuntimeError> {
        let home_dir = std::env::var_os("HOME").map(PathBuf::from);
        let program = explicit
            .or_else(|| {
                std::env::var_os("PATH").and_then(|path| {
                    std::env::split_paths(&path)
                        .map(|dir| dir.join("msb"))
                        .find(|p| p.is_file())
                })
            })
            .or_else(|| {
                home_dir
                    .as_ref()
                    .map(|h| h.join(".microsandbox/bin/msb"))
                    .filter(|p| p.is_file())
            })
            .ok_or_else(|| RuntimeError::Unavailable("msb was not found".into()))?;
        let program = std::fs::canonicalize(&program).unwrap_or(program);
        // The firmware ships next to the binary (`../lib/libkrunfw.so.*`).
        let libkrunfw = program.parent().and_then(|bin| {
            let lib = bin.parent()?.join("lib");
            let mut found: Vec<PathBuf> = std::fs::read_dir(lib)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("libkrunfw.so."))
                })
                .collect();
            found.sort();
            found.pop()
        });
        let home = data_dir.join("microsandbox");
        std::fs::create_dir_all(&home)
            .map_err(|e| RuntimeError::Unavailable(format!("runtime home: {e}")))?;
        Ok(Self {
            program,
            home,
            libkrunfw,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .env("MSB_HOME", &self.home)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(lib) = &self.libkrunfw {
            command.env("MSB_LIBKRUNFW_PATH", lib);
        }
        command
    }

    async fn run(
        &self,
        operation: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let mut command = self.command();
        command.args(args);
        let output = tokio::time::timeout(timeout, command.output())
            .await
            .map_err(|_| RuntimeError::Failed {
                operation: operation.into(),
                message: format!("timed out after {} s", timeout.as_secs()),
            })?
            .map_err(|e| RuntimeError::Unavailable(e.to_string()))?;
        Ok(ExecOutput {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    async fn control(&self, operation: &str, args: &[&str]) -> Result<(), RuntimeError> {
        let out = self.run(operation, args, CONTROL_TIMEOUT).await?;
        if out.status == Some(0) {
            Ok(())
        } else {
            Err(RuntimeError::Failed {
                operation: operation.into(),
                message: last_line(&out.stderr, &out.stdout),
            })
        }
    }
}

fn last_line(stderr: &str, stdout: &str) -> String {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let line = text.trim().lines().last().unwrap_or("unknown error").trim();
    line.chars().take(500).collect()
}

#[derive(Deserialize)]
struct Listed {
    name: String,
    status: String,
}

pub fn parse_state(status: &str) -> RuntimeState {
    match status {
        "Running" => RuntimeState::Running,
        "Paused" => RuntimeState::Paused,
        "Stopped" => RuntimeState::Stopped,
        _ => RuntimeState::Failed,
    }
}

#[async_trait]
impl ComputerRuntime for Msb {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        let out = self
            .run("list", &["list", "--format", "json"], CONTROL_TIMEOUT)
            .await?;
        if out.status != Some(0) {
            return Err(RuntimeError::Failed {
                operation: "list".into(),
                message: last_line(&out.stderr, &out.stdout),
            });
        }
        let listed: Vec<Listed> =
            serde_json::from_str(out.stdout.trim()).map_err(|e| RuntimeError::Failed {
                operation: "list".into(),
                message: format!("unreadable output: {e}"),
            })?;
        Ok(listed
            .into_iter()
            .map(|l| (l.name, parse_state(&l.status)))
            .collect())
    }

    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        let cpus = spec.cpus.to_string();
        let memory = format!("{}M", spec.memory_mib);
        self.control(
            "create",
            &[
                "create",
                "--name",
                &spec.name,
                "--cpus",
                &cpus,
                "--memory",
                &memory,
                "--quiet",
                &spec.image,
            ],
        )
        .await
    }

    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.control("start", &["start", name]).await
    }

    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        self.control("stop", &["stop", name]).await
    }

    async fn pause(&self, name: &str) -> Result<(), RuntimeError> {
        self.control("pause", &["pause", name]).await
    }

    async fn resume(&self, name: &str) -> Result<(), RuntimeError> {
        self.control("resume", &["resume", name]).await
    }

    async fn exec(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let mut all = vec!["exec", name, "--", program];
        all.extend_from_slice(args);
        self.run("exec", &all, timeout).await
    }
}
