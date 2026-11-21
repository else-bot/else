mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

use common::live::{self, IMAGE, Install, finish, guest, messages, note, runtime_name};

const FIXTURE: &str = include_str!("fixtures/smtp_capture.py");

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
    if !live::enabled("ELSE_LIVE_SCRIPTS") {
        eprintln!("SKIPPED: set ELSE_LIVE_SCRIPTS=1 for the POC-026 live run");
        return;
    }
    let run_started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let log = live::log_to("scripts");
    let db = common::TestDb::empty().await;
    let data = dir.path().join("installation");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = data.join("microsandbox");
    std::fs::create_dir_all(&home).unwrap();
    let _cleanup = live::Computers(home.clone());
    live::load_image(&home, dir.path());
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
            "codex",
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
                &format!("/v1/task-secrets/{secret}"),
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
            "codex",
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
            "codex",
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
            "codex",
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
            "codex",
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
            "codex",
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
    let late_file_count = || {
        guest(
            &home,
            &name,
            "ls ~/work/late.txt 2>/dev/null | wc -l",
            None,
            false,
        )
    };
    common::never_polled(
        "the timed-out command finished its side effect",
        Duration::from_secs(30),
        || std::future::ready(Some(late_file_count()).filter(|count| count != "0")),
    )
    .await;

    note("== 7. Stop work ends foreground and background script work");
    let turn = api
        .send(
            &bot,
            "codex",
            "Two steps, in this order. First start a background job with exactly the command \
             `sleep 290; echo late-job`. Then run exactly `sleep 280; echo late-run` with \
             computer_run and timeout_secs 300, and wait for it.",
        )
        .await;
    const WORK_START_WAIT: Duration = Duration::from_secs(300);
    let mut last_processes = String::new();
    tokio::time::timeout(WORK_START_WAIT, async {
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
            last_processes = procs;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the work did not start: {last_processes}"));
    let stop_at = Instant::now();
    let (status, body) = api
        .call(
            reqwest::Method::POST,
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(status < 300, "{body}");
    let mut last_processes = String::new();
    tokio::time::timeout(common::SETTLE, async {
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
            last_processes = procs;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("still running: {last_processes}"));
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
    common::until("the bot to respond after stopping work", async {
        loop {
            let (status, _) = api
                .call(reqwest::Method::GET, &format!("/v1/bots/{bot}"), None)
                .await;
            if status == 200 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;

    note("== 8. a lost provider and API: the job keeps its lease, the foreground command ends");
    let turn = common::until("the API to accept the turn", async {
        loop {
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
        }
    })
    .await;
    let mut last_processes = String::new();
    tokio::time::timeout(WORK_START_WAIT, async {
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
            last_processes = procs;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the work did not start: {last_processes}"));
    let job: (Uuid, Option<Uuid>) = sqlx::query_as(
        "SELECT id, lease_id FROM guest_jobs WHERE bot_id = ?1 AND command LIKE 'sleep 45%'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
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
    let restarted = Instant::now();
    common::until("the leftover sleep 120 to exit after restart", async {
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
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    note(&format!(
        "the left-over foreground command ended {:?} after the restart",
        restarted.elapsed()
    ));
    let (status, _) = finish(&pool, turn, long).await;
    assert_ne!(
        status, "completed",
        "the interrupted turn is not reported done"
    );
    const JOB_WAIT: Duration = Duration::from_secs(120);
    let mut last_status = String::new();
    tokio::time::timeout(JOB_WAIT, async {
        loop {
            let status: String = sqlx::query_scalar("SELECT status FROM guest_jobs WHERE id = ?1")
                .bind(job.0)
                .fetch_one(&pool)
                .await
                .unwrap();
            if status == "completed" {
                break;
            }
            last_status = status;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("job {last_status}"));
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
