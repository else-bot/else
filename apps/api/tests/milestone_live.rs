//! POC-032: the VM automation and sidebar categories milestone, end to end,
//! against the real application: the `else-api` binary with the owner's
//! Codex and Claude logins, two real `else-guest:2` computers, local
//! fixtures (a capture-only SMTP server, a search site, a randomized visual
//! page), no mocked screenshots, DOM-only automation, or synthetic provider
//! replies. A Codex bot writes and runs code and sends exactly ten captured
//! messages through a lost acknowledgment; Claude writes a CDP script that
//! searches the visible browser; a second Codex bot completes a randomized
//! visual task and types into a terminal; the bots are organized into
//! categories, one moved while it runs a job across an API restart; a
//! coordinated backup is restored into a new installation and checked for
//! categories, project files, usable tooling, and browser state; isolation,
//! the host boundary, owner stop, and disabled native sub-agents are checked.
//! It consumes subscription quota and boots VMs, so it runs only with
//! `ELSE_LIVE_MILESTONE=1`:
//!
//! ELSE_LIVE_MILESTONE=1 cargo test -p else-api --test milestone_live -- --nocapture

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

const IMAGE: &str = "else-guest:2";
const FIXTURE: &str = include_str!("fixtures/smtp_capture.py");
const SEARCH_SITE: &str = include_str!("fixtures/cdp_site.py");
const VISUAL_SITE: &str = include_str!("fixtures/input_site.py");

fn live() -> bool {
    std::env::var("ELSE_LIVE_MILESTONE").as_deref() == Ok("1")
}

fn msb() -> String {
    format!("{}/.microsandbox/bin/msb", std::env::var("HOME").unwrap())
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

static LOG: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

fn note(text: &str) {
    eprintln!("{text}");
    if let Some(path) = LOG.get() {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "[milestone] {text}").unwrap();
    }
}

/// Run a process to completion within `limit`: past it the process is
/// killed and reaped and an error returned, so no step can hang the run.
fn bounded(
    command: std::process::Command,
    limit: Duration,
) -> Result<std::process::Output, String> {
    bounded_with(command, None, limit)
}

/// `bounded`, writing `input` to the process's standard input first.
fn bounded_with(
    mut command: std::process::Command,
    input: Option<&str>,
    limit: Duration,
) -> Result<std::process::Output, String> {
    use std::io::{Read, Write};
    let what = format!("{:?}", command.get_args().take(4).collect::<Vec<_>>());
    let mut child = command
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{what}: {e}"))?;
    let read = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let input = input.to_owned();
        // A writer thread: a process that never reads cannot block us.
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let stdout = read(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = read(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| format!("{what}: {e}"))? {
            break status;
        }
        if started.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{what} did not finish within {limit:?}; killed"));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

struct Install {
    data: PathBuf,
    database_url: String,
    log: PathBuf,
}

impl Install {
    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_else-api"));
        command
            .current_dir(common::without_dotenv(self.data.parent().unwrap()))
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
            .env("ELSE_BACKUP_DIR", self.data.join("backups"))
            .env("RUST_LOG", "info,sqlx=warn")
            .args(args);
        command
    }

    /// Operator commands (setup, backup, restore) get 15 minutes.
    fn run(&self, args: &[&str]) {
        let out = bounded(self.command(args), Duration::from_secs(900)).unwrap();
        assert!(
            out.status.success(),
            "else-api {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Run an operator command and return its output.
    fn run_output(&self, args: &[&str]) -> String {
        let out = bounded(self.command(args), Duration::from_secs(900)).unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(out.status.success(), "else-api {args:?}: {text}");
        text
    }

    async fn serve(&self) -> Api {
        let port = common::free_port();
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .unwrap();
        let child = self
            .command(&["serve"])
            .env("ELSE_API_BIND", format!("127.0.0.1:{port}"))
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let token = std::fs::read_to_string(self.data.join("owner-token"))
            .unwrap()
            .trim()
            .to_owned();
        let api = Api {
            child: Some(child),
            base: format!("http://127.0.0.1:{port}"),
            token,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .unwrap(),
        };
        for _ in 0..600 {
            if api
                .http
                .get(format!("{}/readyz", api.base))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return api;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the API did not start; see {}", self.log.display());
    }
}

struct Api {
    child: Option<std::process::Child>,
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
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

    async fn send(&self, bot: &str, provider: &str, text: &str) -> Uuid {
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

    /// SIGTERM and wait: a normal shutdown.
    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // SAFETY: signalling our own child.
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let started = Instant::now();
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if started.elapsed() > Duration::from_secs(60) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("the API did not stop within 60 s of SIGTERM; killed");
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            assert!(status.success(), "the API exited with {status}");
        }
    }

    /// An abrupt end (SIGKILL): the API gets no chance to clean up.
    fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Removes every computer in each runtime home and the restored database.
/// `finish` does it before the run reports success and fails unless the
/// runtime lists nothing and the database is gone; `Drop` repeats it after
/// a failed assertion. When removal cannot be confirmed the temporary
/// directory (runtime homes included) is kept for inspection.
struct Cleanup {
    homes: Vec<PathBuf>,
    databases: std::sync::Mutex<Vec<String>>,
    admin: String,
    dir: std::sync::Mutex<Option<tempfile::TempDir>>,
    done: std::sync::atomic::AtomicBool,
}

fn listed(home: &Path) -> Result<Vec<String>, String> {
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

impl Cleanup {
    /// Remove, then confirm; returns what could not be confirmed.
    fn clean(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for home in &self.homes {
            if !home.exists() {
                continue;
            }
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
            match listed(home) {
                Ok(left) if left.is_empty() => {}
                Ok(left) => problems.push(format!("{} still lists {left:?}", home.display())),
                Err(e) => problems.push(e),
            }
        }
        let admin = self.admin.clone();
        let databases = self.databases.lock().unwrap().clone();
        let result = std::thread::spawn(move || {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(async move {
                    let options = else_api::db::options(&admin).map_err(|e| e.to_string())?;
                    for db in &databases {
                        common::remove_database(&options, db);
                    }
                    Ok::<(), String>(())
                })
        })
        .join();
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => problems.push(e),
            Err(_) => problems.push("the database cleanup panicked".into()),
        }
        problems
    }

    /// Keep the directory when removal was not confirmed.
    fn settle(&self, problems: &[String]) -> Option<PathBuf> {
        let dir = self.dir.lock().unwrap().take()?;
        if problems.is_empty() {
            drop(dir);
            None
        } else {
            Some(dir.keep())
        }
    }

    /// Checked cleanup before the run reports success.
    fn finish(&self) {
        self.done.store(true, std::sync::atomic::Ordering::SeqCst);
        let problems = self.clean();
        if let Some(kept) = self.settle(&problems) {
            panic!(
                "cleanup not confirmed (kept {}): {problems:?}",
                kept.display()
            );
        }
        note("cleanup confirmed: no computer in either runtime home, restored database dropped");
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if self.done.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let problems = self.clean();
        if let Some(kept) = self.settle(&problems) {
            eprintln!(
                "cleanup not confirmed; kept {} for inspection: {problems:?}",
                kept.display()
            );
        }
    }
}

/// Wait for a turn's final state; returns (status, assistant text).
async fn finish(db: &sqlx::SqlitePool, turn: Uuid, timeout: Duration) -> (String, String) {
    let started = Instant::now();
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
        assert!(started.elapsed() < timeout, "turn {turn} still {status}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Run a command in the guest directly (the test's own fixture setup and
/// inspection, not the bot's tools), as `elsebot` unless `root`.
fn guest(home: &Path, name: &str, script: &str, input: Option<&str>, root: bool) -> String {
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
    // msb's own limit is 120 s; the process gets a little more.
    let out = bounded_with(command, input, Duration::from_secs(150)).unwrap();
    assert!(
        out.status.success(),
        "guest {script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

async fn runtime_name(db: &sqlx::SqlitePool, bot: &str) -> String {
    sqlx::query_scalar("SELECT runtime_name FROM bot_computers WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(db)
        .await
        .unwrap()
}

/// Every captured message: (recipients, sender, data).
fn messages(home: &Path, name: &str) -> Vec<(Vec<String>, String, String)> {
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

async fn turn_activity(db: &sqlx::SqlitePool, bot: &str, turn: Uuid) -> Vec<String> {
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

fn version(program: &str) -> String {
    let mut command = std::process::Command::new(program);
    command.arg("--version");
    bounded(command, Duration::from_secs(30))
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

fn dir_size(path: &Path) -> u64 {
    let mut command = std::process::Command::new("du");
    command.args(["-sb"]).arg(path);
    let out = bounded(command, Duration::from_secs(60)).unwrap();
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// Send and wait; records the latency.
async fn ask(
    api: &Api,
    pool: &sqlx::SqlitePool,
    bot: &str,
    provider: &str,
    text: &str,
) -> (Uuid, String, String) {
    let started = Instant::now();
    let turn = api.send(bot, provider, text).await;
    let (status, reply) = finish(pool, turn, Duration::from_secs(900)).await;
    note(&format!("   ({provider}, {:?})", started.elapsed()));
    (turn, status, reply)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_vm_automation_and_categories_milestone_works_end_to_end() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_MILESTONE=1 for the POC-032 acceptance run");
        return;
    }
    let run_started = Instant::now();
    let evidence = root().join("target/acceptance");
    std::fs::create_dir_all(&evidence).unwrap();
    // On disk, not tmpfs (two installations and a 1.3 GB set do not fit),
    // and short: the runtime's socket paths live under it.
    let dir = tempfile::tempdir_in("/var/tmp").unwrap();
    let tmp = dir.path().to_path_buf();
    let log = evidence.join("milestone.log");
    let _ = std::fs::remove_file(&log);
    LOG.set(log.clone()).unwrap();
    let db = common::TestDb::empty().await;
    let params = else_api::backup::DatabaseParams::parse(db.url.expose()).unwrap();
    let data = tmp.as_path().join("installation");
    let restored_dir = tmp.as_path().join("restored");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = data.join("microsandbox");
    std::fs::create_dir_all(&home).unwrap();
    let target = format!("else_milestone_{}", Uuid::new_v4().simple());
    let admin = db.url.expose().to_owned();
    let cleanup = Cleanup {
        homes: vec![home.clone(), restored_dir.join("microsandbox")],
        databases: std::sync::Mutex::new(vec![target.clone()]),
        admin,
        dir: std::sync::Mutex::new(Some(dir)),
        done: std::sync::atomic::AtomicBool::new(false),
    };
    let tar = tmp.as_path().join("guest.tar");
    let mut save = std::process::Command::new("docker");
    save.args(["save", IMAGE, "-o"]).arg(&tar);
    assert!(
        bounded(save, Duration::from_secs(600))
            .unwrap()
            .status
            .success()
    );
    let mut load = std::process::Command::new(msb());
    load.env("MSB_HOME", &home)
        .args(["load", "-q", "-t", IMAGE, "-i"])
        .arg(&tar);
    assert!(
        bounded(load, Duration::from_secs(600))
            .unwrap()
            .status
            .success()
    );
    std::fs::remove_file(&tar).unwrap();
    let mut inspect = std::process::Command::new("docker");
    inspect.args(["image", "inspect", "--format", "{{.Id}} {{.Size}}", IMAGE]);
    let image_id = bounded(inspect, Duration::from_secs(60)).unwrap();
    note(&format!(
        "versions: {} | {} | {} | image {IMAGE} {}",
        version("codex"),
        version("claude"),
        version(&msb()),
        String::from_utf8_lossy(&image_id.stdout).trim()
    ));
    let install = Install {
        data: data.clone(),
        database_url: db.url.expose().to_owned(),
        log: log.clone(),
    };
    install.run(&["setup"]);
    let mut api = install.serve().await;
    let pool = db.pool.clone();
    for (provider, body) in [
        ("codex", json!({ "reasoning_effort": "low" })),
        (
            "claude",
            json!({ "model": "claude-haiku-5-5", "reasoning_effort": "low", "service_tier": "default" }),
        ),
    ] {
        let (status, body) = api
            .call(
                reqwest::Method::PATCH,
                &format!("/v1/settings/providers/{provider}"),
                Some(body),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        note(&format!("{provider} profile: {}", body["profile"]));
    }

    note("== 1. bots and categories");
    let mut bots = Vec::new();
    for (name, role) in [
        ("Scripter", "Automates tasks with scripts and the browser."),
        ("Hands", "Operates its desktop visually."),
    ] {
        let (_, body) = api
            .call(
                reqwest::Method::POST,
                "/v1/bots",
                Some(json!({ "request_id": Uuid::new_v4(), "name": name, "role": role })),
            )
            .await;
        bots.push(body["bot"]["id"].as_str().unwrap().to_owned());
    }
    let (scripter, hands) = (bots[0].clone(), bots[1].clone());
    let mut categories = Vec::new();
    for name in ["Automation", "Visual"] {
        let (status, body) = api
            .call(
                reqwest::Method::POST,
                "/v1/categories",
                Some(json!({ "request_id": Uuid::new_v4(), "name": name })),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        categories.push(body["category"]["id"].as_str().unwrap().to_owned());
    }
    for (bot, category) in [(&scripter, &categories[0]), (&hands, &categories[1])] {
        let (status, body) = api
            .call(
                reqwest::Method::PATCH,
                &format!("/v1/bots/{bot}/category"),
                Some(json!({ "category_id": category })),
            )
            .await;
        assert_eq!(status, 200, "{body}");
    }

    note("== 2. Scripter (Codex): a multi-file project and its artifact");
    let (_, status, reply) = ask(&api, &pool, &scripter, "codex",
        "In your computer create a small multi-file Python project in ~/work/inventory: data.py with \
         five (item, unit price, quantity) tuples, report.py turning them into CSV rows with line \
         totals and a final TOTAL row, and main.py writing report.csv. Run it and fetch report.csv as \
         an artifact. Reply with the artifact ID and the grand total.").await;
    assert_eq!(status, "completed", "{reply}");
    let artifact: (Uuid, String) = sqlx::query_as(
        "SELECT id, guest_path FROM artifacts WHERE bot_id = ?1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(scripter.parse::<Uuid>().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        artifact.1.ends_with("report.csv") && reply.contains(&artifact.0.to_string()),
        "{reply}"
    );
    let sname = runtime_name(&pool, &scripter).await;

    note(
        "== 3. Scripter (Codex): ten messages through the capture fixture, one acknowledgment lost",
    );
    let password = format!("pw-{}", Uuid::new_v4().simple());
    guest(
        &home,
        &sname,
        "mkdir -p ~/.capture && cat > ~/.capture/smtp_capture.py && chmod 0700 ~/.capture",
        Some(FIXTURE),
        false,
    );
    guest(
        &home,
        &sname,
        "umask 077 && cat > ~/.capture/password",
        Some(&password),
        false,
    );
    guest(
        &home,
        &sname,
        "cd ~/.capture && (/usr/bin/setsid python3 -I smtp_capture.py --port 2525 --user mailer \
         --password-file password --capture messages.jsonl --drop-ack r07@example.test \
         > fixture.log 2>&1 < /dev/null &) ; sleep 1",
        None,
        false,
    );
    let (status, body) = api
        .call(reqwest::Method::PUT, &format!("/v1/bots/{scripter}/task-secrets/smtp"),
              Some(json!({ "env": { "SMTP_HOST": "127.0.0.1", "SMTP_PORT": "2525", "SMTP_USER": "mailer",
                                    "SMTP_PASSWORD": password, "SMTP_FROM": "owner@example.test" } })))
        .await;
    assert_eq!(status, 201, "{body}");
    let recipients: Vec<String> = (1..=10).map(|i| format!("r{i:02}@example.test")).collect();
    let started = Instant::now();
    let (_, status, reply) = ask(
        &api,
        &pool,
        &scripter,
        "codex",
        &format!(
            "Please email this announcement to these ten people now: {}. Subject: \"Team picnic on \
         Saturday\". Body: \"Hi! The team picnic is this Saturday at noon in Riverside Park.\" Use \
         the SMTP server and login in the task secret `smtp`. Write the sender as a Node.js script \
         in its own project and send the whole list in one run.",
            recipients.join(", ")
        ),
    )
    .await;
    assert_eq!(status, "completed", "{reply}");
    note(&format!("ten-message send took {:?}", started.elapsed()));
    let sent = messages(&home, &sname);
    assert_eq!(sent.len(), 10, "exactly ten captured messages");
    let mut seen: Vec<String> = sent.iter().map(|(to, _, _)| to.join(",")).collect();
    seen.sort();
    assert_eq!(seen, recipients, "one message per recipient");
    assert!(
        sent.iter()
            .all(|(_, from, data)| from == "owner@example.test" && data.contains("Riverside Park"))
    );
    // r07's outcome is unknown (the server may have taken the message).
    // Asked plainly, the bot must name exactly r07, from its own records.
    let (_, status, unknown) = ask(
        &api,
        &pool,
        &scripter,
        "codex",
        "Which recipients of that announcement, if any, have an unknown delivery outcome? \
         Name them exactly, or say none.",
    )
    .await;
    assert_eq!(status, "completed", "{unknown}");
    let named: Vec<&String> = recipients
        .iter()
        .filter(|r| unknown.contains(r.split('@').next().unwrap()))
        .collect();
    assert_eq!(
        named,
        vec![&recipients[6]],
        "only r07 is reported uncertain: {unknown}"
    );
    let (_, status, reply) = ask(
        &api,
        &pool,
        &scripter,
        "codex",
        "Please make sure everyone on that list has the picnic announcement; send it to anyone who \
         still needs it.",
    )
    .await;
    assert_eq!(status, "completed", "{reply}");
    assert_eq!(messages(&home, &sname).len(), 10, "no blind retransmission");

    note("== 4. Scripter (Claude): a CDP script searches the local site in the visible browser");
    guest(
        &home,
        &sname,
        "mkdir -p ~/site && cat > ~/site/cdp_site.py",
        Some(SEARCH_SITE),
        false,
    );
    guest(
        &home,
        &sname,
        "cd ~/site && (/usr/bin/setsid python3 cdp_site.py 8411 > site.log 2>&1 < /dev/null &); sleep 1",
        None,
        false,
    );
    let (turn, status, reply) = ask(&api, &pool, &scripter, "claude",
        "Write a Python script in ~/work/cdp-search that uses the CDP helper your computer provides \
         (see else-capabilities) to open http://127.0.0.1:8411/ in your visible browser, type \
         lanterns into the search box, submit, and print the results text. Run it and tell me the \
         results it printed.").await;
    assert_eq!(status, "completed", "{reply}");
    assert!(
        reply.contains("lanterns-beta"),
        "the results came from the page: {reply}"
    );
    let shown = guest(
        &home,
        &sname,
        "DISPLAY=:99 xwininfo -root -tree | grep -c 'Results for lanterns' || true",
        None,
        false,
    );
    assert_eq!(shown, "1", "the search ran in the visible browser");
    let script = guest(
        &home,
        &sname,
        "grep -rlE 'elsecdp|cdp.mjs' ~/work/cdp-search | head -1",
        None,
        false,
    );
    assert!(!script.is_empty(), "a script using the helper exists");
    note(&format!(
        "Claude tool activity: {:?}",
        turn_activity(&pool, &scripter, turn).await
    ));

    note("== 5. Hands (Codex): a randomized visual task and a terminal, by screen and input only");
    let (_, status, _) = ask(
        &api,
        &pool,
        &hands,
        "codex",
        "Run `true` with computer_run and reply OK.",
    )
    .await;
    assert_eq!(status, "completed");
    let hname = runtime_name(&pool, &hands).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let id = Uuid::new_v4();
    let b = id.as_bytes();
    let (x, y) = (300 + (b[0] as u32 * 3) % 700, 200 + (b[1] as u32 * 2) % 400);
    let word = ["ORANGE", "VIOLET", "SILVER", "CRIMSON", "INDIGO", "AMBER"][b[2] as usize % 6];
    let code = format!(
        "K{}",
        10000 + u16::from_be_bytes([b[3], b[4]]) as u32 % 90000
    );
    guest(
        &home,
        &hname,
        "mkdir -p ~/site && cat > ~/site/input_site.py",
        Some(VISUAL_SITE),
        false,
    );
    guest(
        &home,
        &hname,
        &format!(
            "cd ~/site && (/usr/bin/setsid python3 input_site.py 8421 {code} {x} {y} {word} 08:00-17:00 > site.log 2>&1 < /dev/null &); sleep 1"
        ),
        None,
        false,
    );
    guest(
        &home,
        &hname,
        "DISPLAY=:99 /usr/local/bin/else-browser open 'http://127.0.0.1:8421/' > /dev/null",
        None,
        false,
    );
    let (turn, status, reply) = ask(
        &api,
        &pool,
        &hands,
        "codex",
        "Your desktop's browser shows a page with one button. Using only computer_screenshot and \
         computer_input: click it, do what the page then asks, scroll to the very bottom of the \
         next page, and tell me the confirmation code there. No other tool.",
    )
    .await;
    assert_eq!(status, "completed", "{reply}");
    let used = turn_activity(&pool, &hands, turn).await;
    assert!(
        used.iter()
            .all(|a| a.starts_with("computer_screenshot") || a.starts_with("computer_input")),
        "{used:?}"
    );
    assert!(reply.contains(&code), "{reply} (expected {code})");
    let submitted = guest(
        &home,
        &hname,
        "curl -s --max-time 10 http://127.0.0.1:8421/log",
        None,
        false,
    );
    assert!(
        submitted.lines().any(|l| l.trim() == word),
        "the page received the requested word {word}: {submitted:?}"
    );
    let screenshot_bytes: Vec<u64> = used
        .iter()
        .filter_map(|a| a.split(" bytes").next()?.rsplit(' ').next()?.parse().ok())
        .collect();
    note(&format!(
        "visual task: {} screenshots, {:?} bytes each",
        screenshot_bytes.len(),
        screenshot_bytes
    ));
    let proof = format!("VISUAL-{}", 1000 + b[5] as u32);
    guest(
        &home,
        &hname,
        "(DISPLAY=:99 /usr/bin/setsid xterm -title terminal-task -geometry 70x12+380+260 -fa 'DejaVu Sans Mono' -fs 14 \
         -e sh -c 'cd ~/work && exec bash' > /dev/null 2>&1 < /dev/null &); sleep 2",
        None,
        false,
    );
    let (turn, status, reply) = ask(
        &api,
        &pool,
        &hands,
        "codex",
        &format!(
            "A terminal window titled terminal-task is open on your desktop. Using only \
         computer_screenshot and computer_input, click into it and type `echo {proof} > \
         ~/work/visual.txt` then Enter, and confirm with a screenshot."
        ),
    )
    .await;
    assert_eq!(status, "completed", "{reply}");
    assert!(
        turn_activity(&pool, &hands, turn)
            .await
            .iter()
            .all(|a| a.starts_with("computer_screenshot") || a.starts_with("computer_input"))
    );
    assert_eq!(
        guest(&home, &hname, "cat ~/work/visual.txt", None, false),
        proof
    );

    note("== 6. isolation and the host boundary");
    assert_eq!(
        guest(
            &home,
            &sname,
            "ls ~/work/visual.txt 2>/dev/null | wc -l",
            None,
            false
        ),
        "0",
        "Scripter cannot see Hands' files"
    );
    assert_eq!(
        guest(
            &home,
            &hname,
            "ls ~/work/inventory 2>/dev/null | wc -l",
            None,
            false
        ),
        "0",
        "nor the reverse"
    );
    for name in [&sname, &hname] {
        assert_eq!(
            guest(
                &home,
                name,
                "command -v codex claude msb docker || echo none",
                None,
                false
            ),
            "none"
        );
    }
    let (sb, hb) = (
        guest(
            &home,
            &sname,
            "curl -s http://127.0.0.1:9222/json/version | jq -r .webSocketDebuggerUrl",
            None,
            false,
        ),
        guest(
            &home,
            &hname,
            "curl -s http://127.0.0.1:9222/json/version | jq -r .webSocketDebuggerUrl",
            None,
            false,
        ),
    );
    assert_ne!(sb, hb, "separate browsers");

    note("== 7. a busy bot moves category across an API restart; nothing changes identity");
    let (_, status, reply) = ask(
        &api,
        &pool,
        &scripter,
        "codex",
        "Start a background job with exactly the command `sleep 60 && date > ~/work/long.txt` and \
         reply with its job ID. Do not wait for it.",
    )
    .await;
    assert_eq!(status, "completed", "{reply}");
    let job: (Uuid, Uuid) = sqlx::query_as(
        "SELECT id, lease_id FROM guest_jobs
         WHERE bot_id = ?1 AND command LIKE 'sleep 60%' AND status = 'running'",
    )
    .bind(scripter.parse::<Uuid>().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    // Identities before the move, while the job runs.
    let before = api.call(reqwest::Method::GET, "/v1/bots", None).await.1;
    let (status, body) = api
        .call(
            reqwest::Method::PATCH,
            &format!("/v1/bots/{scripter}/category"),
            Some(json!({ "category_id": categories[1] })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let moved = api.call(reqwest::Method::GET, "/v1/bots", None).await.1;
    api.stop();
    let mut api = install.serve().await;
    let after = api.call(reqwest::Method::GET, "/v1/bots", None).await.1;
    let project = |list: &Value| -> Vec<(String, String, String)> {
        list["bots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                (
                    b["id"].as_str().unwrap().to_owned(),
                    b["conversation_id"].as_str().unwrap_or_default().to_owned(),
                    b["category_id"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    };
    // The move changed exactly Scripter's category, to the one requested.
    let mut expected = project(&before);
    for bot in expected.iter_mut().filter(|b| b.0 == scripter) {
        assert_eq!(bot.2, categories[0], "Scripter started in Automation");
        bot.2 = categories[1].clone();
    }
    assert_eq!(project(&moved), expected, "only the category moved");
    assert_eq!(
        project(&after),
        expected,
        "bot, conversation, and category identity across the restart"
    );
    assert_eq!(before["categories"], moved["categories"]);
    assert_eq!(moved["categories"], after["categories"]);
    let (status, lease, released): (String, Option<Uuid>, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as(
            "SELECT j.status, j.lease_id, l.released_at
             FROM guest_jobs j JOIN computer_leases l ON l.id = j.lease_id WHERE j.id = ?1",
        )
        .bind(job.0)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (status.as_str(), lease, released),
        ("running", Some(job.1), None),
        "the same job and lease survived the move and the restart"
    );

    note("== 8. owner stop holds, and native sub-agents stay disabled");
    let (status, _) = api
        .call(
            reqwest::Method::POST,
            &format!("/v1/bots/{hands}/computer/stop"),
            None,
        )
        .await;
    assert!(status < 300);
    let (_, status, reply) = ask(&api, &pool, &hands, "codex",
        "Take a screenshot of your desktop with computer_screenshot and tell me what the tool said.").await;
    assert_eq!(status, "completed", "{reply}");
    assert!(reply.to_lowercase().contains("stop"), "{reply}");
    let mut list = std::process::Command::new(msb());
    list.env("MSB_HOME", &home)
        .args(["list", "--format", "json"]);
    let states: Value =
        serde_json::from_slice(&bounded(list, Duration::from_secs(60)).unwrap().stdout).unwrap();
    let hands_state = states
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == hname.as_str())
        .unwrap()["status"]
        .clone();
    assert_eq!(
        hands_state, "Stopped",
        "the owner-stopped computer was not started"
    );
    let (_, status, reply) = ask(&api, &pool, &scripter, "codex",
        "Spawn three parallel sub-agents with another model to count to three and merge their answers.").await;
    assert_eq!(status, "completed", "{reply}");

    note("== 9. browser state, then a coordinated backup (job finished, API stopped)");
    let started = Instant::now();
    loop {
        let status: String = sqlx::query_scalar("SELECT status FROM guest_jobs WHERE id = ?1")
            .bind(job.0)
            .fetch_one(&pool)
            .await
            .unwrap();
        if status != "running" {
            assert_eq!(status, "completed");
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(180));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let marker = format!("kept-{}", &Uuid::new_v4().simple().to_string()[..8]);
    guest(
        &home,
        &sname,
        &format!(
            "cd ~/site && printf '<script>localStorage.setItem(\"kept\", \"{marker}\")</script>stored' > keep.html && \
         printf '<body><script>document.body.textContent = \"KEPT=\" + localStorage.getItem(\"kept\")</script></body>' > read.html && \
         (/usr/bin/setsid python3 -m http.server 8765 --bind 127.0.0.1 > http.log 2>&1 < /dev/null &); sleep 1; \
         DISPLAY=:99 /usr/local/bin/else-browser open http://127.0.0.1:8765/keep.html > /dev/null"
        ),
        None,
        false,
    );
    let checks = "set -e; cd ~/work; ls inventory/main.py; find . -name package.json -not -path '*/node_modules/*' | head -1; \
                  node --version; python3 -m venv /tmp/v && /tmp/v/bin/python -c 'print(\"venv-ok\")'; \
                  cd /tmp && rm -rf t && cargo new -q t && cd t && cargo build -q --offline && ./target/debug/t; \
                  sha256sum ~/work/inventory/report.csv | cut -d' ' -f1";
    let tooling_before = guest(&home, &sname, checks, None, false);
    note(&format!("tooling before backup:\n{tooling_before}"));
    // Environments that must survive as they are: a venv inside the
    // project and a built Cargo project, used once, then the whole tree of
    // ~/work recorded (every file's SHA-256, plus the source file list).
    guest(
        &home,
        &sname,
        "set -e; cd ~/work && python3 -m venv inventory/.venv && cargo new -q tally && \
         cd tally && cargo build -q --offline",
        None,
        false,
    );
    let use_project = "set -e; cd ~/work/inventory; \
                       .venv/bin/python -c 'import sys; print(\"venv\", sys.prefix != sys.base_prefix)'; \
                       .venv/bin/python main.py > /dev/null; sha256sum report.csv | cut -d' ' -f1; \
                       cd ~/work/tally && cargo build -q --offline && ./target/debug/tally";
    let used_before = guest(&home, &sname, use_project, None, false);
    let manifest = "set -e; cd ~/work; \
                    find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1; \
                    find . -type f | wc -l; \
                    find . -type f -not -path '*/node_modules/*' -not -path '*/.venv/*' \
                      -not -path '*/target/*' -not -path '*/__pycache__/*' | LC_ALL=C sort";
    let manifest_before = guest(&home, &sname, manifest, None, false);
    note(&format!(
        "project use before backup:\n{used_before}\n~/work manifest:\n{manifest_before}"
    ));
    assert!(used_before.starts_with("venv True"), "{used_before}");
    let saved_categories = api.call(reqwest::Method::GET, "/v1/bots", None).await.1;
    api.stop();
    let started = Instant::now();
    let out = install.run_output(&["backup"]);
    note(&format!("backup took {:?}:\n{out}", started.elapsed()));
    let set = out
        .lines()
        .find_map(|l| l.strip_prefix("backup set: "))
        .unwrap()
        .to_owned();
    let mut archives = Vec::new();
    for entry in std::fs::read_dir(&set).unwrap() {
        let entry = entry.unwrap();
        archives.push(format!(
            "{} {} bytes",
            entry.file_name().to_string_lossy(),
            entry.metadata().unwrap().len()
        ));
    }
    note(&format!(
        "backup set {} ({} bytes): {archives:?}",
        set,
        dir_size(Path::new(&set))
    ));
    assert!(out.contains("Whole assistant"), "{out}");

    note("== 10. restore into a new installation and check what came back");
    let restored_url = format!(
        "sqlite://{}",
        params
            .with_database(&format!("{target}"))
            .options
            .get_filename()
            .display()
    );
    // The original computers stop for good (no ports or names collide).
    for name in [&sname, &hname] {
        let mut stop = std::process::Command::new(msb());
        stop.env("MSB_HOME", &home).args(["stop", name.as_str()]);
        let _ = bounded(stop, Duration::from_secs(120));
    }
    install.run(&["restore", &set, &target, restored_dir.to_str().unwrap()]);
    let restored = Install {
        data: restored_dir.clone(),
        database_url: restored_url.clone(),
        log: log.clone(),
    };
    let mut api = restored.serve().await;
    let rpool = sqlx::SqlitePool::connect(&restored_url).await.unwrap();
    let back = api.call(reqwest::Method::GET, "/v1/bots", None).await.1;
    assert_eq!(
        back["categories"], saved_categories["categories"],
        "category IDs and names"
    );
    assert_eq!(
        project(&back),
        project(&saved_categories),
        "assignments and conversations"
    );
    assert_eq!(
        back["default_bot_id"], saved_categories["default_bot_id"],
        "the main-bot pointer"
    );
    let (_, status, _) = ask(
        &api,
        &rpool,
        &scripter,
        "codex",
        "Run `true` with computer_run and reply OK.",
    )
    .await;
    assert_eq!(status, "completed");
    let rhome = restored_dir.join("microsandbox");
    tokio::time::sleep(Duration::from_secs(4)).await;
    // Nothing in ~/work has run yet: every file is exactly as backed up.
    let manifest_after = guest(&rhome, &sname, manifest, None, false);
    assert_eq!(manifest_after, manifest_before, "the whole ~/work tree");
    let used_after = guest(&rhome, &sname, use_project, None, false);
    assert_eq!(
        used_after, used_before,
        "the restored venv and Cargo build work as they are"
    );
    let tooling_after = guest(&rhome, &sname, checks, None, false);
    note(&format!("tooling after restore:\n{tooling_after}"));
    assert_eq!(
        tooling_after, tooling_before,
        "files, environments, and tooling"
    );
    guest(
        &rhome,
        &sname,
        "cd ~/site && (/usr/bin/setsid python3 -m http.server 8765 --bind 127.0.0.1 > http.log 2>&1 < /dev/null &); sleep 1",
        None,
        false,
    );
    let read = guest(
        &rhome,
        &sname,
        "DISPLAY=:99 /usr/local/bin/else-browser open http://127.0.0.1:8765/read.html",
        None,
        false,
    );
    assert!(
        read.contains(&format!("KEPT={marker}")),
        "browser profile: {read}"
    );
    // CDP and input sessions are not portable: a fresh script reconnects.
    guest(
        &rhome,
        &sname,
        "cd ~/site && (/usr/bin/setsid python3 cdp_site.py 8411 > site.log 2>&1 < /dev/null &); sleep 1",
        None,
        false,
    );
    let again = guest(
        &rhome,
        &sname,
        "python3 /usr/local/lib/elsebot/cdp/example.py http://127.0.0.1:8411/ after-restore",
        None,
        false,
    );
    assert!(again.contains("after-restore-alpha"), "{again}");

    let violations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turns WHERE error_code IN ('policy_violation', 'profile_mismatch')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(violations, 0, "no native tool or sub-agent appeared");
    let providers: Vec<(String, i64)> = sqlx::query_as(
        "SELECT provider_id, count(*) FROM turns GROUP BY provider_id ORDER BY provider_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    note(&format!("turns per provider: {providers:?}"));
    api.stop();
    cleanup.finish();
    note(&format!(
        "POC-032 milestone run passed in {:?}",
        run_started.elapsed()
    ));
}
