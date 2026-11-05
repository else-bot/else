mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

const IMAGE: &str = "else-guest:2";
const SITE: &str = include_str!("fixtures/input_site.py");

fn live() -> bool {
    std::env::var("ELSE_LIVE_INPUT").as_deref() == Ok("1")
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
        writeln!(file, "[input] {text}").unwrap();
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

fn input(home: &Path, name: &str, batch: Value) -> String {
    guest(
        home,
        name,
        "IFS= read -r b; ELSE_INPUT_BATCH=\"$b\" DISPLAY=:99 /usr/local/bin/else-input || true",
        Some(&format!("{batch}\n")),
        false,
    )
}

const HELD: &str = r#"python3 - <<'PY'
import ctypes
x = ctypes.cdll.LoadLibrary("libX11.so.6")
x.XOpenDisplay.restype = ctypes.c_void_p
d = x.XOpenDisplay(b":99")
keys = (ctypes.c_char * 32)()
x.XQueryKeymap.argtypes = [ctypes.c_void_p, ctypes.c_char_p]
x.XQueryKeymap(d, keys)
x.XDefaultRootWindow.restype = ctypes.c_ulong
x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
r = x.XDefaultRootWindow(d)
u = [ctypes.c_ulong(), ctypes.c_ulong()]; i = [ctypes.c_int() for _ in range(4)]; m = ctypes.c_uint()
x.XQueryPointer.argtypes = [ctypes.c_void_p, ctypes.c_ulong] + [ctypes.c_void_p] * 7
x.XQueryPointer(d, r, *[ctypes.byref(v) for v in u + i], ctypes.byref(m))
held = [n for n in range(256) if keys.raw[n // 8] >> (n % 8) & 1]
print(f"keys={held} buttons={m.value & 0x1f00}")
PY"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_completes_visual_tasks_with_pointer_and_keyboard() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_INPUT=1 for the POC-029 live run");
        return;
    }
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let evidence = root().join("target/acceptance");
    std::fs::create_dir_all(&evidence).unwrap();
    let log = evidence.join("input.log");
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
    let (status, body) = api
        .call(
            reqwest::Method::PATCH,
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "low" })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let (_, body) = api
        .call(
            reqwest::Method::POST,
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Hands" })),
        )
        .await;
    let bot = body["bot"]["id"].as_str().unwrap().to_owned();
    let long = Duration::from_secs(900);
    let turn = api
        .send(&bot, "codex", "Run `true` with computer_run and reply OK.")
        .await;
    assert_eq!(finish(&pool, turn, long).await.0, "completed");
    let name = runtime_name(&pool, &bot).await;
    tokio::time::sleep(Duration::from_secs(4)).await;

    let id = Uuid::new_v4();
    let b = id.as_bytes();
    let (x, y) = (300 + (b[0] as u32 * 3) % 700, 200 + (b[1] as u32 * 2) % 400);
    const WORDS: &[&str] = &["ORANGE", "VIOLET", "SILVER", "CRIMSON", "INDIGO", "AMBER"];
    let word = WORDS[b[2] as usize % WORDS.len()];
    let code = format!(
        "K{}",
        10000 + u16::from_be_bytes([b[3], b[4]]) as u32 % 90000
    );
    let hours = format!("{:02}:30-{:02}:00", 6 + b[6] % 4, 18 + b[7] % 5);
    guest(
        &home,
        &name,
        "mkdir -p ~/site && cat > ~/site/input_site.py",
        Some(SITE),
        false,
    );
    guest(
        &home,
        &name,
        &format!(
            "cd ~/site && (/usr/bin/setsid python3 input_site.py 8421 {code} {x} {y} {word} {hours} > site.log 2>&1 < /dev/null &); \
             (/usr/bin/setsid python3 input_site.py 8420 TEST 40 120 TEST 00:00-00:00 > test.log 2>&1 < /dev/null &); sleep 1"
        ),
        None,
        false,
    );
    note(&format!("layout: button at {x},{y}"));

    note("== 1. the helper itself: a known click lands; a stopped batch leaves nothing pressed");
    guest(
        &home,
        &name,
        "DISPLAY=:99 /usr/local/bin/else-browser open 'http://127.0.0.1:8420/' > /dev/null",
        None,
        false,
    );
    let viewport = guest(
        &home,
        &name,
        "DISPLAY=:99 /usr/local/bin/else-browser text > /dev/null; curl -s http://127.0.0.1:9222/json/list | head -c 0; DISPLAY=:99 xwininfo -root -tree | grep -c 'Visual task'",
        None,
        false,
    );
    assert_eq!(viewport, "1", "the page is the visible window");
    let page = |expr: &str| {
        format!(
            "python3 -c 'import sys; sys.path.insert(0, \"/usr/local/lib/elsebot/cdp\"); import elsecdp\nwith elsecdp.connect() as b: print(b.page(url_prefix=\"http://127.0.0.1:8420/\").read(\"{expr}\"))'"
        )
    };
    let at: Vec<f64> = serde_json::from_str(&guest(
        &home,
        &name,
        &page("JSON.stringify((r => [window.screenX + r.left + r.width / 2, window.screenY + window.outerHeight - window.innerHeight + r.top + r.height / 2])(document.getElementById(`go`).getBoundingClientRect()))"),
        None,
        false,
    ))
    .unwrap();
    let clicked = input(
        &home,
        &name,
        json!({ "frame": [1280, 800], "actions": [{ "click": [at[0] as i64, at[1] as i64] }] }),
    );
    assert!(clicked.contains("\"ok\": true"), "{clicked}");
    let shown = guest(
        &home,
        &name,
        &page("getComputedStyle(document.getElementById(`ask`)).display"),
        None,
        false,
    );
    assert_eq!(shown, "block", "the click revealed the box");
    guest(
        &home,
        &name,
        "DISPLAY=:99 /usr/local/bin/else-browser open 'http://127.0.0.1:8420/done?word=probe' > /dev/null",
        None,
        false,
    );
    let scrolled = input(
        &home,
        &name,
        json!({ "frame": [1280, 800], "actions": [{ "scroll": [640, 500], "dy": 10 }] }),
    );
    assert!(scrolled.contains("\"ok\": true"), "{scrolled}");
    let scroll_y: f64 = guest(&home, &name, &page("window.scrollY"), None, false)
        .parse()
        .unwrap();
    note(&format!("the wheel scrolled the page to {scroll_y}"));
    assert!(scroll_y > 0.0, "the wheel scrolled");
    let refused = input(
        &home,
        &name,
        json!({ "frame": [1024, 768], "actions": [{ "click": [10, 10] }] }),
    );
    assert!(refused.contains("SCREEN_CHANGED"), "{refused}");
    let outside = input(
        &home,
        &name,
        json!({ "frame": [1280, 800], "actions": [{ "click": [5000, 10] }] }),
    );
    assert!(outside.contains("OUT_OF_BOUNDS"), "{outside}");
    let mut stopped = std::process::Command::new(msb())
        .env("MSB_HOME", &home)
        .args([
            "exec",
            &name,
            "--",
            "/usr/sbin/runuser",
            "-u",
            "elsebot",
            "--",
            "/bin/sh",
            "-c",
        ])
        .arg("IFS= read -r b; ELSE_INPUT_BATCH=\"$b\" DISPLAY=:99 exec /usr/local/bin/else-input")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let batch =
            json!({ "frame": [1280, 800], "actions": [{ "type": "AbCdEfGhIj".repeat(150) }] });
        stopped
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{batch}\n").as_bytes())
            .unwrap();
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    guest(
        &home,
        &name,
        "pkill -TERM -f '[/]usr/local/bin/else-input'; sleep 1",
        None,
        true,
    );
    let _ = stopped.wait();
    let held = guest(&home, &name, HELD, None, false);
    note(&format!("after a stopped typing batch: {held}"));
    assert_eq!(held, "keys=[] buttons=0", "nothing stays pressed");

    note("== 2. a visual web task: random button, typed word, scroll, read the code");
    guest(
        &home,
        &name,
        &format!(
            "DISPLAY=:99 /usr/local/bin/else-browser open 'http://127.0.0.1:8421/' > /dev/null"
        ),
        None,
        false,
    );
    let turn = api
        .send(
            &bot,
            "codex",
            "Your desktop's browser shows a page with one button. Using only computer_screenshot \
             and computer_input: click that button, do what the page then asks (type into the box \
             and press Enter), then scroll to the very bottom of the next page and tell me the \
             confirmation code shown there. Do not use computer_browser, computer_run, or any \
             other tool.",
        )
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    let used = turn_activity(&pool, &bot, turn).await;
    note(&format!("tool activity: {used:#?}"));
    assert!(
        used.iter()
            .all(|a| a.starts_with("computer_screenshot") || a.starts_with("computer_input")),
        "screen and input only"
    );
    assert!(used.iter().any(|a| a.contains("click(")), "it clicked");
    assert!(used.iter().any(|a| a.contains("type(")), "it typed");
    assert!(
        used.iter()
            .any(|a| a.contains("scroll(") || a.contains("key(")),
        "it scrolled"
    );
    let submitted = guest(
        &home,
        &name,
        "curl -s http://127.0.0.1:8421/log",
        None,
        false,
    );
    assert_eq!(
        submitted.lines().last(),
        Some(word),
        "the page received the word: {submitted}"
    );
    assert!(
        reply.contains(&code),
        "the code read from the final screenshot: {reply}"
    );

    note("== 2b. an information task on a website, by looking and typing only");
    guest(
        &home,
        &name,
        "DISPLAY=:99 /usr/local/bin/else-browser open 'http://127.0.0.1:8421/search' > /dev/null",
        None,
        false,
    );
    let turn = api
        .send(
            &bot,
            "codex",
            "The browser on your desktop shows a library search page. Using only \
             computer_screenshot and computer_input, search it for Riverside and tell me the \
             opening hours it lists for Riverside Library. Do not use any other tool.",
        )
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    let used = turn_activity(&pool, &bot, turn).await;
    assert!(
        used.iter()
            .all(|a| a.starts_with("computer_screenshot") || a.starts_with("computer_input")),
        "{used:?}"
    );
    let submitted = guest(
        &home,
        &name,
        "curl -s http://127.0.0.1:8421/log",
        None,
        false,
    );
    assert!(
        submitted
            .lines()
            .any(|l| l.to_lowercase().starts_with("query:")
                && l.to_lowercase().contains("riverside")),
        "{submitted}"
    );
    assert!(
        reply
            .replace(['\u{2013}', '\u{2014}', '\u{2012}'], "-")
            .contains(&hours),
        "the hours read from the results: {reply} (expected {hours})"
    );

    note("== 2c. a forced kill mid-typing: every guest kill releases what is held");
    let mut killed = std::process::Command::new(msb())
        .env("MSB_HOME", &home)
        .args([
            "exec",
            &name,
            "--",
            "/usr/sbin/runuser",
            "-u",
            "elsebot",
            "--",
            "/bin/sh",
            "-c",
        ])
        .arg("IFS= read -r b; ELSE_INPUT_BATCH=\"$b\" DISPLAY=:99 exec /usr/local/bin/else-input")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let batch = json!({ "frame": [1280, 800], "actions": [{ "drag": [600, 500, 900, 600] }, { "type": "XyXyXyXyXy".repeat(150) }] });
        killed
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{batch}\n").as_bytes())
            .unwrap();
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    guest(
        &home,
        &name,
        "pkill -KILL -f '[/]usr/local/bin/else-input'; sleep 0.5",
        None,
        true,
    );
    let _ = killed.wait();
    note(&format!(
        "right after SIGKILL: {}",
        guest(&home, &name, HELD, None, false)
    ));
    let released = guest(
        &home,
        &name,
        "DISPLAY=:99 /usr/local/bin/else-input --release-all",
        None,
        false,
    );
    assert!(released.contains("released"), "{released}");
    assert_eq!(
        guest(&home, &name, HELD, None, false),
        "keys=[] buttons=0",
        "released after a forced kill"
    );

    note("== 3. an application outside the browser: a terminal window");
    let proof = format!("VISUAL-{}", 1000 + b[5] as u32);
    guest(
        &home,
        &name,
        "(DISPLAY=:99 /usr/bin/setsid xterm -title terminal-task -geometry 70x12+380+260 -fa 'DejaVu Sans Mono' -fs 14 \
         -e sh -c 'cd ~/work && exec bash' > /dev/null 2>&1 < /dev/null &); sleep 2",
        None,
        false,
    );
    let turn = api
        .send(
            &bot,
            "codex",
            &format!(
                "A terminal window titled terminal-task is open on your desktop. Using only \
                 computer_screenshot and computer_input, click into it and type the command \
                 `echo {proof} > ~/work/visual.txt` followed by Enter. Then take a screenshot and \
                 tell me what the terminal shows."
            ),
        )
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    let used = turn_activity(&pool, &bot, turn).await;
    note(&format!("tool activity: {used:#?}"));
    assert!(
        used.iter()
            .all(|a| a.starts_with("computer_screenshot") || a.starts_with("computer_input"))
    );
    assert_eq!(
        guest(&home, &name, "cat ~/work/visual.txt", None, false),
        proof
    );
    assert!(
        used.iter().all(|a| !a.contains(&proof)),
        "typed text stays out of memory"
    );
    let held = guest(&home, &name, HELD, None, false);
    assert_eq!(held, "keys=[] buttons=0");
    note(&format!(
        "POC-029 live run passed in {:?}",
        started.elapsed()
    ));
}
