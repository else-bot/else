//! A thin adapter over the pinned Microsandbox runtime (`msb` v0.7.13).
//!
//! else.bot keeps its own runtime home (`MSB_HOME` under the API data
//! directory), so the owner's own Microsandbox state is never touched. The
//! adapter only creates, starts, stops, pauses, resumes, inspects, and runs
//! commands in named computers, and exports and restores their disks for
//! backups. It never replaces, prunes, or removes a computer: those destroy
//! saved disks and belong to an explicit future operation.
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
    /// Output beyond the capture limit was discarded.
    pub truncated: bool,
}

/// Bytes kept from each of stdout and stderr; the rest is read and dropped
/// so a noisy guest cannot exhaust host memory.
pub const OUTPUT_LIMIT: usize = 1024 * 1024;

/// The runtime version whose behavior was verified.
pub const VERIFIED_VERSION: &str = "msb 0.7.13";

/// An OCI image reference safe to pass as one positional argument.
pub fn valid_image(image: &str) -> bool {
    !image.is_empty()
        && image.len() <= 512
        && !image.starts_with('-')
        && image
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-/:@+".contains(c))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub name: String,
    pub image: String,
    pub cpus: u32,
    pub memory_mib: u32,
    /// Microsandbox network profile (`public`: internet only, no host,
    /// private, or link-local networks).
    pub network: String,
    /// Published ports as `127.0.0.1:HOST:GUEST` (host loopback only).
    pub ports: Vec<(u16, u16)>,
    /// Host files copied into the guest at creation: (host path, guest path).
    pub files: Vec<(PathBuf, String)>,
    /// Host directories mounted live into the guest.
    pub mounts: Vec<Mount>,
}

/// A host directory mounted live into a computer, its files presented there
/// as owned by `uid` (and the group of the same ID).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub host: PathBuf,
    pub guest: String,
    pub uid: u32,
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

    /// Run a command with `input` on its standard input (task secrets are
    /// sent this way, never in arguments).
    async fn exec_input(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        input: &[u8],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let _ = (name, program, args, input, timeout);
        Err(RuntimeError::Unavailable(
            "this runtime cannot pass input to a command".into(),
        ))
    }

    /// Copy a guest file to a host path.
    async fn copy_out(&self, name: &str, guest: &str, host: &Path) -> Result<(), RuntimeError>;

    /// Export a stopped computer's disk, with its image, to a standalone
    /// archive at `archive` (POC-022). The computer is not started.
    async fn export_disk(&self, name: &str, archive: &Path) -> Result<(), RuntimeError> {
        let _ = (name, archive);
        Err(RuntimeError::Unavailable(
            "disk export is not supported".into(),
        ))
    }

    /// Create computer `spec.name` from a standalone disk archive, with the
    /// spec's resources, ports, and network policy, and leave it stopped.
    async fn restore_disk(&self, archive: &Path, spec: &Spec) -> Result<(), RuntimeError> {
        let _ = (archive, spec);
        Err(RuntimeError::Unavailable(
            "disk restore is not supported".into(),
        ))
    }

    /// Remove a stopped computer and its disk. Only used for a computer
    /// else.bot itself just created and never handed out (a failed image
    /// upgrade's replacement).
    async fn remove(&self, name: &str) -> Result<(), RuntimeError> {
        let _ = name;
        Err(RuntimeError::Unavailable("remove is not supported".into()))
    }

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
/// Snapshot capture, export, import, and restore of large disks.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(1800);

/// The network policy for a restored computer. `msb snapshot restore` does
/// not carry the source's policy (verified with v0.7.13: a restored sandbox
/// had none), so it is stated explicitly. `public` is the same policy
/// `create --net public` records: egress denied except DNS and public
/// destinations, ingress (published host-loopback ports) allowed.
pub fn restore_network_args(network: &str) -> Result<Vec<String>, RuntimeError> {
    let rules: &[&str] = match network {
        "public" => &[
            "--net-default",
            "deny",
            "--net-rule",
            "allow@dns",
            "--net-rule",
            "allow@public",
            "--net-rule",
            "allow:ingress@any",
        ],
        "none" => &["--no-net"],
        other => {
            return Err(RuntimeError::Failed {
                operation: "restore".into(),
                message: format!("no restore policy for network profile {other}"),
            });
        }
    };
    Ok(rules.iter().map(|r| (*r).to_owned()).collect())
}

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
        // Only the verified runtime is used; another version needs an
        // explicit, checked opt-in.
        let version = std::process::Command::new(&program)
            .arg("--version")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default();
        let allow = std::env::var("ELSE_MSB_ALLOW_UNVERIFIED").as_deref() == Ok("1");
        if version != VERIFIED_VERSION && !allow {
            return Err(RuntimeError::Unavailable(format!(
                "{} is not the verified runtime ({VERIFIED_VERSION}); set \
                 ELSE_MSB_ALLOW_UNVERIFIED=1 after checking it",
                if version.is_empty() {
                    "this msb"
                } else {
                    &version
                }
            )));
        }
        // The firmware ships next to the binary (`../lib/libkrunfw.so.*`, or
        // `libkrunfw.*.dylib` on macOS).
        let libkrunfw = program.parent().and_then(|bin| {
            let lib = bin.parent()?.join("lib");
            let mut found: Vec<PathBuf> = std::fs::read_dir(lib)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                        n.starts_with("libkrunfw.so.")
                            || (n.starts_with("libkrunfw.") && n.ends_with(".dylib"))
                    })
                })
                .collect();
            found.sort();
            found.pop()
        });
        if libkrunfw.is_none() {
            return Err(RuntimeError::Unavailable(
                "the libkrunfw firmware bundled with msb was not found".into(),
            ));
        }
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
        self.run_input(operation, args, None, timeout).await
    }

    async fn run_input(
        &self,
        operation: &str,
        args: &[&str],
        input: Option<&[u8]>,
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut command = self.command();
        command
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if input.is_some() {
            command.stdin(std::process::Stdio::piped());
        }
        let mut child = command
            .spawn()
            .map_err(|e| RuntimeError::Unavailable(e.to_string()))?;
        if let Some(input) = input {
            let mut stdin = child.stdin.take().expect("piped");
            let written = tokio::time::timeout(timeout, async {
                stdin.write_all(input).await?;
                stdin.shutdown().await
            })
            .await;
            drop(stdin);
            if !matches!(written, Ok(Ok(()))) {
                return Err(RuntimeError::Failed {
                    operation: operation.into(),
                    message: "the command's input could not be written".into(),
                });
            }
        }
        // Keep at most OUTPUT_LIMIT bytes of each stream; keep reading (and
        // dropping) the rest so the child never blocks on a full pipe.
        async fn capture(mut stream: impl tokio::io::AsyncRead + Unpin) -> (Vec<u8>, bool) {
            let mut kept = Vec::new();
            let mut truncated = false;
            let mut chunk = [0u8; 8192];
            while let Ok(n) = stream.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let room = OUTPUT_LIMIT.saturating_sub(kept.len());
                kept.extend_from_slice(&chunk[..n.min(room)]);
                truncated |= n > room;
            }
            (kept, truncated)
        }
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let work = async {
            let (out, err, status) = tokio::join!(capture(stdout), capture(stderr), child.wait());
            (out, err, status)
        };
        let ((stdout, out_cut), (stderr, err_cut), status) = tokio::time::timeout(timeout, work)
            .await
            .map_err(|_| RuntimeError::Failed {
                operation: operation.into(),
                message: format!("timed out after {} s", timeout.as_secs()),
            })?;
        let status = status.map_err(|e| RuntimeError::Unavailable(e.to_string()))?;
        Ok(ExecOutput {
            status: status.code(),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            truncated: out_cut || err_cut,
        })
    }

    async fn run_checked(&self, operation: &str, args: &[&str]) -> Result<(), RuntimeError> {
        let out = self.run(operation, args, SNAPSHOT_TIMEOUT).await?;
        if out.status == Some(0) {
            Ok(())
        } else {
            Err(RuntimeError::Failed {
                operation: operation.into(),
                message: last_line(&out.stderr, &out.stdout),
            })
        }
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
        if !valid_image(&spec.image) {
            return Err(RuntimeError::Failed {
                operation: "create".into(),
                message: "the image reference is not valid".into(),
            });
        }
        let cpus = spec.cpus.to_string();
        let memory = format!("{}M", spec.memory_mib);
        let mut args: Vec<String> = [
            "create",
            "--name",
            &spec.name,
            "--cpus",
            &cpus,
            "--memory",
            &memory,
            "--quiet",
            "--net",
            &spec.network,
        ]
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
        for (host, guest) in &spec.ports {
            args.push("--port".into());
            args.push(format!("127.0.0.1:{host}:{guest}"));
        }
        for (host, guest) in &spec.files {
            args.push("--copy-file".into());
            args.push(format!("{}:{guest}", host.display()));
        }
        for Mount { host, guest, uid } in &spec.mounts {
            // `SOURCE:DEST:OPTIONS`: a colon in the host path would shift
            // the fields.
            let host =
                host.to_str()
                    .filter(|h| !h.contains(':'))
                    .ok_or_else(|| RuntimeError::Failed {
                        operation: "create".into(),
                        message: format!(
                            "{} cannot be mounted (not UTF-8, or a ':')",
                            host.display()
                        ),
                    })?;
            args.push("--mount-dir".into());
            args.push(format!("{host}:{guest}:uid={uid},gid={uid}"));
        }
        // `--` keeps the image a positional argument, never an option.
        args.push("--".into());
        args.push(spec.image.clone());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.control("create", &args).await
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

    async fn export_disk(&self, name: &str, archive: &Path) -> Result<(), RuntimeError> {
        // A disk snapshot of the stopped computer, verified, exported with
        // its image so it boots without this runtime, then removed here.
        let snapshot = format!("backup-{}", uuid::Uuid::new_v4().simple());
        let member = format!("{name}:{snapshot}");
        self.run_checked(
            "snapshot",
            &[
                "snapshot",
                "create",
                &snapshot,
                "--sandbox",
                name,
                "--integrity",
                "--quiet",
            ],
        )
        .await?;
        let archive_arg = archive.display().to_string();
        let exported = async {
            self.run_checked("snapshot verify", &["snapshot", "verify", &member])
                .await?;
            self.run_checked(
                "snapshot export",
                &[
                    "snapshot",
                    "export",
                    &member,
                    "--with-image",
                    "--output",
                    &archive_arg,
                ],
            )
            .await
        }
        .await;
        let removed = self
            .run_checked(
                "snapshot remove",
                &["snapshot", "remove", "--quiet", &member],
            )
            .await;
        exported?;
        removed
    }

    async fn restore_disk(&self, archive: &Path, spec: &Spec) -> Result<(), RuntimeError> {
        let group = format!("{}-restored", spec.name);
        let archive_arg = archive.display().to_string();
        self.run_checked(
            "snapshot import",
            &["snapshot", "import", &archive_arg, "--group", &group],
        )
        .await?;
        let cpus = spec.cpus.to_string();
        let memory = format!("{}M", spec.memory_mib);
        let mut args: Vec<String> = [
            "snapshot", "restore", "--name", &spec.name, "--cpus", &cpus, "--memory", &memory,
        ]
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
        for (host, guest) in &spec.ports {
            args.push("--port".into());
            args.push(format!("127.0.0.1:{host}:{guest}"));
        }
        args.extend(restore_network_args(&spec.network)?);
        args.push(group);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let restored = self.run_checked("snapshot restore", &args).await;
        // Restoring boots it; a restored computer stays stopped until use,
        // and a failed restore must not leave one running either.
        let stopped = match self.state(&spec.name).await? {
            None | Some(RuntimeState::Stopped) => Ok(()),
            Some(_) => self.control("stop", &["stop", &spec.name]).await,
        };
        restored?;
        stopped?;
        match self.state(&spec.name).await? {
            Some(RuntimeState::Stopped) => Ok(()),
            other => Err(RuntimeError::Failed {
                operation: "snapshot restore".into(),
                message: format!("the restored computer is {other:?}, not stopped"),
            }),
        }
    }

    async fn remove(&self, name: &str) -> Result<(), RuntimeError> {
        self.control("remove", &["remove", "--quiet", name]).await
    }

    async fn copy_out(&self, name: &str, guest: &str, host: &Path) -> Result<(), RuntimeError> {
        let source = format!("{name}:{guest}");
        let host = host.display().to_string();
        self.control("copy", &["copy", "--quiet", &source, &host])
            .await
    }

    async fn exec(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        // `--stream` keeps the runtime from buffering the whole output
        // (it is bounded here, and by the guest scripts); `--timeout` kills
        // the guest command itself.
        let guest_timeout = format!("{}s", timeout.as_secs().max(1));
        let mut all = vec![
            "exec",
            "--stream",
            "--timeout",
            &guest_timeout,
            name,
            "--",
            program,
        ];
        all.extend_from_slice(args);
        self.run("exec", &all, timeout).await
    }

    async fn exec_input(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        input: &[u8],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let guest_timeout = format!("{}s", timeout.as_secs().max(1));
        let mut all = vec![
            "exec",
            "--stream",
            "--timeout",
            &guest_timeout,
            name,
            "--",
            program,
        ];
        all.extend_from_slice(args);
        self.run_input("exec", &all, Some(input), timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_references_cannot_become_options() {
        assert!(valid_image("python:3.12-slim"));
        assert!(valid_image("ghcr.io/else/desktop@sha256:abc123"));
        for bad in ["--help", "--replace", "", "a b", "x;rm"] {
            assert!(!valid_image(bad), "{bad}");
        }
    }
}
