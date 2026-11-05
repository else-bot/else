//! POC-026 against the real application: the `else-api` binary with the
//! owner's Codex login and real Microsandbox VMs from `else-guest:2`. A
//! Codex bot writes and runs multi-file scripts in its own computer (Python,
//! Node, Rust), returns an artifact, sends a batch of ten messages to a
//! capture-only SMTP fixture inside its guest with task-secret credentials,
//! survives a lost acknowledgment without resending, reports bad
//! credentials and a missing dependency, and its foreground and background
//! work stops with Stop work; a job keeps running across an API restart
//! without being launched twice. It consumes subscription quota and boots
//! VMs, so it runs only with `ELSE_LIVE_SCRIPTS=1` (after
//! `docker build --build-arg BASE="$(python3 scripts/guest-release.py --base pinned)" --build-arg IMAGE_TAG=else-guest:2 -t else-guest:2 infra/guest`):
//!
//! ELSE_LIVE_SCRIPTS=1 cargo test -p else-api --test scripts_live -- --nocapture

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

const IMAGE: &str = "else-guest:2";
const FIXTURE: &str = include_str!("fixtures/smtp_capture.py");

fn live() -> bool {
    std::env::var("ELSE_LIVE_SCRIPTS").as_deref() == Ok("1")
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
        writeln!(file, "[scripts] {text}").unwrap();
    }
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
            .env("RUST_LOG", "info,sqlx=warn")
            .args(args);
        command
    }

    fn run(&self, args: &[&str]) {
        let out = self.command(args).output().unwrap();
        assert!(
            out.status.success(),
            "else-api {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
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
            http: reqwest::Client::new(),
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

    async fn send(&self, bot: &str, text: &str) -> Uuid {
        let (status, body) = self
            .call(
                reqwest::Method::POST,
                &format!("/v1/bots/{bot}/turns"),
                Some(json!({ "request_id": Uuid::new_v4(), "text": text, "provider_id": "codex" })),
            )
            .await;
        assert!(status < 300, "send: {status} {body}");
        body["turn"]["id"].as_str().unwrap().parse().unwrap()
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

/// Stops and removes every computer in the installation's runtime home.
struct Cleanup {
    home: PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let Ok(out) = std::process::Command::new(msb())
            .env("MSB_HOME", &self.home)
            .args(["list", "--format", "json"])
            .output()
        else {
            return;
        };
        let listed: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        for name in listed.iter().filter_map(|l| l["name"].as_str()) {
            for args in [["stop", "--force"].as_slice(), ["rm"].as_slice()] {
                let _ = std::process::Command::new(msb())
                    .env("MSB_HOME", &self.home)
                    .args(args)
                    .arg(name)
                    .output();
            }
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
    use std::io::Write;
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
    command
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().unwrap();
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

/// Captured messages per recipient (the fixture's own record).
fn captured(home: &Path, name: &str) -> (std::collections::BTreeMap<String, usize>, usize) {
    let text = guest(
        home,
        name,
        "cat /home/elsebot/.capture/messages.jsonl 2>/dev/null || true",
        None,
        false,
    );
    let mut counts = std::collections::BTreeMap::new();
    let mut auth_failures = 0;
    for line in text.lines() {
        let entry: Value = serde_json::from_str(line).unwrap();
        match entry["event"].as_str() {
            Some("message") => {
                for to in entry["to"].as_array().unwrap() {
                    *counts.entry(to.as_str().unwrap().to_owned()).or_insert(0) += 1;
                }
            }
            Some("auth_failed") => auth_failures += 1,
            _ => {}
        }
    }
    (counts, auth_failures)
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

/// The last receipt line naming each recipient in the bot's receipt file.
fn receipts(home: &Path, name: &str, project: &str) -> std::collections::BTreeMap<String, String> {
    let text = guest(
        home,
        name,
        &format!("find {project} -maxdepth 2 -iname '*receipt*' -type f -exec cat {{}} +"),
        None,
        false,
    );
    let mut last = std::collections::BTreeMap::new();
    for line in text.lines() {
        for i in 1..=10 {
            let who = format!("r{i:02}@example.test");
            if line.contains(&who) {
                last.insert(who, line.to_lowercase());
            }
        }
    }
    last
}

async fn activity(db: &sqlx::SqlitePool, bot: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT body FROM memory_entry WHERE bot_id = ?1 AND kind = 'tool_activity' ORDER BY seq",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(db)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_codex_bot_writes_runs_and_stops_scripts_in_its_computer() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_SCRIPTS=1 for the POC-026 live run");
        return;
    }
    let run_started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let evidence = root().join("target/acceptance");
    std::fs::create_dir_all(&evidence).unwrap();
    let log = evidence.join("scripts.log");
    let _ = std::fs::remove_file(&log);
    LOG.set(log.clone()).unwrap();
    let db = common::TestDb::empty().await;
    let data = dir.path().join("installation");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = data.join("microsandbox");
    std::fs::create_dir_all(&home).unwrap();
    let _cleanup = Cleanup { home: home.clone() };
    let tar = dir.path().join("guest.tar");
    assert!(
        std::process::Command::new("docker")
            .args(["save", IMAGE, "-o"])
            .arg(&tar)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new(msb())
            .env("MSB_HOME", &home)
            .args(["load", "-q", "-t", IMAGE, "-i"])
            .arg(&tar)
            .status()
            .unwrap()
            .success()
    );
    std::fs::remove_file(&tar).unwrap();
    let install = Install {
        data: data.clone(),
        database_url: db.url.expose().to_owned(),
        log: log.clone(),
    };
    install.run(&["setup"]);
    let mut api = install.serve().await;
    let pool = db.pool.clone();
    let codex = std::process::Command::new("codex")
        .arg("--version")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default();
    note(&format!("versions: {codex} | msb 0.7.13 | image {IMAGE}"));
    let (status, body) = api
        .call(
            reqwest::Method::PATCH,
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "low" })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    note(&format!("profile: {}", body["profile"]));
    let (_, body) = api
        .call(
            reqwest::Method::POST,
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Scripter" })),
        )
        .await;
    let bot = body["bot"]["id"].as_str().unwrap().to_owned();
    let long = Duration::from_secs(900);

    note("== 1. a multi-file Python project, its result as an artifact");
    let turn = api
        .send(
            &bot,
            "In your computer, create a small multi-file Python project in ~/work/inventory: \
             data.py defines a list of five (item, unit price, quantity) tuples of your choice, \
             report.py has a function that turns that list into CSV rows with a line total and \
             a final TOTAL row, and main.py imports both and writes report.csv. Run it, then \
             fetch report.csv as an artifact. Reply with the artifact ID and the grand total.",
        )
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    let name = runtime_name(&pool, &bot).await;
    let sources = guest(
        &home,
        &name,
        "cd ~/work/inventory && ls *.py | sort | tr '\\n' ' ' && grep -c import main.py",
        None,
        false,
    );
    note(&format!("saved source: {sources}"));
    assert!(sources.contains("data.py main.py report.py"), "{sources}");
    let artifact: (Uuid, String, i64) = sqlx::query_as(
        "SELECT id, guest_path, size_bytes FROM artifacts WHERE bot_id = ?1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    let stored = std::fs::read_to_string(
        data.join("artifacts")
            .join(&bot)
            .join(artifact.0.to_string()),
    )
    .unwrap();
    note(&format!(
        "artifact {} ({} bytes): {stored:?}",
        artifact.1, artifact.2
    ));
    assert!(artifact.1.ends_with("report.csv") && stored.to_uppercase().contains("TOTAL"));
    assert!(
        reply.contains(&artifact.0.to_string()),
        "the reply names the artifact"
    );

    note("== 2. ten messages through the capture fixture with task-secret credentials");
    let password = format!("pw-{}", Uuid::new_v4().simple());
    guest(
        &home,
        &name,
        "mkdir -p ~/.capture && cat > ~/.capture/smtp_capture.py && chmod 0700 ~/.capture",
        Some(FIXTURE),
        false,
    );
    guest(
        &home,
        &name,
        "umask 077 && cat > ~/.capture/password",
        Some(&password),
        false,
    );
    guest(
        &home,
        &name,
        "cd ~/.capture && (/usr/bin/setsid python3 -I smtp_capture.py --port 2525 --user mailer \
         --password-file password --capture messages.jsonl --drop-ack r07@example.test \
         > fixture.log 2>&1 < /dev/null &) ; sleep 1; python3 -c \"import socket; socket.create_connection(('127.0.0.1', 2525), 3); print('listening')\"",
        None,
        false,
    );
    for (secret, value) in [
        ("smtp", password.as_str()),
        ("smtp-old", "an-expired-password"),
    ] {
        let (status, body) = api
            .call(
                reqwest::Method::PUT,
                &format!("/v1/bots/{bot}/task-secrets/{secret}"),
                Some(json!({ "env": {
                    "SMTP_HOST": "127.0.0.1", "SMTP_PORT": "2525", "SMTP_USER": "mailer",
                    "SMTP_PASSWORD": value, "SMTP_FROM": "owner@example.test" } })),
            )
            .await;
        assert_eq!(status, 201, "{body}");
    }
    let recipients: Vec<String> = (1..=10).map(|i| format!("r{i:02}@example.test")).collect();
    let turn = api
        .send(
            &bot,
            &format!(
                "Please email this announcement to these ten people now: {}. Subject: \
                 \"Team picnic on Saturday\". Body: \"Hi! The team picnic is this Saturday at \
                 noon in Riverside Park. Bring something to share.\" Use the SMTP server and \
                 login in the task secret `smtp`. Write the sender as a Node.js script in its \
                 own project and send the whole list in one run.",
                recipients.join(", ")
            ),
        )
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    let (counts, _) = captured(&home, &name);
    note(&format!("captured after the send: {counts:?}"));
    assert_eq!(
        counts.keys().cloned().collect::<Vec<_>>(),
        recipients,
        "each recipient was delivered to"
    );
    assert!(
        counts.values().all(|c| *c == 1),
        "no duplicates: {counts:?}"
    );
    // Ten separate messages, one per recipient, with the requested
    // sender, subject, and body.
    let sent = messages(&home, &name);
    assert_eq!(sent.len(), 10, "ten captured messages");
    for (to, from, data) in &sent {
        assert_eq!(to.len(), 1, "one recipient per message: {to:?}");
        assert_eq!(from, "owner@example.test");
        assert!(data.contains("Subject: Team picnic on Saturday"), "{data}");
        assert!(data.contains("Riverside Park"), "{data}");
    }
    let lowered = reply.to_lowercase();
    assert!(
        lowered.contains("r07")
            && (lowered.contains("uncertain")
                || lowered.contains("unknown")
                || lowered.contains("unconfirmed")),
        "the lost acknowledgment is reported: {reply}"
    );
    let node_project = guest(
        &home,
        &name,
        "find ~/work -path '*/node_modules' -prune -o -type f \\( -name '*.js' -o -name '*.mjs' -o -name '*.cjs' \\) -print | xargs -r grep -l 'process.env' | head -3; \
         echo ---; find ~/work -path '*/node_modules' -prune -o -type f -print | head -40",
        None,
        false,
    );
    note(&format!("Node sender: {node_project}"));
    let project = node_project
        .lines()
        .next()
        .and_then(|l| l.rsplit_once('/'))
        .map(|(dir, _)| dir.to_owned())
        .unwrap_or_default();
    let states = receipts(&home, &name, &project);
    note(&format!("receipts: {states:?}"));
    assert_eq!(states.len(), 10, "a receipt per recipient");
    for (who, line) in &states {
        if who == "r07@example.test" {
            assert!(line.contains("uncertain"), "{line}");
        } else {
            assert!(
                line.contains("sent") && !line.contains("uncertain"),
                "{who}: {line}"
            );
        }
    }
    let (scripts, _) = node_project
        .split_once("---")
        .unwrap_or((&node_project, ""));
    assert!(
        !scripts.trim().is_empty(),
        "a Node script reads its credentials from the environment"
    );

    note("== 3. a repeated request checks receipts instead of resending");
    let turn = api
        .send(
            &bot,
            "Please make sure everyone on that list has the picnic announcement; send it to \
             anyone who still needs it.",
        )
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    let (counts, _) = captured(&home, &name);
    note(&format!("captured after the repeat: {counts:?}"));
    let read: Vec<(String,)> = sqlx::query_as(
        "SELECT body FROM memory_entry WHERE bot_id = ?1 AND kind = 'tool_activity'
           AND source->>'turn_id' = ?2 AND body LIKE '%receipt%'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .bind(turn.to_string())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(!read.is_empty(), "the repeat consulted the receipt file");
    assert_eq!(messages(&home, &name).len(), 10, "no new message");
    assert_eq!(
        receipts(&home, &name, &project),
        states,
        "receipts unchanged"
    );
    assert!(
        counts.values().all(|c| *c == 1),
        "nothing was resent: {counts:?}"
    );
    assert_eq!(counts.len(), 10);

    note("== 4. Python with rejected credentials, and a missing dependency");
    let turn = api
        .send(
            &bot,
            "Using Python's smtplib and the task secret `smtp-old`, send a one-line test email \
             to r01@example.test. Then, in a new Node project, try to install the npm package \
             `else-bot-no-such-package-7f3a`. Tell me exactly what happened in each case.",
        )
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    let (counts, auth_failures) = captured(&home, &name);
    assert!(auth_failures >= 1, "the fixture refused the login");
    assert_eq!(
        counts["r01@example.test"], 1,
        "nothing was sent with the old login"
    );
    let lowered = reply.to_lowercase();
    assert!(
        lowered.contains("auth") || lowered.contains("535") || lowered.contains("credential"),
        "{reply}"
    );
    assert!(
        lowered.contains("404") || lowered.contains("not found") || lowered.contains("not in"),
        "{reply}"
    );

    note("== 5. a compiled Rust task as a background job");
    let turn = api
        .send(
            &bot,
            "Write a Rust program (a Cargo project in ~/work/primes) that counts the primes below \
             2,000,000 and writes the count to primes.txt. Build and run it as a background job, \
             wait for it to finish, and tell me the count.",
        )
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    assert!(reply.replace([',', ' '], "").contains("148933"), "{reply}");
    let jobs: Vec<(String, String)> = sqlx::query_as(
        "SELECT status, command FROM guest_jobs WHERE bot_id = ?1 ORDER BY started_at",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    note(&format!("jobs so far: {jobs:?}"));
    assert!(
        jobs.iter()
            .any(|(s, c)| s == "completed" && c.contains("cargo")),
        "{jobs:?}"
    );
    assert_eq!(
        guest(&home, &name, "cat ~/work/primes/primes.txt", None, false).trim(),
        "148933"
    );

    note("== 6. secrets never left the environment");
    let leaks = guest(
        &home,
        &name,
        &format!(
            "grep -rl --exclude-dir=.capture -e '{password}' ~ /var/lib/elsebot 2>/dev/null | head; true"
        ),
        None,
        false,
    );
    assert_eq!(leaks, "", "no guest file holds the password");
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (
             SELECT text AS t FROM items UNION ALL SELECT body FROM memory_entry
             UNION ALL SELECT command FROM guest_jobs UNION ALL SELECT coalesce(output_tail, '') FROM guest_jobs
         ) x WHERE t LIKE '%' || ?1 || '%'",
    )
    .bind(&password)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 0, "no record holds the password");
    assert!(
        !std::fs::read_to_string(&log).unwrap().contains(&password),
        "nor the API log"
    );
    let used: Vec<String> = activity(&pool, &bot).await;
    assert!(
        used.iter().any(|a| a.contains("\"secrets\":[\"smtp\"]")),
        "the secret was named"
    );

    note("== 6b. a foreground timeout ends the command");
    let turn = api
        .send(
            &bot,
            "Run exactly `sleep 30; touch ~/work/late.txt` with computer_run and \
             timeout_secs 5, then tell me what happened.",
        )
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    assert_eq!(
        guest(&home, &name, "pgrep -c -f '^sleep 30$' || true", None, true),
        "0",
        "the timed-out command was ended"
    );
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(
        guest(
            &home,
            &name,
            "ls ~/work/late.txt 2>/dev/null | wc -l",
            None,
            false
        ),
        "0",
        "it never finished its side effect"
    );

    note("== 7. Stop work ends foreground and background script work");
    let turn = api
        .send(
            &bot,
            "Two steps, in this order. First start a background job with exactly the command \
             `sleep 290; echo late-job`. Then run exactly `sleep 280; echo late-run` with \
             computer_run and timeout_secs 300, and wait for it.",
        )
        .await;
    let started = Instant::now();
    loop {
        let procs = guest(
            &home,
            &name,
            "pgrep -fa 'sleep 2[89]0' | grep -v pgrep || true",
            None,
            true,
        );
        if procs.contains("sleep 280") && procs.contains("sleep 290") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "the work did not start: {procs}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let stop_at = Instant::now();
    let (status, body) = api
        .call(
            reqwest::Method::POST,
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(status < 300, "{body}");
    loop {
        let procs = guest(
            &home,
            &name,
            "pgrep -fa 'sleep 2[89]0' | grep -v pgrep || true",
            None,
            true,
        );
        if procs.is_empty() {
            break;
        }
        assert!(
            stop_at.elapsed() < Duration::from_secs(20),
            "still running: {procs}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    note(&format!(
        "Stop work ended both within {:?}",
        stop_at.elapsed()
    ));
    let (status, _) = finish(&pool, turn, long).await;
    assert_ne!(status, "completed");
    let cancelled: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM guest_jobs WHERE bot_id = ?1 AND command LIKE 'sleep 290%' AND status = 'cancelled'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(cancelled, 1);
    for _ in 0..200 {
        let (status, _) = api
            .call(reqwest::Method::GET, &format!("/v1/bots/{bot}"), None)
            .await;
        if status == 200 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    note("== 8. a lost provider and API: the job keeps its lease, the foreground command ends");
    let turn = loop {
        let (status, body) = api
            .call(
                reqwest::Method::POST,
                &format!("/v1/bots/{bot}/turns"),
                Some(
                    json!({ "request_id": Uuid::new_v4(), "provider_id": "codex",
                    "text": "Two steps, in this order. First start a background job with \
                    exactly the command `sleep 45 && date +%s > ~/work/restart-proof.txt`. \
                    Then run exactly `sleep 120` with computer_run and timeout_secs 300, and \
                    wait for it." }),
                ),
            )
            .await;
        if status < 300 {
            break body["turn"]["id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap();
        }
        assert_eq!(status, 409, "{body}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let started = Instant::now();
    loop {
        let procs = guest(
            &home,
            &name,
            "pgrep -fa '^sleep (45|120)$' || true",
            None,
            true,
        );
        if procs.contains("sleep 45") && procs.contains("sleep 120") {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(300),
            "the work did not start: {procs}"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let job: (Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT id, lease_id FROM guest_jobs WHERE bot_id = ?1 AND command LIKE 'sleep 45%'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    // The API (and with it the provider's connection) dies mid-turn.
    api.kill();
    let mut api = install.serve().await;
    let after: Vec<(Uuid, String, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
        "SELECT j.id, j.status, l.released_at FROM guest_jobs j
         LEFT JOIN computer_leases l ON l.id = j.lease_id
         WHERE j.bot_id = ?1 AND j.command LIKE 'sleep 45%'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    note(&format!("after the restart: {after:?}"));
    assert_eq!(after.len(), 1, "one job, not relaunched");
    assert_eq!(
        (after[0].0, after[0].1.as_str(), after[0].2),
        (job.0, "running", None)
    );
    assert_eq!(
        guest(&home, &name, "pgrep -c -f '^sleep 45$' || true", None, true),
        "1",
        "one process"
    );
    // The foreground command the dead API left is ended after the restart.
    let restarted = Instant::now();
    loop {
        let left = guest(
            &home,
            &name,
            "pgrep -c -f '^sleep 120$' || true",
            None,
            true,
        );
        if left == "0" {
            break;
        }
        assert!(
            restarted.elapsed() < Duration::from_secs(30),
            "sleep 120 still runs"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    note(&format!(
        "the left-over foreground command ended {:?} after the restart",
        restarted.elapsed()
    ));
    let (status, _) = finish(&pool, turn, long).await;
    assert_ne!(
        status, "completed",
        "the interrupted turn is not reported done"
    );
    let started = Instant::now();
    loop {
        let status: String = sqlx::query_scalar("SELECT status FROM guest_jobs WHERE id = ?1")
            .bind(job.0)
            .fetch_one(&pool)
            .await
            .unwrap();
        if status == "completed" {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(120), "job {status}");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let released: bool =
        sqlx::query_scalar("SELECT released_at IS NOT NULL FROM computer_leases WHERE id = ?1")
            .bind(job.1.unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(released, "the finished job released its lease");

    note("== 9. files survive stop and start");
    let (status, _) = api
        .call(
            reqwest::Method::POST,
            &format!("/v1/bots/{bot}/computer/stop"),
            None,
        )
        .await;
    assert!(status < 300);
    let (status, _) = api
        .call(
            reqwest::Method::POST,
            &format!("/v1/bots/{bot}/computer/resume"),
            None,
        )
        .await;
    assert!(status < 300);
    let (status, body) = api
        .call(
            reqwest::Method::GET,
            &format!("/v1/bots/{bot}/desktop"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let kept = guest(
        &home,
        &name,
        "ls ~/work/inventory/main.py ~/work/primes/Cargo.toml ~/work/restart-proof.txt | wc -l",
        None,
        false,
    );
    assert_eq!(kept, "3");
    let _ = api
        .call(
            reqwest::Method::DELETE,
            &format!("/v1/bots/{bot}/desktop"),
            None,
        )
        .await;
    note(&format!(
        "POC-026 live run passed in {:?}",
        run_started.elapsed()
    ));
    api.kill();
}
