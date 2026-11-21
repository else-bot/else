//! The prelude the live end-to-end runs share: an `else-api` installation
//! driven through its binary and HTTP API, real Microsandbox guests, and
//! bounded commands so a stuck process fails the run instead of hanging it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

pub const IMAGE: &str = "else-guest:2";

/// Whether the opt-in variable `var` is set to `1`.
pub fn enabled(var: &str) -> bool {
    std::env::var(var).as_deref() == Ok("1")
}

pub fn msb() -> String {
    format!("{}/.microsandbox/bin/msb", std::env::var("HOME").unwrap())
}

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

static LOG: std::sync::OnceLock<(&'static str, PathBuf)> = std::sync::OnceLock::new();

/// Start a fresh evidence log at `target/acceptance/<tag>.log`, which
/// [`note`] appends to, and return its path.
pub fn log_to(tag: &'static str) -> PathBuf {
    let evidence = root().join("target/acceptance");
    std::fs::create_dir_all(&evidence).unwrap();
    let log = evidence.join(format!("{tag}.log"));
    let _ = std::fs::remove_file(&log);
    LOG.set((tag, log.clone())).unwrap();
    log
}

pub fn note(text: &str) {
    eprintln!("{text}");
    if let Some((tag, path)) = LOG.get() {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "[{tag}] {text}").unwrap();
    }
}

pub fn bounded(
    command: std::process::Command,
    limit: Duration,
) -> Result<std::process::Output, String> {
    bounded_with(command, None, limit)
}

/// Run `command` with `input` on its standard input, killing it after
/// `limit`. Output goes to temporary files, so a descendant that keeps them
/// open cannot hold the call past `limit`, and a short write of `input`
/// fails the call.
pub fn bounded_with(
    mut command: std::process::Command,
    input: Option<&str>,
    limit: Duration,
) -> Result<std::process::Output, String> {
    use std::io::{Read, Seek, Write};
    let what = format!("{:?}", command.get_args().take(4).collect::<Vec<_>>());
    let fail = |e: std::io::Error| format!("{what}: {e}");
    let mut stdout = tempfile::tempfile().map_err(fail)?;
    let mut stderr = tempfile::tempfile().map_err(fail)?;
    let mut child = command
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(stdout.try_clone().map_err(fail)?)
        .stderr(stderr.try_clone().map_err(fail)?)
        .spawn()
        .map_err(fail)?;
    let writer = match (input, child.stdin.take()) {
        (Some(input), Some(mut stdin)) => {
            let input = input.to_owned();
            Some(std::thread::spawn(move || {
                stdin.write_all(input.as_bytes())
            }))
        }
        _ => None,
    };
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(fail)? {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{what} did not finish within {limit:?}; killed"));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    if let Some(writer) = writer {
        writer
            .join()
            .map_err(|_| format!("{what}: the input writer panicked"))?
            .map_err(|e| format!("{what}: writing its input: {e}"))?;
    }
    let read = |file: &mut std::fs::File| {
        let mut bytes = Vec::new();
        file.rewind()
            .and_then(|()| file.read_to_end(&mut bytes))
            .map(|_| bytes)
            .map_err(fail)
    };
    Ok(std::process::Output {
        status,
        stdout: read(&mut stdout)?,
        stderr: read(&mut stderr)?,
    })
}

/// Run `command` under `limit` and require it to succeed.
fn succeed(command: std::process::Command, limit: Duration) {
    let what = format!("{:?}", command.get_program());
    let out = bounded(command, limit).unwrap();
    assert!(
        out.status.success(),
        "{what}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Copy the guest image from Docker into the runtime home `home`, through a
/// temporary archive in `scratch`.
pub fn load_image(home: &Path, scratch: &Path) {
    let tar = scratch.join("guest.tar");
    let mut save = std::process::Command::new("docker");
    save.args(["save", IMAGE, "-o"]).arg(&tar);
    succeed(save, Duration::from_secs(600));
    let mut load = std::process::Command::new(msb());
    load.env("MSB_HOME", home)
        .args(["load", "-q", "-t", IMAGE, "-i"])
        .arg(&tar);
    succeed(load, Duration::from_secs(600));
    std::fs::remove_file(&tar).unwrap();
}

/// An installation run through the `else-api` binary.
pub struct Install {
    pub data: PathBuf,
    pub database_url: String,
    pub log: PathBuf,
}

impl Install {
    pub fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_else-api"));
        command
            .current_dir(super::without_dotenv(self.data.parent().unwrap()))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", std::env::var("HOME").unwrap())
            .env("DATABASE_URL", &self.database_url)
            .env("ELSE_DATA_DIR", &self.data)
            .env("ELSE_MAX_ACTIVE_TURNS", "1")
            .env("ELSE_MSB_BIN", msb())
            .env("ELSE_VM_IMAGE", IMAGE)
            .env("ELSE_VM_DESKTOP", "1")
            .env("ELSE_BACKUP_INTERVAL_SECS", "0")
            .env("RUST_LOG", "info,sqlx=warn")
            .args(args);
        command
    }

    pub fn run(&self, args: &[&str]) {
        self.run_output(args);
    }

    /// Run `else-api` with `args` and return its standard output and error.
    pub fn run_output(&self, args: &[&str]) -> String {
        let out = bounded(self.command(args), Duration::from_secs(900)).unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "else-api {args:?}: {text}");
        text
    }

    pub async fn serve(&self) -> Api {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        let child = self
            .command(&["serve"])
            .env("ELSE_API_BIND", "127.0.0.1:0")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let token = std::fs::read_to_string(self.data.join("owner-token"))
            .unwrap()
            .trim()
            .to_owned();
        let Some(base) = super::ready_url(&self.data, &child, super::SETTLE).await else {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            panic!("the API did not start; see {}", self.log.display());
        };
        Api {
            child: Some(child),
            base,
            token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap(),
        }
    }
}

/// A running `else-api serve`, killed when dropped.
pub struct Api {
    child: Option<std::process::Child>,
    pub base: String,
    pub token: String,
    pub http: reqwest::Client,
}

impl Api {
    pub async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    /// Send the owner message `text` to `bot` on `provider` and return the
    /// new turn.
    pub async fn send(&self, bot: &str, provider: &str, text: &str) -> Uuid {
        let (status, body) = self
            .call(
                reqwest::Method::POST,
                &format!("/v1/bots/{bot}/turns"),
                Some(
                    json!({ "request_id": Uuid::new_v4(), "text": text, "provider_id": provider }),
                ),
            )
            .await;
        assert!(status < 300, "send: {status} {body}");
        body["turn"]["id"].as_str().unwrap().parse().unwrap()
    }

    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// Stop the API with SIGTERM and require a clean exit.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SAFETY: signalling our own child.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let started = Instant::now();
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if started.elapsed() > super::SETTLE {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("the API did not stop within 60 s of SIGTERM; killed");
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            assert!(status.success(), "the API exited with {status}");
        }
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The computers the runtime home `home` lists.
pub fn listed(home: &Path) -> Result<Vec<String>, String> {
    let mut command = std::process::Command::new(msb());
    command
        .env("MSB_HOME", home)
        .args(["list", "--format", "json"]);
    let out = bounded(command, Duration::from_secs(60))?;
    if !out.status.success() {
        return Err(format!(
            "msb list in {}: {}",
            home.display(),
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let list: Vec<Value> = serde_json::from_slice(&out.stdout)
        .map_err(|e| format!("msb list in {}: {e}", home.display()))?;
    Ok(list
        .iter()
        .filter_map(|l| l["name"].as_str().map(str::to_owned))
        .collect())
}

/// Stop and remove every computer in `home`, and return what went wrong.
pub fn clear(home: &Path) -> Vec<String> {
    let mut problems = Vec::new();
    match listed(home) {
        Err(e) => problems.push(e),
        Ok(names) => {
            for name in names {
                for args in [["stop", "--force"].as_slice(), ["rm"].as_slice()] {
                    let mut command = std::process::Command::new(msb());
                    command.env("MSB_HOME", home).args(args).arg(&name);
                    if let Err(e) = bounded(command, Duration::from_secs(120)) {
                        problems.push(e);
                    }
                }
            }
        }
    }
    problems
}

/// Clears the runtime home it holds when dropped.
pub struct Computers(pub PathBuf);

impl Drop for Computers {
    fn drop(&mut self) {
        let problems = clear(&self.0);
        if !problems.is_empty() {
            eprintln!("cleanup of {}: {problems:?}", self.0.display());
        }
    }
}

/// Wait for `turn` to reach a final status, and return it with the turn's
/// assistant text.
pub async fn finish(db: &sqlx::SqlitePool, turn: Uuid, timeout: Duration) -> (String, String) {
    let started = Instant::now();
    let mut last_status = String::new();
    tokio::time::timeout(timeout, async {
        loop {
            let (status, error): (String, Option<String>) =
                sqlx::query_as("SELECT status, error_message FROM turns WHERE id = ?1")
                    .bind(turn)
                    .fetch_one(db)
                    .await
                    .unwrap();
            if ["completed", "failed", "interrupted", "uncertain"].contains(&status.as_str()) {
                let text: String = sqlx::query_scalar(
                    "SELECT coalesce(group_concat(text, ' | ' ORDER BY seq), '') FROM items
                 WHERE turn_id = ?1 AND kind = 'assistant'",
                )
                .bind(turn)
                .fetch_one(db)
                .await
                .unwrap();
                note(&format!(
                    "  -> {status} {error:?} after {:?}: {}",
                    started.elapsed(),
                    text.chars().take(600).collect::<String>()
                ));
                return (status, text);
            }
            last_status = status;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("turn {turn} still {last_status}"))
}

/// Run `script` in the guest `name` as the bot user, or as root when
/// `root`, and return its trimmed standard output.
pub fn guest(home: &Path, name: &str, script: &str, input: Option<&str>, root: bool) -> String {
    let mut command = std::process::Command::new(msb());
    command
        .env("MSB_HOME", home)
        .args(["exec", "--timeout", "120s", name, "--"]);
    if root {
        command.args(["/bin/sh", "-c", script]);
    } else {
        command.args([
            "/usr/sbin/runuser",
            "-u",
            "elsebot",
            "--",
            "/bin/sh",
            "-c",
            script,
        ]);
    }
    let out = bounded_with(command, input, Duration::from_secs(150)).unwrap();
    assert!(
        out.status.success(),
        "guest {script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

pub async fn runtime_name(db: &sqlx::SqlitePool, bot: &str) -> String {
    sqlx::query_scalar("SELECT runtime_name FROM bot_computers WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(db)
        .await
        .unwrap()
}

/// The tool activity `bot` recorded during `turn`, in order.
pub async fn turn_activity(db: &sqlx::SqlitePool, bot: &str, turn: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT body FROM memory_entry WHERE bot_id = ?1 AND kind = 'tool_activity'
           AND source->>'turn_id' = ?2 ORDER BY seq",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .bind(turn.to_string())
    .fetch_all(db)
    .await
    .unwrap()
}

/// The messages the guest's SMTP capture received, as recipients, sender
/// and data.
pub fn messages(home: &Path, name: &str) -> Vec<(Vec<String>, String, String)> {
    let text = guest(
        home,
        name,
        "cat /home/elsebot/.capture/messages.jsonl 2>/dev/null || true",
        None,
        false,
    );
    text.lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["event"] == "message")
        .map(|e| {
            (
                e["to"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().to_owned())
                    .collect(),
                e["from"].as_str().unwrap_or_default().to_owned(),
                e["data"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}
