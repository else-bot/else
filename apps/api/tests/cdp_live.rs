//! POC-027 against real Microsandbox VMs from `else-guest:2`: Node and
//! Python scripts drive the computer's visible Chromium through the guest
//! CDP helpers, and the checks cover the visible window, closed targets,
//! navigation timeouts, a browser restart with a mutating action in flight,
//! the browser lock shared with `computer_browser`, Stop work, backups
//! deferring to a running script, loopback-only DevTools, and separate
//! browsers per bot. Runs only with `ELSE_LIVE_VM=1`:
//!
//! ELSE_LIVE_VM=1 cargo test -p else-api --test cdp_live -- --nocapture

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::TestApp;
use else_api::computers::jobs::Started;
use else_api::computers::runtime::Msb;
use else_api::computers::{Acquire, ComputerConfig, ComputerManager, ComputerRuntime};
use serde_json::Value;
use uuid::Uuid;

const SITE: &str = include_str!("fixtures/cdp_site.py");

fn live() -> bool {
    std::env::var("ELSE_LIVE_VM").as_deref() == Ok("1")
}

fn msb_bin() -> String {
    format!("{}/.microsandbox/bin/msb", std::env::var("HOME").unwrap())
}

struct Cleanup {
    home: std::path::PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let Ok(out) = std::process::Command::new(msb_bin())
            .env("MSB_HOME", &self.home)
            .args(["list", "--format", "json"])
            .output()
        else {
            return;
        };
        let listed: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        for name in listed.iter().filter_map(|l| l["name"].as_str()) {
            for args in [["stop", "--force"].as_slice(), ["rm"].as_slice()] {
                let _ = std::process::Command::new(msb_bin())
                    .env("MSB_HOME", &self.home)
                    .args(args)
                    .arg(name)
                    .output();
            }
        }
    }
}

async fn run(
    manager: &ComputerManager,
    bot: Uuid,
    command: &str,
    secs: u64,
) -> (i32, String, String) {
    let out = manager
        .run_command(bot, "test:turn", command, Duration::from_secs(secs))
        .await
        .unwrap()
        .unwrap();
    (
        out.exit_code.unwrap_or(-1),
        out.stdout.trim().to_owned(),
        out.stderr.trim().to_owned(),
    )
}

async fn ok(manager: &ComputerManager, bot: Uuid, command: &str, secs: u64) -> String {
    let (code, out, err) = run(manager, bot, command, secs).await;
    assert_eq!(code, 0, "{command}\nstdout: {out}\nstderr: {err}");
    out
}

async fn write(manager: &ComputerManager, bot: Uuid, path: &str, content: &str) {
    let Acquire::Ready { lease_id, .. } = manager.acquire(bot, "test:files").await.unwrap() else {
        panic!("not ready")
    };
    manager.write_file(lease_id, path, content).await.unwrap();
    manager.release(lease_id).await.unwrap();
}

/// The window titles on the guest's X display (the one VNC serves).
async fn windows(manager: &ComputerManager, bot: Uuid) -> String {
    ok(
        manager,
        bot,
        "xwininfo -display :99 -root -tree | grep -o '\"[^\"]*Chromium[^\"]*\"' || true",
        30,
    )
    .await
}

const NODE: &str = r#"
import { connect, NavigationTimeout, TargetClosed } from "/usr/local/lib/elsebot/cdp/cdp.mjs";
const site = "http://127.0.0.1:8411";
const out = {};
const browser = await connect();
try {
  const page = await browser.page({ create: site + "/" });
  out.form_title = await page.read("document.title");
  await page.fill("input[name=q]", "node-picnic");
  await page.click("button[type=submit]");
  await page.waitFor("document.querySelector('#results')");
  out.title = await page.read("document.title");
  out.results = await page.read("document.querySelector('#results').innerText");
  out.target = page.id;
  // A navigation that never finishes is bounded.
  const slow = await browser.page({ create: site + "/search?q=other" });
  try { await slow.navigate(site + "/slow", { timeoutMs: 3000 }); out.slow = "loaded?"; }
  catch (e) { out.slow = e instanceof NavigationTimeout ? "NavigationTimeout" : String(e); }
  // A closed target is reported, not silently recreated.
  await slow.call("Page.close", {}, { mutating: false }).catch(() => {});
  await new Promise((r) => setTimeout(r, 500));
  try { await slow.read("document.title"); out.closed = "still open?"; }
  catch (e) { out.closed = e instanceof TargetClosed ? "TargetClosed" : String(e); }
  const blank = await browser.page({ create: "about:blank" });
  out.blank = await blank.read("location.href");
  await page.show();
} finally {
  await browser.close();
}
console.log(JSON.stringify(out));
"#;

const PYTHON: &str = r#"
import json, sys
sys.path.insert(0, "/usr/local/lib/elsebot/cdp")
import elsecdp
site = "http://127.0.0.1:8411"
out = {}
with elsecdp.connect() as browser:
    page = browser.page(create=site + "/")
    page.fill("input[name=q]", "python-picnic")
    page.click("button[type=submit]")
    page.wait_for("document.querySelector('#results')")
    out["title"] = page.read("document.title")
    out["results"] = page.read("document.querySelector('#results').innerText")
    slow = browser.page(create=site + "/search?q=other")
    try:
        slow.navigate(site + "/slow", timeout=3)
        out["slow"] = "loaded?"
    except elsecdp.NavigationTimeout:
        out["slow"] = "NavigationTimeout"
    slow.close()
    page.show()
print(json.dumps(out))
"#;

/// A mutating action whose acknowledgment is lost: the order is counted at
/// once and its reply withheld; the test restarts the browser after the
/// count moved. Reported as uncertain, never repeated; then the script opens
/// the page again.
const RESTART_PY: &str = r#"
import json, sys, urllib.request
sys.path.insert(0, "/usr/local/lib/elsebot/cdp")
import elsecdp
site = "http://127.0.0.1:8411"
out = {}
with elsecdp.connect() as browser:
    page = browser.page(create=site + "/order")
    open("/home/elsebot/work/restart-ready", "w").write("ready")
    try:
        out["order"] = page.evaluate("fetch('/count', {method: 'POST'}).then(r => r.text())", timeout=60)
    except elsecdp.Uncertain:
        out["order"] = "Uncertain"
    try:
        page.reconnect(timeout=30)
        out["same_page"] = "reconnected"
    except elsecdp.TargetClosed:
        out["same_page"] = "TargetClosed"
    again = browser.page(create=site + "/search?q=after-restart")
    out["after"] = again.read("document.title")
print(json.dumps(out))
"#;

const RESTART_NODE: &str = r#"
import fs from "node:fs";
import { connect, Uncertain, TargetClosed } from "/usr/local/lib/elsebot/cdp/cdp.mjs";
const site = "http://127.0.0.1:8411";
const out = {};
const browser = await connect();
try {
  const page = await browser.page({ create: site + "/order" });
  fs.writeFileSync("/home/elsebot/work/restart-ready", "ready");
  try { out.order = await page.evaluate("fetch('/count', {method: 'POST'}).then(r => r.text())", { timeoutMs: 60000 }); }
  catch (e) { out.order = e instanceof Uncertain ? "Uncertain" : String(e); }
  try { await page.reconnect(); out.same_page = "reconnected"; }
  catch (e) { out.same_page = e instanceof TargetClosed ? "TargetClosed" : String(e); }
  const again = await browser.page({ create: site + "/search?q=after-restart" });
  out.after = await again.read("document.title");
} finally {
  await browser.close();
}
console.log(JSON.stringify(out));
"#;

/// Work left inside the browser by a stopped script: a delayed order in a
/// tab it opened, and a busy loop in a tab it attached to.
const DELAYED: &str = r#"
import sys
sys.path.insert(0, "/usr/local/lib/elsebot/cdp")
import elsecdp
site = "http://127.0.0.1:8411"
with elsecdp.connect() as browser:
    page = browser.page(create=site + "/order")
    open("/home/elsebot/work/delayed-ready", "w").write("ready")
    page.evaluate("new Promise(r => setTimeout(() => { fetch('/count', {method: 'POST'}); r(1); }, 8000))", timeout=60)
"#;

const BUSY: &str = r#"
import sys
sys.path.insert(0, "/usr/local/lib/elsebot/cdp")
import elsecdp
with elsecdp.connect() as browser:
    page = browser.page(url_prefix="http://127.0.0.1:8411/search?q=keep")
    open("/home/elsebot/work/busy-ready", "w").write("ready")
    page.evaluate("(() => { for (;;) {} })()", timeout=15)
"#;

async fn count(manager: &ComputerManager, bot: Uuid) -> u32 {
    ok(manager, bot, "curl -s -m 3 http://127.0.0.1:8411/count", 30)
        .await
        .parse()
        .unwrap()
}

async fn wait_file(manager: &ComputerManager, bot: Uuid, file: &str) {
    let started = Instant::now();
    while ok(manager, bot, &format!("cat {file} 2>/dev/null || true"), 30).await != "ready" {
        assert!(started.elapsed() < Duration::from_secs(60), "{file}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

async fn wait_job(
    manager: &ComputerManager,
    bot: Uuid,
    job: Uuid,
) -> else_api::computers::jobs::JobState {
    let started = Instant::now();
    loop {
        let state = manager.job(bot, job).await.unwrap().unwrap();
        if !matches!(state.status.as_str(), "starting" | "running") {
            return state;
        }
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "{}",
            state.output_tail
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn scripts_drive_the_visible_browser_safely() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_VM=1 to boot real Microsandbox VMs");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("microsandbox");
    std::fs::create_dir_all(&home).unwrap();
    let _cleanup = Cleanup { home: home.clone() };
    let tar = dir.path().join("guest.tar");
    assert!(
        std::process::Command::new("docker")
            .args(["save", "else-guest:2", "-o"])
            .arg(&tar)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new(msb_bin())
            .env("MSB_HOME", &home)
            .args(["load", "-q", "-t", "else-guest:2", "-i"])
            .arg(&tar)
            .status()
            .unwrap()
            .success()
    );
    std::fs::remove_file(&tar).unwrap();
    let app = TestApp::installed().await;
    let msb = Arc::new(Msb::detect(dir.path(), None).unwrap());
    let manager = Arc::new(ComputerManager::new(
        app.db.pool.clone(),
        msb.clone(),
        ComputerConfig {
            image: "else-guest:2".into(),
            desktop: true,
            idle_after: Duration::from_secs(3600),
            state_dir: dir.path().to_owned(),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let bot: Uuid = app.create_bot("Browser").await.parse().unwrap();
    ok(&manager, bot, "true", 120).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    write(&manager, bot, "site/cdp_site.py", SITE).await;
    ok(
        &manager,
        bot,
        "cd site && (setsid python3 cdp_site.py 8411 > site.log 2>&1 < /dev/null &); sleep 1; curl -s -m 3 http://127.0.0.1:8411/ | grep -c Fixture",
        30,
    )
    .await;
    let caps: Value =
        serde_json::from_str(&ok(&manager, bot, "else-capabilities", 30).await).unwrap();
    eprintln!("browser capabilities: {}", caps["browser"]);
    assert_eq!(caps["browser"]["ready"], true);
    let version = ok(
        &manager,
        bot,
        "curl -s http://127.0.0.1:9222/json/version",
        30,
    )
    .await;
    eprintln!("DevTools: {version}");

    // Node: the visible browser, a new tab, a search, a bounded navigation,
    // a closed target.
    write(&manager, bot, "cdp/node.mjs", NODE).await;
    let node: Value =
        serde_json::from_str(&ok(&manager, bot, "node cdp/node.mjs", 120).await).unwrap();
    eprintln!("node: {node}");
    assert_eq!(node["form_title"], "Fixture search");
    assert_eq!(node["title"], "Results for node-picnic");
    assert!(
        node["results"]
            .as_str()
            .unwrap()
            .contains("node-picnic-beta")
    );
    assert_eq!(node["slow"], "NavigationTimeout");
    assert_eq!(node["closed"], "TargetClosed");
    assert_eq!(node["blank"], "about:blank");
    // The script's tab is the one on the display VNC serves.
    let shown = windows(&manager, bot).await;
    eprintln!("windows: {shown}");
    assert!(shown.contains("Results for node-picnic"), "{shown}");
    let targets = ok(&manager, bot, "curl -s http://127.0.0.1:9222/json/list", 30).await;
    assert!(
        targets.contains(node["target"].as_str().unwrap()),
        "the same browser's tab"
    );

    // Python, from a virtual environment (stdlib helper).
    write(&manager, bot, "cdp/py.py", PYTHON).await;
    let python: Value = serde_json::from_str(
        &ok(
            &manager,
            bot,
            "python3 -m venv cdp/.venv && cdp/.venv/bin/python cdp/py.py",
            120,
        )
        .await,
    )
    .unwrap();
    eprintln!("python: {python}");
    assert_eq!(python["title"], "Results for python-picnic");
    assert_eq!(python["slow"], "NavigationTimeout");
    assert!(
        windows(&manager, bot)
            .await
            .contains("Results for python-picnic")
    );

    // The shipped examples run as they are.
    let example = ok(
        &manager,
        bot,
        "node /usr/local/lib/elsebot/cdp/example.mjs http://127.0.0.1:8411/ example-node && \
         python3 /usr/local/lib/elsebot/cdp/example.py http://127.0.0.1:8411/ example-py",
        120,
    )
    .await;
    assert!(
        example.contains("example-node-alpha") && example.contains("example-py-alpha"),
        "{example}"
    );

    // A lost acknowledgment, for each helper: the order happened (the count
    // moved) before the browser restarted; it is reported uncertain and
    // never repeated.
    let name = ComputerManager::runtime_name(bot);
    write(&manager, bot, "cdp/restart.py", RESTART_PY).await;
    write(&manager, bot, "cdp/restart.mjs", RESTART_NODE).await;
    for (expected, command) in [(1, "python3 cdp/restart.py"), (2, "node cdp/restart.mjs")] {
        ok(&manager, bot, "rm -f restart-ready", 30).await;
        let Started::Job(job) = manager
            .start_job(bot, None, command, Duration::from_secs(300))
            .await
            .unwrap()
        else {
            panic!("not started")
        };
        wait_file(&manager, bot, "restart-ready").await;
        let started = Instant::now();
        while count(&manager, bot).await < expected {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the order never arrived"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if expected == 1 {
            // While the script holds the browser, a backup defers.
            let held = manager
                .capture_disk(bot, &dir.path().join("disk.msb"))
                .await;
            assert!(
                held.is_err(),
                "a running script defers the backup: {held:?}"
            );
        }
        let restarted = msb
            .exec(
                &name,
                "/usr/bin/supervisorctl",
                &["-c", "/etc/elsebot/supervisord.conf", "restart", "chromium"],
                Duration::from_secs(60),
            )
            .await
            .unwrap();
        assert_eq!(restarted.status, Some(0), "{}", restarted.stderr);
        let done = wait_job(&manager, bot, job).await;
        eprintln!("restart ({command}): {}", done.output_tail);
        assert_eq!(done.status, "completed", "{}", done.output_tail);
        let restart: Value =
            serde_json::from_str(done.output_tail.trim().lines().last().unwrap()).unwrap();
        assert_eq!(restart["order"], "Uncertain");
        assert_eq!(
            restart["same_page"], "TargetClosed",
            "never rebinds another tab"
        );
        assert_eq!(restart["after"], "Results for after-restart");
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(
            count(&manager, bot).await,
            expected,
            "exactly one order per run, never repeated"
        );
    }

    // Stop work reconciles the browser: a stopped script's delayed order
    // in a tab it opened never happens (the tab is closed), and its busy
    // loop in a tab it attached to is halted.
    let before = count(&manager, bot).await;
    write(&manager, bot, "cdp/delayed.py", DELAYED).await;
    let Started::Job(job) = manager
        .start_job(
            bot,
            None,
            "python3 cdp/delayed.py",
            Duration::from_secs(300),
        )
        .await
        .unwrap()
    else {
        panic!("not started")
    };
    wait_file(&manager, bot, "delayed-ready").await;
    let tabs_before = ok(&manager, bot, "curl -s http://127.0.0.1:9222/json/list", 30).await;
    assert!(tabs_before.contains("/order"));
    let cancelled = manager.cancel_job(bot, job).await.unwrap().unwrap();
    assert_eq!(cancelled.status, "cancelled");
    let tabs_after = ok(&manager, bot, "curl -s http://127.0.0.1:9222/json/list", 30).await;
    assert!(
        !tabs_after.contains("/order"),
        "the stopped script's tab was closed: {tabs_after}"
    );
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(
        count(&manager, bot).await,
        before,
        "the delayed order never ran"
    );
    // A tab the owner had open, attached to by a script stuck in a loop.
    let keep = ok(
        &manager,
        bot,
        "python3 -c 'import sys; sys.path.insert(0,\"/usr/local/lib/elsebot/cdp\"); import elsecdp\nwith elsecdp.connect() as b: b.page(create=\"http://127.0.0.1:8411/search?q=keep\"); print(\"open\")'",
        60,
    )
    .await;
    assert_eq!(keep, "open");
    write(&manager, bot, "cdp/busy.py", BUSY).await;
    let Started::Job(job) = manager
        .start_job(bot, None, "python3 cdp/busy.py", Duration::from_secs(300))
        .await
        .unwrap()
    else {
        panic!("not started")
    };
    wait_file(&manager, bot, "busy-ready").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    manager.cancel_job(bot, job).await.unwrap().unwrap();
    // The page's own browser-side limit ends the stopped script's loop
    // (a busy page cannot be attached to for an earlier halt).
    let stopped_at = Instant::now();
    let responsive = loop {
        let (_, title, _) = run(
            &manager,
            bot,
            "python3 -c 'import sys; sys.path.insert(0,\"/usr/local/lib/elsebot/cdp\"); import elsecdp\nwith elsecdp.connect() as b: print(b.page(url_prefix=\"http://127.0.0.1:8411/search?q=keep\").read(\"document.title\", timeout=5))'",
            60,
        )
        .await;
        if title == "Results for keep" || stopped_at.elapsed() > Duration::from_secs(40) {
            break title;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    eprintln!(
        "the attached tab answered {:?} after the stop",
        stopped_at.elapsed()
    );
    eprintln!(
        "reconcile log: {}",
        ok(
            &manager,
            bot,
            "cat /var/lib/elsebot/browser-reconcile.log 2>&1",
            30
        )
        .await
    );
    assert_eq!(
        responsive, "Results for keep",
        "the attached tab's loop was halted"
    );

    // One controller at a time: computer_browser waits for a script.
    let Started::Job(holder) = manager
        .start_job(
            bot,
            None,
            "python3 -c 'import sys,time; sys.path.insert(0,\"/usr/local/lib/elsebot/cdp\"); import elsecdp\nwith elsecdp.connect(): time.sleep(120)'",
            Duration::from_secs(300),
        )
        .await
        .unwrap()
    else {
        panic!("not started")
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    let Acquire::Ready { lease_id, .. } = manager.acquire(bot, "test:tool").await.unwrap() else {
        panic!("not ready")
    };
    let busy = manager.browser(lease_id, &["title"]).await.unwrap();
    assert!(
        busy.stdout.contains("BROWSER_BUSY") || busy.stderr.contains("BROWSER_BUSY"),
        "{busy:?}"
    );
    // Stop the script: the lock is released with its process.
    let cancelled = manager.cancel_job(bot, holder).await.unwrap().unwrap();
    assert_eq!(cancelled.status, "cancelled");
    let free = manager.browser(lease_id, &["title"]).await.unwrap();
    assert!(free.stdout.contains("\"title\""), "{free:?}");
    manager.release(lease_id).await.unwrap();

    // DevTools listens on guest loopback only and is not published.
    let listening = ok(
        &manager,
        bot,
        "awk 'NR > 1 && $4 == \"0A\" {print $2}' /proc/net/tcp | grep -i ':2406$' || true",
        30,
    )
    .await;
    assert_eq!(listening, "0100007F:2406", "{listening}");
    let inspect = std::process::Command::new(msb_bin())
        .env("MSB_HOME", &home)
        .args(["inspect", &name])
        .output()
        .unwrap();
    let inspect = String::from_utf8_lossy(&inspect.stdout);
    assert!(!inspect.contains("9222"), "not published: {inspect}");
    let chromium = ok(
        &manager,
        bot,
        "tr '\\0' ' ' < /proc/$(pgrep -o -x chromium)/cmdline",
        30,
    )
    .await;
    assert!(
        chromium.contains("--user-data-dir=/home/elsebot/.config/chromium"),
        "{chromium}"
    );
    assert!(chromium.contains("--remote-debugging-address=127.0.0.1"));

    // Another bot's computer has its own browser.
    let other: Uuid = app.create_bot("Other").await.parse().unwrap();
    ok(&manager, other, "true", 120).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let mine: Value = serde_json::from_str(
        &ok(
            &manager,
            bot,
            "curl -s http://127.0.0.1:9222/json/version",
            30,
        )
        .await,
    )
    .unwrap();
    let theirs: Value = serde_json::from_str(
        &ok(
            &manager,
            other,
            "curl -s http://127.0.0.1:9222/json/version",
            30,
        )
        .await,
    )
    .unwrap();
    assert_ne!(mine["webSocketDebuggerUrl"], theirs["webSocketDebuggerUrl"]);
    let their_tabs = ok(
        &manager,
        other,
        "curl -s http://127.0.0.1:9222/json/list",
        30,
    )
    .await;
    assert!(!their_tabs.contains("picnic"), "{their_tabs}");
}
