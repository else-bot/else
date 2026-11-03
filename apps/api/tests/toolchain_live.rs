//! POC-025 against the real Microsandbox runtime: the guest programming
//! toolchain, project dependency installs, persistence across stop/start
//! and a disk snapshot restore, and the explicit upgrade (and rollback) of
//! an existing computer to the new image. It boots VMs and downloads small
//! packages from the public npm, PyPI, and crates.io registries, so it runs
//! only with `ELSE_LIVE_VM=1`, after building both images:
//!
//! docker build -t else-guest:1 <POC-018 revision of infra/guest>
//! docker build --build-arg IMAGE_TAG=else-guest:2 -t else-guest:2 infra/guest
//! ELSE_LIVE_VM=1 cargo test -p else-api --test toolchain_live -- --nocapture --test-threads=1

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::TestApp;
use else_api::computers::jobs::Started;
use else_api::computers::runtime::Msb;
use else_api::computers::{
    Acquire, ComputerConfig, ComputerManager, ComputerRuntime, RuntimeState, Spec,
};
use serde_json::Value;
use uuid::Uuid;

fn live() -> bool {
    std::env::var("ELSE_LIVE_VM").as_deref() == Ok("1")
}

fn msb_bin() -> String {
    format!("{}/.microsandbox/bin/msb", std::env::var("HOME").unwrap())
}

/// Load a Docker image into this test's private runtime home.
fn load(home: &std::path::Path, dir: &std::path::Path, tag: &str) {
    let tar = dir.join(format!("{}.tar", tag.replace(':', "-")));
    assert!(
        std::process::Command::new("docker")
            .args(["save", tag, "-o"])
            .arg(&tar)
            .status()
            .unwrap()
            .success(),
        "docker save {tag} (build the image first)"
    );
    assert!(
        std::process::Command::new(msb_bin())
            .env("MSB_HOME", home)
            .args(["load", "-q", "-t", tag, "-i"])
            .arg(&tar)
            .status()
            .unwrap()
            .success()
    );
    std::fs::remove_file(&tar).unwrap();
}

/// Stops and removes every computer in this test's own runtime home, even
/// when the test fails.
struct Cleanup {
    home: std::path::PathBuf,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let out = std::process::Command::new(msb_bin())
            .env("MSB_HOME", &self.home)
            .args(["list", "--format", "json"])
            .output();
        let Ok(out) = out else { return };
        let listed: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
        for sandbox in listed {
            let Some(name) = sandbox["name"].as_str() else {
                continue;
            };
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

fn config(dir: &std::path::Path, image: &str) -> ComputerConfig {
    ComputerConfig {
        image: image.into(),
        desktop: true,
        idle_after: Duration::ZERO,
        state_dir: dir.to_owned(),
        ..ComputerConfig::default()
    }
}

async fn manager(app: &TestApp, dir: &std::path::Path, image: &str) -> ComputerManager {
    let manager = ComputerManager::new(
        app.db.pool.clone(),
        Arc::new(Msb::detect(dir, None).unwrap()),
        config(dir, image),
    );
    manager.reconcile_startup().await.unwrap();
    manager
}

async fn lease(manager: &ComputerManager, bot: Uuid, holder: &str) -> Uuid {
    match manager.acquire(bot, holder).await.unwrap() {
        Acquire::Ready { lease_id, .. } => lease_id,
        other => panic!("not ready: {other:?}"),
    }
}

/// Run a command as the guest user through the computer_run path; panics
/// on a nonzero exit with its output.
async fn run(manager: &ComputerManager, bot: Uuid, command: &str, secs: u64) -> String {
    let out = manager
        .run_command(bot, "test:turn", command, Duration::from_secs(secs))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        out.exit_code,
        Some(0),
        "{command}\nstdout: {}\nstderr: {}",
        out.stdout,
        out.stderr
    );
    out.stdout.trim().to_owned()
}

/// Release every lease (the computer is idle but still running).
async fn release_all(app: &TestApp, manager: &ComputerManager, bot: Uuid) {
    let leases: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM computer_leases WHERE bot_id = $1 AND released_at IS NULL",
    )
    .bind(bot)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    for lease in leases {
        manager.release(lease).await.unwrap();
    }
}

/// Release every lease and stop the computer through the idle path.
async fn stop(app: &TestApp, manager: &ComputerManager, bot: Uuid) {
    release_all(app, manager, bot).await;
    manager
        .tick(&tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let state: String =
        sqlx::query_scalar("SELECT observed_state FROM bot_computers WHERE bot_id = $1")
            .bind(bot)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(state, "stopped");
}

async fn runtime_name(app: &TestApp, bot: Uuid) -> (String, String) {
    sqlx::query_as("SELECT runtime_name, image FROM bot_computers WHERE bot_id = $1")
        .bind(bot)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

/// Write a page that stores `value` in the browser profile and one that
/// reads it back, served later from the guest's own loopback.
async fn site(manager: &ComputerManager, bot: Uuid, value: &str) {
    let lease_id = lease(manager, bot, "test:site").await;
    let write = format!("<script>localStorage.setItem('kept', '{value}')</script>stored");
    manager
        .write_file(lease_id, "site/write.html", &write)
        .await
        .unwrap();
    let read = "<body><script>document.body.textContent = 'KEPT=' + localStorage.getItem('kept')</script></body>";
    manager
        .write_file(lease_id, "site/read.html", read)
        .await
        .unwrap();
    manager.release(lease_id).await.unwrap();
}
const SERVE: &str =
    "cd site && (setsid python3 -m http.server 8765 --bind 127.0.0.1 >/dev/null 2>&1 &); sleep 1";

/// POC-025 on a new computer: the toolchain, dependency installs for each
/// ecosystem, measurements, persistence across stop/start, and a disk
/// snapshot restore.
#[tokio::test(flavor = "multi_thread")]
async fn toolchain_projects_persist_and_restore() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_VM=1 to boot real Microsandbox VMs");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("microsandbox");
    std::fs::create_dir_all(&home).unwrap();
    let _cleanup = Cleanup { home: home.clone() };
    load(&home, dir.path(), "else-guest:2");
    let app = TestApp::installed().await;
    let manager = manager(&app, dir.path(), "else-guest:2").await;
    let bot: Uuid = app
        .create_bot("Builder", "programming")
        .await
        .parse()
        .unwrap();

    // The capability report: every runtime answers as the unprivileged
    // user, without a login shell, and matches the recorded image.
    let started = Instant::now();
    let report = run(&manager, bot, "whoami; else-capabilities", 120).await;
    eprintln!("first boot and report: {:?}", started.elapsed());
    let (user, json) = report.split_once('\n').unwrap();
    assert_eq!(user, "elsebot");
    let caps: Value = serde_json::from_str(json).unwrap();
    eprintln!("capabilities: {caps:#}");
    assert_eq!(caps["image"]["image"], "else-guest:2");
    for tool in ["node", "npm", "python", "pip", "rustc", "cargo"] {
        assert!(
            caps["runtimes"][tool]
                .as_str()
                .is_some_and(|v| !v.is_empty()),
            "{tool} missing: {caps}"
        );
    }
    let packages = &caps["image"]["packages"];
    let rustc = packages["rustc"].as_str().unwrap();
    let upstream = rustc.split('+').next().unwrap();
    assert!(
        caps["runtimes"]["rustc"]
            .as_str()
            .unwrap()
            .contains(upstream),
        "rustc {} is the recorded {rustc}",
        caps["runtimes"]["rustc"]
    );
    let node = packages["nodejs"].as_str().unwrap();
    assert!(
        node.starts_with(
            caps["runtimes"]["node"]
                .as_str()
                .unwrap()
                .trim_start_matches('v')
        )
    );
    for tool in ["git", "gcc", "make"] {
        assert!(caps["tools"][tool].as_str().is_some_and(|v| !v.is_empty()));
    }
    let plain = run(
        &manager,
        bot,
        "for t in g++ pkg-config jq curl tar xz bzip2 zip unzip; do command -v $t >/dev/null || echo missing $t; done; \
         echo PATH=$PATH; df -h / | tail -1",
        60,
    )
    .await;
    eprintln!("{plain}");
    assert!(!plain.contains("missing"), "{plain}");
    // No provider CLI, credential, or runtime socket in the guest.
    let isolated = run(
        &manager,
        bot,
        "command -v codex claude msb docker || echo none; \
         ls -d /home/elsebot/.codex /home/elsebot/.claude /root/.codex /run/docker.sock 2>/dev/null | wc -l",
        60,
    )
    .await;
    assert_eq!(isolated, "none\n0");

    // A Node script with a dependency install.
    let lease_id = lease(&manager, bot, "test:files").await;
    manager
        .write_file(
            lease_id,
            "node-app/index.js",
            "const ms = require('ms');\nconsole.log(`node ${process.version} ms(2h)=${ms('2h')}`);\n",
        )
        .await
        .unwrap();
    let node = run(
        &manager,
        bot,
        "cd node-app && npm init -y >/dev/null && npm install --no-audit --no-fund ms@2.1.3 2>&1 | tail -2 && node index.js",
        240,
    )
    .await;
    eprintln!("{node}");
    assert!(node.ends_with("ms(2h)=7200000"), "{node}");

    // A Python virtual environment with pip.
    manager
        .write_file(
            lease_id,
            "py-app/main.py",
            "import sys, six\nprint(f'python {sys.version.split()[0]} six {six.__version__} venv={sys.prefix != sys.base_prefix}')\n",
        )
        .await
        .unwrap();
    let python = run(
        &manager,
        bot,
        "cd py-app && python3 -m venv .venv && .venv/bin/pip --version && \
         .venv/bin/pip install --quiet --disable-pip-version-check six==1.16.0 && .venv/bin/python main.py",
        240,
    )
    .await;
    eprintln!("{python}");
    assert!(python.ends_with("six 1.16.0 venv=True"), "{python}");

    // A Rust program with a crates.io dependency, built as a background job
    // and measured.
    manager
        .write_file(
            lease_id,
            "rs-app/Cargo.toml",
            "[package]\nname = \"rs-app\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n\
             serde = { version = \"=1.0.219\", features = [\"derive\"] }\nserde_json = \"=1.0.140\"\n",
        )
        .await
        .unwrap();
    manager
        .write_file(
            lease_id,
            "rs-app/src/main.rs",
            "#[derive(serde::Serialize)]\nstruct Out { rust: bool, sum: u32 }\n\
             fn main() { println!(\"{}\", serde_json::to_string(&Out { rust: true, sum: (1..=10).sum() }).unwrap()); }\n",
        )
        .await
        .unwrap();
    manager.release(lease_id).await.unwrap();
    let build = "cd rs-app && /usr/bin/time -v cargo build --release > build.log 2>&1; c=$?; \
                 grep -E 'Maximum resident|Elapsed|Percent of CPU' build.log; \
                 du -sh target ~/.cargo | tr '\\t' ' '; exit $c";
    let Started::Job(job) = manager
        .start_job(bot, None, build, Duration::from_secs(1800))
        .await
        .unwrap()
    else {
        panic!("the build job did not start")
    };
    let started = Instant::now();
    let done = loop {
        let state = manager.job(bot, job).await.unwrap().unwrap();
        if !matches!(state.status.as_str(), "starting" | "running") {
            break state;
        }
        assert!(
            started.elapsed() < Duration::from_secs(1800),
            "build timed out"
        );
        tokio::time::sleep(Duration::from_secs(3)).await;
    };
    eprintln!(
        "cargo build (serde + serde_json with derive, 1 vCPU, 2 GiB) in {:?}:\n{}",
        started.elapsed(),
        done.output_tail
    );
    assert_eq!(
        (done.status.as_str(), done.exit_code),
        ("completed", Some(0)),
        "{}",
        done.output_tail
    );
    let rust = run(&manager, bot, "rs-app/target/release/rs-app", 60).await;
    assert_eq!(rust, r#"{"rust":true,"sum":55}"#);
    let usage = run(
        &manager,
        bot,
        "df -h / | tail -1; free -m | sed -n 2p; du -sh ~ | cut -f1",
        60,
    )
    .await;
    eprintln!("guest disk and memory after the builds:\n{usage}");

    // A value in the browser profile, to check it survives with the files.
    site(&manager, bot, "survives-25").await;
    run(&manager, bot, SERVE, 60).await;
    let lease_id = lease(&manager, bot, "test:browser").await;
    manager
        .browser(lease_id, &["open", "http://127.0.0.1:8765/write.html"])
        .await
        .unwrap();
    manager.release(lease_id).await.unwrap();

    // Executables the bot plants in its own PATH directories never run as
    // root: not for the computer tools' privilege drop, the desktop's init
    // and checkpoint, or a disk capture. Each shim records its user ID.
    let lease_id = lease(&manager, bot, "test:shims").await;
    for tool in [
        "runuser",
        "sh",
        "tar",
        "rm",
        "chown",
        "supervisord",
        "supervisorctl",
        "sync",
        "kill",
    ] {
        let shim = format!(
            "#!/bin/sh\nid -u > /home/elsebot/shadowed/{tool}-$$\nexec /usr/bin/{tool} \"$@\"\n"
        );
        for dir in [".cargo/bin", ".local/bin"] {
            manager
                .write_file(lease_id, &format!("/home/elsebot/{dir}/{tool}"), &shim)
                .await
                .unwrap();
        }
    }
    manager.release(lease_id).await.unwrap();
    let planted = run(
        &manager,
        bot,
        "mkdir -p ~/shadowed && chmod +x ~/.cargo/bin/* ~/.local/bin/* && command -v tar",
        60,
    )
    .await;
    assert_eq!(
        planted, "/home/elsebot/.cargo/bin/tar",
        "the bot's own PATH comes first"
    );
    // Stop and start: projects and environments still work.
    stop(&app, &manager, bot).await;
    // The offline rebuild compiles from scratch, from the Cargo registry
    // cache kept in the guest.
    let check = "node node-app/index.js && py-app/.venv/bin/python py-app/main.py && \
                 rs-app/target/release/rs-app && cd rs-app && cargo clean -q && \
                 cargo build --release --offline -q && test -x target/release/rs-app && echo REBUILT";
    let after = run(&manager, bot, check, 240).await;
    assert!(
        after.contains("ms(2h)=7200000") && after.contains("venv=True"),
        "{after}"
    );
    assert!(
        after.contains(r#"{"rust":true,"sum":55}"#) && after.ends_with("REBUILT"),
        "{after}"
    );
    let user = run(&manager, bot, "whoami; command -v cargo tar | head -1", 60).await;
    assert_eq!(
        user, "elsebot\n/usr/bin/cargo",
        "real cargo; tools resolve normally"
    );
    run(&manager, bot, SERVE, 60).await;
    let lease_id = lease(&manager, bot, "test:browser").await;
    let read = manager
        .browser(lease_id, &["open", "http://127.0.0.1:8765/read.html"])
        .await
        .unwrap();
    assert!(read.stdout.contains("KEPT=survives-25"), "{}", read.stdout);
    manager.release(lease_id).await.unwrap();

    // A disk snapshot (the backup path) restored as another computer keeps
    // the projects and a usable toolchain configuration.
    stop(&app, &manager, bot).await;
    let archive = dir.path().join("disk.msb");
    let captured = manager.capture_disk(bot, &archive).await.unwrap();
    let size = std::fs::metadata(&archive).unwrap().len();
    eprintln!(
        "disk archive with image: {:.1} MiB ({captured:?})",
        size as f64 / 1048576.0
    );
    // (The shims' own records are checked on the restored copy below.)
    let msb = Msb::detect(dir.path(), None).unwrap();
    let restored = format!("{}-restored", runtime_name(&app, bot).await.0);
    msb.restore_disk(
        &archive,
        &Spec {
            name: restored.clone(),
            image: "else-guest:2".into(),
            cpus: 1,
            memory_mib: 2048,
            network: "public".into(),
            ports: Vec::new(),
            files: Vec::new(),
        },
    )
    .await
    .unwrap();
    msb.start(&restored).await.unwrap();
    let out = msb
        .exec(
            &restored,
            "/usr/sbin/runuser",
            &[
                "-u",
                "elsebot",
                "--",
                "/bin/sh",
                "-c",
                &format!("cd ~/work && PATH=$HOME/.cargo/bin:$PATH && {check}"),
            ],
            Duration::from_secs(240),
        )
        .await
        .unwrap();
    assert_eq!(out.status, Some(0), "{} {}", out.stdout, out.stderr);
    assert!(out.stdout.contains("ms(2h)=7200000") && out.stdout.contains("venv=True"));
    assert!(out.stdout.trim().ends_with("REBUILT"), "{}", out.stdout);
    let shims = msb
        .exec(
            &restored,
            "/bin/sh",
            &[
                "-c",
                "cat /home/elsebot/shadowed/* 2>/dev/null | sort | uniq -c",
            ],
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    eprintln!("planted shims ran as: {:?}", shims.stdout.trim());
    assert!(
        !shims.stdout.split_whitespace().any(|w| w == "0"),
        "a planted executable ran as root: {}",
        shims.stdout
    );
    // The restored desktop starts, and its browser profile kept the value.
    let init = msb
        .exec(
            &restored,
            "/usr/local/bin/else-init",
            &[],
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    assert_eq!(init.status, Some(0), "{}", init.stderr);
    tokio::time::sleep(Duration::from_secs(4)).await;
    let read = msb
        .exec(
            &restored,
            "/usr/sbin/runuser",
            &[
                "-u",
                "elsebot",
                "--",
                "/bin/sh",
                "-c",
                &format!("cd ~/work && {SERVE} && DISPLAY=:99 /usr/local/bin/else-browser open http://127.0.0.1:8765/read.html"),
            ],
            Duration::from_secs(90),
        )
        .await
        .unwrap();
    assert!(
        read.stdout.contains("KEPT=survives-25"),
        "restored browser state: {} {}",
        read.stdout,
        read.stderr
    );
    msb.stop(&restored).await.unwrap();
    msb.remove(&restored).await.unwrap();
    assert_eq!(msb.state(&restored).await.unwrap(), None);
}

/// POC-025 upgrade path: an existing computer built from the POC-018 image
/// keeps its image through ordinary use while new bots get the new one; an
/// explicit upgrade moves it to the new image with its files and browser
/// profile, keeps the old computer stopped, and can be rolled back.
#[tokio::test(flavor = "multi_thread")]
async fn existing_computers_upgrade_explicitly_and_roll_back() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_VM=1 to boot real Microsandbox VMs");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("microsandbox");
    std::fs::create_dir_all(&home).unwrap();
    let _cleanup = Cleanup { home: home.clone() };
    load(&home, dir.path(), "else-guest:1");
    load(&home, dir.path(), "else-guest:2");
    let app = TestApp::installed().await;

    // A bot whose computer was created from the old image, with a project
    // and browser state.
    let old = manager(&app, dir.path(), "else-guest:1").await;
    let bot: Uuid = app.create_bot("Veteran", "existing").await.parse().unwrap();
    let before = run(
        &old,
        bot,
        "mkdir -p project && echo kept-through-upgrade > project/notes.txt && \
         command -v npm cargo rustc gcc || echo no-toolchain",
        120,
    )
    .await;
    assert_eq!(before, "no-toolchain");
    site(&old, bot, "old-profile").await;
    run(&old, bot, SERVE, 60).await;
    let lease_id = lease(&old, bot, "test:browser").await;
    old.browser(lease_id, &["open", "http://127.0.0.1:8765/write.html"])
        .await
        .unwrap();
    old.release(lease_id).await.unwrap();
    let (old_name, _) = runtime_name(&app, bot).await;
    stop(&app, &old, bot).await;
    drop(old);

    // The API now configures the new image: the existing computer keeps
    // its own through ordinary use, and a new bot gets the new image.
    let manager = manager(&app, dir.path(), "else-guest:2").await;
    assert_eq!(
        run(
            &manager,
            bot,
            "cat project/notes.txt; command -v cargo || echo no-cargo",
            120
        )
        .await,
        "kept-through-upgrade\nno-cargo"
    );
    assert_eq!(
        runtime_name(&app, bot).await,
        (old_name.clone(), "else-guest:1".into())
    );
    let newcomer: Uuid = app.create_bot("Newcomer", "new").await.parse().unwrap();
    assert!(
        run(&manager, newcomer, "cargo --version", 120)
            .await
            .starts_with("cargo 1.85")
    );
    assert_eq!(runtime_name(&app, newcomer).await.1, "else-guest:2");
    stop(&app, &manager, newcomer).await;

    // An active job refuses the upgrade; nothing changes.
    let Started::Job(job) = manager
        .start_job(bot, None, "sleep 120", Duration::from_secs(300))
        .await
        .unwrap()
    else {
        panic!("job did not start")
    };
    let refused = manager.upgrade_image(bot, "else-guest:2", None).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.to_string().contains("guest job")),
        "{refused:?}"
    );
    manager.cancel_job(bot, job).await.unwrap();
    assert!(
        manager
            .upgrade_image(bot, "else-guest:1", None)
            .await
            .is_err(),
        "same image"
    );

    // The upgrade, from a running idle computer.
    release_all(&app, &manager, bot).await;
    let started = Instant::now();
    let report = manager
        .upgrade_image(bot, "else-guest:2", None)
        .await
        .unwrap();
    eprintln!("upgrade in {:?}: {report:?}", started.elapsed());
    assert_eq!(report.from_runtime_name, old_name);
    let (new_name, image) = runtime_name(&app, bot).await;
    assert_eq!(
        (new_name.as_str(), image.as_str()),
        (report.to_runtime_name.as_str(), "else-guest:2")
    );
    assert_eq!(
        manager.runtime_state(&old_name).await.unwrap(),
        Some(RuntimeState::Stopped)
    );
    assert_eq!(
        manager.runtime_state(&new_name).await.unwrap(),
        Some(RuntimeState::Stopped)
    );
    let state: (String, String) =
        sqlx::query_as("SELECT state, data_sha256 FROM computer_upgrades WHERE id = $1")
            .bind(report.id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(state, ("completed".into(), report.data_sha256.clone()));
    assert!(
        !dir.path().join("upgrades").join(bot.to_string()).exists(),
        "staging removed"
    );

    // On the new computer: the files, their owner, the toolchain, the
    // desktop, and the browser profile.
    let after = run(
        &manager,
        bot,
        "cat project/notes.txt; stat -c %U project/notes.txt; node -e 'console.log(1+1)'; cargo --version | cut -d' ' -f1",
        120,
    )
    .await;
    assert_eq!(after, "kept-through-upgrade\nelsebot\n2\ncargo");
    tokio::time::sleep(Duration::from_secs(3)).await;
    run(&manager, bot, SERVE, 60).await;
    let lease_id = lease(&manager, bot, "test:browser").await;
    let read = manager
        .browser(lease_id, &["open", "http://127.0.0.1:8765/read.html"])
        .await
        .unwrap();
    if !read.stdout.contains("KEPT=old-profile") {
        let diag = manager
            .run_command(bot, "test:turn", "ls -la ~/.config/chromium/Default/ | head -40; ps -eo user,comm | sort -u; tail -20 /var/log/elsebot/*.log 2>&1 | tail -40", Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        panic!(
            "browser: {} {}\n{}\n{}",
            read.stdout, read.stderr, diag.stdout, diag.stderr
        );
    }
    let desktop = manager.desktop(bot).await.unwrap().unwrap();
    assert!(desktop.url.starts_with("http://127.0.0.1:"));
    manager.close_desktop(bot).await.unwrap();
    manager.release(lease_id).await.unwrap();
    run(&manager, bot, "echo after-upgrade > project/new.txt", 60).await;

    // Startup on the upgraded installation changes nothing.
    stop(&app, &manager, bot).await;
    drop(manager);
    let manager = self::manager(&app, dir.path(), "else-guest:2").await;
    assert_eq!(runtime_name(&app, bot).await.0, new_name);

    // Rollback switches back to the kept computer, its data as it was.
    let back = manager.rollback_upgrade(bot).await.unwrap();
    assert_eq!(back.to_runtime_name, old_name);
    assert_eq!(
        runtime_name(&app, bot).await,
        (old_name.clone(), "else-guest:1".into())
    );
    assert_eq!(
        run(&manager, bot, "cat project/notes.txt; ls project/new.txt 2>/dev/null || echo absent; command -v cargo || echo no-cargo", 120).await,
        "kept-through-upgrade\nabsent\nno-cargo"
    );
    assert!(
        manager.rollback_upgrade(bot).await.is_err(),
        "nothing to roll back"
    );
    assert_eq!(
        manager.runtime_state(&new_name).await.unwrap(),
        Some(RuntimeState::Stopped),
        "kept"
    );
}
