mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use uuid::Uuid;

use common::live::{self, Install, finish, guest, msb, note, runtime_name, turn_activity};

const SITE: &str = include_str!("fixtures/input_site.py");

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
    if !live::enabled("ELSE_LIVE_INPUT") {
        eprintln!("SKIPPED: set ELSE_LIVE_INPUT=1 for the POC-029 live run");
        return;
    }
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let log = live::log_to("input");
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
        "DISPLAY=:99 /usr/local/bin/else-browser open 'http://127.0.0.1:8421/' > /dev/null",
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
