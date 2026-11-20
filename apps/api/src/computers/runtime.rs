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

use std::collections::HashMap;
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
    /// A create could not bind the desktop's host port; another port may work.
    #[error("create failed: {0}")]
    PortInUse(String),
}

/// A running computer's live use, as the runtime last sampled it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Usage {
    /// Percent of one vCPU: 200 is two busy vCPUs.
    pub cpu_percent: f32,
    /// The vCPUs it runs with now, when the runtime knows.
    pub cpus: Option<u32>,
    /// Memory the guest uses, when it reported it.
    pub memory_bytes: Option<u64>,
    pub memory_limit_bytes: u64,
    /// The guest's writable disk, used and in all, when it reported them.
    pub disk: Option<(u64, u64)>,
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
    /// The vCPUs and memory a running computer may grow to (the boot size
    /// when it cannot grow).
    pub max_cpus: u32,
    pub max_memory_mib: u32,
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

    /// Grow a running computer's vCPUs and memory, within the maximums it
    /// booted with. The guest keeps the added memory until it stops.
    async fn grow(&self, name: &str, cpus: u32, memory_mib: u32) -> Result<(), RuntimeError> {
        let _ = (name, cpus, memory_mib);
        Err(RuntimeError::Unavailable(
            "this runtime cannot resize a computer".into(),
        ))
    }

    /// Set a stopped computer's boot vCPUs and memory and the maximums it
    /// may grow to, for its next start.
    async fn configure(
        &self,
        name: &str,
        cpus: u32,
        memory_mib: u32,
        max_cpus: u32,
        max_memory_mib: u32,
    ) -> Result<(), RuntimeError> {
        let _ = (name, cpus, memory_mib, max_cpus, max_memory_mib);
        Err(RuntimeError::Unavailable(
            "this runtime cannot resize a computer".into(),
        ))
    }
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

    /// Run a command with its standard output written to the host file
    /// `stdout`, so the output can outgrow both memory and the guest's disk.
    /// Only standard error is captured.
    async fn exec_to_file(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        stdout: &Path,
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let _ = (name, program, args, stdout, timeout);
        Err(RuntimeError::Unavailable(
            "this runtime cannot stream a command's output".into(),
        ))
    }

    /// Run a command with the host file `stdin` on its standard input.
    async fn exec_from_file(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        stdin: &Path,
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let _ = (name, program, args, stdin, timeout);
        Err(RuntimeError::Unavailable(
            "this runtime cannot stream a command's input".into(),
        ))
    }

    /// Copy a guest file to a host path.
    async fn copy_out(&self, name: &str, guest: &str, host: &Path) -> Result<(), RuntimeError>;

    /// Export a stopped computer's disk to an archive at `archive`, with its
    /// image when `with_image`. An archive without its image
    /// restores only where the image is already cached, such as after
    /// importing an archive that carries it. The computer is not started.
    async fn export_disk(
        &self,
        name: &str,
        archive: &Path,
        with_image: bool,
    ) -> Result<(), RuntimeError> {
        let _ = (name, archive, with_image);
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

    /// The host directories mounted into a computer, as it was created.
    async fn mounts(&self, name: &str) -> Result<Vec<Mount>, RuntimeError> {
        let _ = name;
        Ok(Vec::new())
    }

    /// Remove a stopped computer and its disk. Only used for a computer
    /// else.bot itself just created and never handed out (a failed image
    /// upgrade's replacement).
    async fn remove(&self, name: &str) -> Result<(), RuntimeError> {
        let _ = name;
        Err(RuntimeError::Unavailable("remove is not supported".into()))
    }

    /// Live use of each running computer, by name. The runtime samples it
    /// continuously, so reading it is cheap.
    async fn usage(&self) -> Result<HashMap<String, Usage>, RuntimeError> {
        Ok(HashMap::new())
    }

    /// The host bytes allocated to each computer's files, stopped ones
    /// included, by name. This scans their files.
    async fn disks(&self) -> Result<HashMap<String, u64>, RuntimeError> {
        Ok(HashMap::new())
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
/// Metrics are read from shared memory; the list that shows them is polled.
const METRICS_TIMEOUT: Duration = Duration::from_secs(10);
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
        self.run_io(operation, args, Stdin::Null, None, timeout)
            .await
    }

    /// Run `msb` with `stdin` as its input and its standard output captured,
    /// or written to `stdout` when given.
    async fn run_io(
        &self,
        operation: &str,
        args: &[&str],
        stdin: Stdin<'_>,
        stdout: Option<std::fs::File>,
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        use std::process::Stdio;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut command = self.command();
        command.args(args).stderr(Stdio::piped());
        command.stdout(stdout.map_or_else(Stdio::piped, Stdio::from));
        let input = match stdin {
            Stdin::Null => None,
            Stdin::Bytes(input) => {
                command.stdin(Stdio::piped());
                Some(input)
            }
            Stdin::File(file) => {
                command.stdin(Stdio::from(file));
                None
            }
        };
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
        async fn capture(stream: Option<impl tokio::io::AsyncRead + Unpin>) -> (Vec<u8>, bool) {
            let mut kept = Vec::new();
            let mut truncated = false;
            let Some(mut stream) = stream else {
                return (kept, truncated);
            };
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
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
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

/// `msb metrics --format json`: running computers only.
fn parse_metrics(json: &str) -> Option<HashMap<String, Usage>> {
    #[derive(Deserialize)]
    struct Sample {
        name: String,
        state: String,
        cpus: Option<u32>,
        cpu_percent: f32,
        memory_bytes: u64,
        memory_limit_bytes: u64,
        upper_used_bytes: Option<u64>,
        upper_free_bytes: Option<u64>,
    }
    let samples: Vec<Sample> = serde_json::from_str(json.trim()).ok()?;
    Some(
        samples
            .into_iter()
            .filter(|sample| sample.state == "running")
            .map(|sample| {
                let usage = Usage {
                    cpu_percent: sample.cpu_percent,
                    cpus: sample.cpus,
                    memory_bytes: (sample.memory_bytes > 0).then_some(sample.memory_bytes),
                    memory_limit_bytes: sample.memory_limit_bytes,
                    disk: sample
                        .upper_used_bytes
                        .zip(sample.upper_free_bytes)
                        .map(|(used, free)| (used, used + free)),
                };
                (sample.name, usage)
            })
            .collect(),
    )
}

/// `msb df --format json`: the blocks each computer's directory holds.
fn parse_disks(json: &str) -> Option<HashMap<String, u64>> {
    #[derive(Deserialize)]
    struct Storage {
        sandboxes: Category,
    }
    #[derive(Deserialize)]
    struct Category {
        items: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        name: String,
        logical_bytes: Option<u64>,
        allocated_bytes: Option<u64>,
    }
    let storage: Storage = serde_json::from_str(json.trim()).ok()?;
    Some(
        storage
            .sandboxes
            .items
            .into_iter()
            .filter_map(|item| Some((item.name, item.allocated_bytes.or(item.logical_bytes)?)))
            .collect(),
    )
}

pub fn parse_state(status: &str) -> RuntimeState {
    match status {
        "Running" => RuntimeState::Running,
        "Paused" => RuntimeState::Paused,
        "Stopped" => RuntimeState::Stopped,
        _ => RuntimeState::Failed,
    }
}

fn mount_arg(mount: &Mount, operation: &str) -> Result<String, RuntimeError> {
    // A colon in the host path would shift the fields.
    let host = mount
        .host
        .to_str()
        .filter(|h| !h.contains(':'))
        .ok_or_else(|| RuntimeError::Failed {
            operation: operation.into(),
            message: format!(
                "{} cannot be mounted (not UTF-8, or a ':')",
                mount.host.display()
            ),
        })?;
    let (guest, uid) = (&mount.guest, mount.uid);
    Ok(format!("{host}:{guest}:uid={uid},gid={uid}"))
}

fn inspected_mounts(json: &str) -> Option<Vec<Mount>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let mounts = value["config"]["mounts"].as_array()?;
    Some(
        mounts
            .iter()
            .filter(|m| m["type"] == "Bind")
            .filter_map(|m| {
                Some(Mount {
                    host: PathBuf::from(m["host"].as_str()?),
                    guest: m["guest"].as_str()?.to_owned(),
                    uid: m["options"]["override_uid"].as_u64().unwrap_or(0) as u32,
                })
            })
            .collect(),
    )
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

    async fn usage(&self) -> Result<HashMap<String, Usage>, RuntimeError> {
        let out = self
            .run("metrics", &["metrics", "--format", "json"], METRICS_TIMEOUT)
            .await?;
        if out.status != Some(0) {
            return Err(RuntimeError::Failed {
                operation: "metrics".into(),
                message: last_line(&out.stderr, &out.stdout),
            });
        }
        parse_metrics(&out.stdout).ok_or_else(|| RuntimeError::Failed {
            operation: "metrics".into(),
            message: "unreadable output".into(),
        })
    }

    async fn disks(&self) -> Result<HashMap<String, u64>, RuntimeError> {
        let out = self
            .run("df", &["df", "--format", "json"], CONTROL_TIMEOUT)
            .await?;
        if out.status != Some(0) {
            return Err(RuntimeError::Failed {
                operation: "df".into(),
                message: last_line(&out.stderr, &out.stdout),
            });
        }
        parse_disks(&out.stdout).ok_or_else(|| RuntimeError::Failed {
            operation: "df".into(),
            message: "unreadable output".into(),
        })
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
        if spec.max_cpus > spec.cpus {
            args.push("--max-cpus".into());
            args.push(spec.max_cpus.to_string());
        }
        if spec.max_memory_mib > spec.memory_mib {
            args.push("--max-memory".into());
            args.push(format!("{}M", spec.max_memory_mib));
        }
        for (host, guest) in &spec.ports {
            args.push("--port".into());
            args.push(format!("127.0.0.1:{host}:{guest}"));
        }
        for (host, guest) in &spec.files {
            args.push("--copy-file".into());
            args.push(format!("{}:{guest}", host.display()));
        }
        for mount in &spec.mounts {
            args.push("--mount-dir".into());
            args.push(mount_arg(mount, "create")?);
        }
        args.push("--".into());
        args.push(spec.image.clone());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match self.control("create", &args).await {
            Err(RuntimeError::Failed { message, .. })
                if !spec.ports.is_empty() && {
                    let lower = message.to_lowercase();
                    lower.contains("address") || lower.contains("in use")
                } =>
            {
                Err(RuntimeError::PortInUse(message))
            }
            other => other,
        }
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

    async fn grow(&self, name: &str, cpus: u32, memory_mib: u32) -> Result<(), RuntimeError> {
        let cpus = cpus.to_string();
        let memory = format!("{memory_mib}M");
        self.control(
            "modify",
            &["modify", name, "--cpus", &cpus, "--memory", &memory],
        )
        .await
    }

    async fn configure(
        &self,
        name: &str,
        cpus: u32,
        memory_mib: u32,
        max_cpus: u32,
        max_memory_mib: u32,
    ) -> Result<(), RuntimeError> {
        let (cpus, max_cpus) = (cpus.to_string(), max_cpus.to_string());
        let memory = format!("{memory_mib}M");
        let max_memory = format!("{max_memory_mib}M");
        self.control(
            "modify",
            &[
                "modify",
                name,
                "--cpus",
                &cpus,
                "--memory",
                &memory,
                "--max-cpus",
                &max_cpus,
                "--max-memory",
                &max_memory,
            ],
        )
        .await
    }

    async fn export_disk(
        &self,
        name: &str,
        archive: &Path,
        with_image: bool,
    ) -> Result<(), RuntimeError> {
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
            let mut args = vec!["snapshot", "export", &member];
            if with_image {
                args.push("--with-image");
            }
            args.extend(["--output", &archive_arg]);
            self.run_checked("snapshot export", &args).await
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
        // A computer created with a mount is restored only with a binding
        // for it (`msb` refuses otherwise).
        for mount in &spec.mounts {
            args.push("-v".into());
            args.push(mount_arg(mount, "snapshot restore")?);
        }
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

    async fn mounts(&self, name: &str) -> Result<Vec<Mount>, RuntimeError> {
        let out = self
            .run(
                "inspect",
                &["inspect", name, "--format", "json"],
                CONTROL_TIMEOUT,
            )
            .await?;
        if out.status != Some(0) {
            return Err(RuntimeError::Failed {
                operation: "inspect".into(),
                message: last_line(&out.stderr, &out.stdout),
            });
        }
        inspected_mounts(&out.stdout).ok_or_else(|| RuntimeError::Failed {
            operation: "inspect".into(),
            message: "unreadable output".into(),
        })
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
        let guest_timeout = guest_timeout(timeout);
        let all = exec_args(&guest_timeout, name, program, args);
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
        let guest_timeout = guest_timeout(timeout);
        let all = exec_args(&guest_timeout, name, program, args);
        self.run_io("exec", &all, Stdin::Bytes(input), None, timeout)
            .await
    }

    async fn exec_to_file(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        stdout: &Path,
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let file = std::fs::File::create(stdout).map_err(|e| RuntimeError::Failed {
            operation: "exec".into(),
            message: format!("{}: {e}", stdout.display()),
        })?;
        let guest_timeout = guest_timeout(timeout);
        let all = exec_args(&guest_timeout, name, program, args);
        self.run_io("exec", &all, Stdin::Null, Some(file), timeout)
            .await
    }

    async fn exec_from_file(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        stdin: &Path,
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let file = std::fs::File::open(stdin).map_err(|e| RuntimeError::Failed {
            operation: "exec".into(),
            message: format!("{}: {e}", stdin.display()),
        })?;
        let guest_timeout = guest_timeout(timeout);
        let all = exec_args(&guest_timeout, name, program, args);
        self.run_io("exec", &all, Stdin::File(file), None, timeout)
            .await
    }
}

enum Stdin<'a> {
    Null,
    Bytes(&'a [u8]),
    File(std::fs::File),
}

/// `--timeout` kills the guest command itself.
fn guest_timeout(timeout: Duration) -> String {
    format!("{}s", timeout.as_secs().max(1))
}

/// `msb exec` arguments. `--stream` keeps the runtime from buffering the
/// whole output (it is bounded or written to a file here).
fn exec_args<'a>(
    guest_timeout: &'a str,
    name: &'a str,
    program: &'a str,
    args: &[&'a str],
) -> Vec<&'a str> {
    let mut all = vec![
        "exec",
        "--stream",
        "--timeout",
        guest_timeout,
        name,
        "--",
        program,
    ];
    all.extend_from_slice(args);
    all
}

#[cfg(test)]
mod mount_tests {
    use super::*;

    #[test]
    fn metrics_and_disks_are_read_from_msb_json() {
        let metrics = parse_metrics(
            r#"[
              {"name": "a", "state": "running", "cpus": 2, "cpu_percent": 57.5,
               "memory_bytes": 1073741824, "memory_limit_bytes": 4294967296,
               "upper_used_bytes": 10, "upper_free_bytes": 30, "uptime_secs": 3.5},
              {"name": "b", "state": "running", "cpu_percent": 0.0,
               "memory_bytes": 0, "memory_limit_bytes": 2147483648},
              {"name": "c", "state": "exited", "cpu_percent": 0.0,
               "memory_bytes": 5, "memory_limit_bytes": 5}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            metrics.get("a"),
            Some(&Usage {
                cpu_percent: 57.5,
                cpus: Some(2),
                memory_bytes: Some(1 << 30),
                memory_limit_bytes: 4 << 30,
                disk: Some((10, 40)),
            })
        );
        assert_eq!(metrics["b"].memory_bytes, None, "not reported");
        assert!(!metrics.contains_key("c"), "only running computers");
        assert_eq!(parse_metrics("[]"), Some(HashMap::new()));
        assert_eq!(parse_metrics("No running sandboxes."), None);

        let disks = parse_disks(
            r#"{"images": {"items": []},
                "sandboxes": {"count": 2, "items": [
                  {"name": "a", "path": "/x/a", "logical_bytes": 34359738368,
                   "allocated_bytes": 1048576, "in_use": true, "reclaimable": false, "reasons": []},
                  {"name": "b", "path": "/x/b", "logical_bytes": 100, "allocated_bytes": null},
                  {"name": "c", "path": "/x/c", "logical_bytes": null, "allocated_bytes": null}
                ], "notes": []},
                "notes": []}"#,
        )
        .unwrap();
        assert_eq!(disks.get("a"), Some(&(1 << 20)), "allocated blocks");
        assert_eq!(disks["b"], 100, "file lengths when blocks are unknown");
        assert!(!disks.contains_key("c"), "unknown size");
    }

    #[test]
    fn bind_mounts_are_read_from_inspect() {
        let json = r#"{"config": {"mounts": [
            {"type": "Bind", "guest": "/root/work", "host": "/p/bots/b",
             "options": {"override_uid": 0, "override_gid": 0}},
            {"type": "Bind", "guest": "/home/elsebot/work", "host": "/q",
             "options": {"override_uid": 1000, "override_gid": 1000}},
            {"type": "Named", "guest": "/data", "name": "v"}]}}"#;
        assert_eq!(
            inspected_mounts(json).unwrap(),
            vec![
                Mount {
                    host: "/p/bots/b".into(),
                    guest: "/root/work".into(),
                    uid: 0
                },
                Mount {
                    host: "/q".into(),
                    guest: "/home/elsebot/work".into(),
                    uid: 1000
                },
            ]
        );
        assert_eq!(
            inspected_mounts(r#"{"config": {"mounts": []}}"#).unwrap(),
            vec![]
        );
        assert!(inspected_mounts("not json").is_none());
        let arg = mount_arg(
            &Mount {
                host: "/a:b".into(),
                guest: "/root/work".into(),
                uid: 0,
            },
            "create",
        );
        assert!(arg.is_err(), "a colon would shift the fields");
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
