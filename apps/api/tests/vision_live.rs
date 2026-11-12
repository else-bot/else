mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

const IMAGE: &str = "else-guest:2";

fn live() -> bool {
    std::env::var("ELSE_LIVE_VISION").as_deref() == Ok("1")
}

/// The model a provider runs, from `ELSE_LIVE_VISION_<PROVIDER>_MODEL`.
fn model(provider: &str, default: &str) -> String {
    std::env::var(format!("ELSE_LIVE_VISION_{provider}_MODEL")).unwrap_or_else(|_| default.into())
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
        writeln!(file, "[vision] {text}").unwrap();
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
        let Some(base) = common::ready_url(&self.data, &child, Duration::from_secs(30)).await
        else {
            panic!("the API did not start; see {}", self.log.display());
        };
        Api {
            child: Some(child),
            base,
            token,
            http: reqwest::Client::new(),
        }
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

fn marker() -> String {
    const WORDS: &[&str] = &[
        "MAPLE", "COBALT", "PEBBLE", "LANTERN", "QUARTZ", "WILLOW", "EMBER", "HARBOR",
    ];
    let id = Uuid::new_v4();
    let b = id.as_bytes();
    format!(
        "{}-{}",
        WORDS[b[0] as usize % WORDS.len()],
        1000 + (u16::from_be_bytes([b[1], b[2]]) % 9000)
    )
}

fn draw(home: &Path, name: &str, word: &str) {
    guest(
        home,
        name,
        "pkill -f '[x]term -title note' || true",
        None,
        false,
    );
    guest(
        home,
        name,
        "umask 077 && cat > /tmp/.note",
        Some(word),
        false,
    );
    guest(
        home,
        name,
        "(DISPLAY=:99 /usr/bin/setsid xterm -title note -geometry 16x3+760+560 -fa 'DejaVu Sans Mono' -fs 28 \\
         -bg yellow -fg black -e sh -c 'w=$(cat /tmp/.note); rm -f /tmp/.note; printf \"\\\\n %s\" \"$w\"; exec sleep 1800' \\
         > /dev/null 2>&1 < /dev/null &); sleep 2; ls /tmp/.note 2>/dev/null | wc -l",
        None,
        false,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn both_providers_read_pixels_from_their_desktop() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_VISION=1 for the POC-028 live run");
        return;
    }
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let evidence = root().join("target/acceptance");
    std::fs::create_dir_all(&evidence).unwrap();
    let log = evidence.join("vision.log");
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
    let api = install.serve().await;
    let pool = db.pool.clone();
    for (provider, body) in [
        (
            "codex",
            json!({ "model": model("CODEX", "gpt-6.1-sol"), "reasoning_effort": "low", "service_tier": "default" }),
        ),
        (
            "claude",
            json!({ "model": model("CLAUDE", "claude-haiku-5-5"), "reasoning_effort": "low", "service_tier": "default" }),
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
    let (_, body) = api
        .call(
            reqwest::Method::POST,
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Seer" })),
        )
        .await;
    let bot = body["bot"]["id"].as_str().unwrap().to_owned();
    let long = Duration::from_secs(600);

    note("== 1. the computer starts; the capture matches the display VNC serves");
    let turn = api
        .send(
            &bot,
            "codex",
            "Run `else-desktop` with computer_run and reply OK.",
        )
        .await;
    let (status, _) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed");
    let name = runtime_name(&pool, &bot).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let geometry = guest(
        &home,
        &name,
        "DISPLAY=:99 xwininfo -root | grep -E 'Width|Height' | tr -s ' ' | cut -d' ' -f3 | tr '\\n' x",
        None,
        false,
    );
    note(&format!("display :99 is {geometry}"));
    assert_eq!(geometry, "1280x800x");
    let x11vnc = guest(
        &home,
        &name,
        "tr '\\0' ' ' < /proc/$(pgrep -o x11vnc)/cmdline",
        None,
        true,
    );
    assert!(x11vnc.contains("-display :99"), "{x11vnc}");

    for (provider, word) in [("codex", marker()), ("claude", marker())] {
        note(&format!(
            "== 2. {provider} reads a code word drawn only into pixels"
        ));
        draw(&home, &name, &word);
        let turn = api
            .send(
                &bot,
                provider,
                "Take a screenshot of your computer's desktop with computer_screenshot. A yellow \\
                 window shows a short code word. Reply with exactly that code word and nothing else. \\
                 Do not use any other tool.",
            )
            .await;
        let (status, reply) = finish(&pool, turn, long).await;
        assert_eq!(status, "completed", "{reply}");
        assert!(
            reply.contains(&word),
            "{provider} read {reply:?}, the word was {word}"
        );
        let used = turn_activity(&pool, &bot, turn).await;
        note(&format!("{provider} tool activity: {used:?}"));
        assert!(
            used.iter()
                .any(|a| a.starts_with("computer_screenshot") && a.contains("1280x800"))
        );
        assert!(
            used.iter().all(|a| a.starts_with("computer_screenshot")),
            "only the screenshot"
        );
        assert!(
            used.iter().all(|a| !a.contains(&word)),
            "the word reached the model only as pixels"
        );

        note(&format!(
            "== 3. {provider} resumes its session after the image"
        ));
        let turn = api
            .send(
                &bot,
                provider,
                "What was that code word? Answer from the conversation, without tools.",
            )
            .await;
        let (status, reply) = finish(&pool, turn, long).await;
        assert_eq!(status, "completed", "{reply}");
        assert!(reply.contains(&word), "{reply}");
        let sessions: Vec<(String, String)> = sqlx::query_as(
            "SELECT native_session_id, status FROM provider_sessions
             WHERE bot_id = ?1 AND provider_id = ?2 ORDER BY created_at",
        )
        .bind(bot.parse::<Uuid>().unwrap())
        .bind(provider)
        .fetch_all(&pool)
        .await
        .unwrap();
        note(&format!("{provider} sessions: {sessions:?}"));
        assert_eq!(
            sessions.len(),
            1,
            "one native session across the image and the follow-up"
        );
        assert_eq!(sessions[0].1, "active");
    }

    note("== 4. a missing display is an explicit outcome");
    guest(
        &home,
        &name,
        "supervisorctl -c /etc/elsebot/supervisord.conf stop xvfb",
        None,
        true,
    );
    let turn = api
        .send(&bot, "codex", "Take a screenshot with computer_screenshot and tell me exactly what the tool returned.")
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    let lowered = reply.to_lowercase();
    assert!(lowered.contains("display"), "{reply}");
    guest(
        &home,
        &name,
        "supervisorctl -c /etc/elsebot/supervisord.conf start xvfb openbox",
        None,
        true,
    );

    note("== 5. Stop work after a screenshot was taken; the next turn is normal");
    let turn = api
        .send(
            &bot,
            "claude",
            "Take three screenshots, one at a time, and describe each in detail.",
        )
        .await;
    let started_turn = Instant::now();
    while turn_activity(&pool, &bot, turn).await.is_empty() {
        assert!(
            started_turn.elapsed() < Duration::from_secs(120),
            "no screenshot was taken"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (status, body) = api
        .call(
            reqwest::Method::POST,
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(status < 300, "{body}");
    let (status, _) = finish(&pool, turn, long).await;
    note(&format!(
        "stopped turn: {status}, screenshots taken: {}",
        turn_activity(&pool, &bot, turn).await.len()
    ));
    assert_eq!(status, "interrupted");
    assert!(
        turn_activity(&pool, &bot, turn).await.len() < 3,
        "stopped before finishing"
    );
    let turn = loop {
        let (status, body) = api
            .call(
                reqwest::Method::POST,
                &format!("/v1/bots/{bot}/turns"),
                Some(
                    json!({ "request_id": Uuid::new_v4(), "provider_id": "claude",
                               "text": "Reply with the single word READY." }),
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
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    note("== 6. the API dies during a screenshot turn (the provider loses its tools)");
    let mut api = api;
    let turn = api
        .send(
            &bot,
            "codex",
            "Take a screenshot with computer_screenshot, then describe the desktop.",
        )
        .await;
    let started_turn = Instant::now();
    loop {
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&pool)
            .await
            .unwrap();
        if status == "running" {
            break;
        }
        assert!(started_turn.elapsed() < Duration::from_secs(60));
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    api.kill();
    let api = install.serve().await;
    let (status, _) = finish(&pool, turn, long).await;
    note(&format!("turn cut off by the API's death: {status}"));
    assert_ne!(status, "completed");
    let turn = api
        .send(&bot, "codex", "Reply with the single word AGAIN.")
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");

    let violations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turns WHERE bot_id = ?1 AND error_code IN ('policy_violation', 'profile_mismatch')",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(violations, 0, "no native tools or sub-agents appeared");
    let images: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT text AS t FROM items UNION ALL SELECT body FROM memory_entry) x
         WHERE t LIKE '%iVBORw0KGgo%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(images, 0, "no image data in the conversation or memory");
    assert!(
        !std::fs::read_to_string(&log)
            .unwrap()
            .contains("iVBORw0KGgo"),
        "nor the API log"
    );
    let holders = std::process::Command::new("grep")
        .args(["-rl", "--exclude-dir=microsandbox", "iVBORw0KGgo"])
        .arg(&data)
        .output()
        .unwrap();
    let holders = String::from_utf8_lossy(&holders.stdout)
        .trim()
        .replace(&data.display().to_string(), "$DATA");
    note(&format!("files holding image data: {holders}"));
    for path in holders.lines() {
        assert!(
            path.starts_with("$DATA/bots/") || path.starts_with("$DATA/artifacts/"),
            "{path}"
        );
    }
    note(&format!(
        "POC-028 live run passed in {:?}",
        started.elapsed()
    ));
}
