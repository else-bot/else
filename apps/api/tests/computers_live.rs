//! POC-017 against the real Microsandbox runtime (`msb` v0.7.13, KVM). It
//! boots small VMs, so it runs only with `ELSE_LIVE_VM=1`:
//!
//! ELSE_LIVE_VM=1 cargo test -p else-api --test computers_live -- --nocapture
//!
//! It uses a private runtime home in a temporary directory and removes its
//! own computers at the end; the owner's ~/.microsandbox is not touched.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::TestApp;
use else_api::computers::runtime::Msb;
use else_api::computers::{
    Acquire, ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, IdleAction,
    RuntimeError, RuntimeState, Spec,
};
use uuid::Uuid;

fn live() -> bool {
    std::env::var("ELSE_LIVE_VM").as_deref() == Ok("1")
}

async fn ready(manager: &ComputerManager, bot: Uuid, holder: &str) -> (Uuid, String) {
    match manager.acquire(bot, holder).await.unwrap() {
        Acquire::Ready {
            lease_id,
            runtime_name,
            ..
        } => (lease_id, runtime_name),
        other => panic!("not ready: {other:?}"),
    }
}

async fn sh(manager: &ComputerManager, lease: Uuid, script: &str) -> String {
    let out = manager
        .exec(lease, "sh", &["-c", script], Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(out.status, Some(0), "{}", out.stderr);
    out.stdout.trim().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn two_named_computers_keep_their_files_across_stop_pause_and_restart() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_VM=1 to boot real Microsandbox VMs");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let app = TestApp::installed().await;
    let config = ComputerConfig {
        idle_after: Duration::ZERO,
        max_cpus: 2,
        max_memory_mib: 2 * (2048 + 256),
        ..ComputerConfig::default()
    };
    let msb = || Arc::new(Msb::detect(dir.path(), None).expect("msb installed"));
    let manager = ComputerManager::new(app.db.pool.clone(), msb(), config.clone());
    manager.reconcile_startup().await.unwrap();
    let a: Uuid = app.create_bot("Alpha").await.parse().unwrap();
    let b: Uuid = app.create_bot("Beta").await.parse().unwrap();

    let started = std::time::Instant::now();
    let (lease_a, name_a) = ready(&manager, a, "test:a").await;
    eprintln!(
        "first create (with image pull) took {:?}",
        started.elapsed()
    );
    let (lease_b, name_b) = ready(&manager, b, "test:b").await;
    sh(&manager, lease_a, "echo ALPHA-FILE > /root/marker").await;
    sh(&manager, lease_b, "echo BETA-FILE > /root/marker").await;
    assert_eq!(
        sh(&manager, lease_a, "cat /root/marker").await,
        "ALPHA-FILE"
    );
    assert_eq!(sh(&manager, lease_b, "cat /root/marker").await, "BETA-FILE");
    assert_eq!(sh(&manager, lease_a, "nproc").await, "1");

    // Idle: both stop once released; their workers are gone.
    manager.release(lease_a).await.unwrap();
    manager.release(lease_b).await.unwrap();
    manager
        .tick(&tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let states = manager.runtime_list().await.unwrap();
    for name in [&name_a, &name_b] {
        assert!(
            states.contains(&(name.clone(), RuntimeState::Stopped)),
            "{states:?}"
        );
    }

    // An API restart only inspects: nothing wakes.
    let restarted = ComputerManager::new(app.db.pool.clone(), msb(), config.clone());
    restarted.reconcile_startup().await.unwrap();
    let states = restarted.runtime_list().await.unwrap();
    assert!(
        states.iter().all(|(_, s)| *s == RuntimeState::Stopped),
        "{states:?}"
    );

    // Reuse after restart: the same computer with its file.
    let (lease_a, _) = ready(&restarted, a, "test:a2").await;
    assert_eq!(
        sh(&restarted, lease_a, "cat /root/marker").await,
        "ALPHA-FILE"
    );
    restarted.release(lease_a).await.unwrap();

    // Owner stop: no implicit wake.
    restarted.owner_stop(a).await.unwrap();
    assert_eq!(
        restarted.acquire(a, "test:a3").await.unwrap(),
        Acquire::OwnerStopped
    );
    assert_eq!(
        restarted.runtime_state(&name_a).await.unwrap(),
        Some(RuntimeState::Stopped)
    );

    // Idle pause keeps the process; resume continues with the file.
    let pausing = ComputerManager::new(
        app.db.pool.clone(),
        msb(),
        ComputerConfig {
            idle_action: IdleAction::Pause,
            ..config
        },
    );
    pausing.reconcile_startup().await.unwrap();
    let (lease_b, _) = ready(&pausing, b, "test:b2").await;
    sh(
        &pausing,
        lease_b,
        "sleep 600 >/dev/null 2>&1 & echo $! > /root/pid",
    )
    .await;
    let pid = sh(&pausing, lease_b, "cat /root/pid").await;
    pausing.release(lease_b).await.unwrap();
    pausing
        .tick(&tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        pausing.runtime_state(&name_b).await.unwrap(),
        Some(RuntimeState::Paused)
    );
    let (lease_b, _) = ready(&pausing, b, "test:b3").await;
    assert_eq!(
        sh(&pausing, lease_b, &format!("kill -0 {pid} && echo ALIVE")).await,
        "ALIVE"
    );
    assert_eq!(sh(&pausing, lease_b, "cat /root/marker").await, "BETA-FILE");
    pausing.release(lease_b).await.unwrap();

    // Cleanup of this test's own runtime home only.
    for name in [&name_a, &name_b] {
        let _ = std::process::Command::new(
            std::env::var("HOME")
                .map(|h| format!("{h}/.microsandbox/bin/msb"))
                .unwrap(),
        )
        .env("MSB_HOME", dir.path().join("microsandbox"))
        .args(["stop", name])
        .output();
        let _ = std::process::Command::new(
            std::env::var("HOME")
                .map(|h| format!("{h}/.microsandbox/bin/msb"))
                .unwrap(),
        )
        .env("MSB_HOME", dir.path().join("microsandbox"))
        .args(["rm", name])
        .output();
    }
}

/// POC-018: the desktop image, guest tools, jobs, artifacts, the browser,
/// desktop access, isolation, and persistence across stop and start. Needs
/// the image built: `docker build --build-arg BASE="$(python3 scripts/guest-release.py --base pinned)" -t else-guest:2 infra/guest`.
#[tokio::test(flavor = "multi_thread")]
async fn guest_tools_desktop_and_isolation() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_VM=1 to boot real Microsandbox VMs");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let msb_bin = format!("{}/.microsandbox/bin/msb", std::env::var("HOME").unwrap());
    let home = dir.path().join("microsandbox");
    std::fs::create_dir_all(&home).unwrap();
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
        std::process::Command::new(&msb_bin)
            .env("MSB_HOME", &home)
            .args(["load", "-q", "-t", "else-guest:2", "-i"])
            .arg(&tar)
            .status()
            .unwrap()
            .success()
    );
    let app = TestApp::installed().await;
    let _cleanup = Cleanup {
        msb: msb_bin.clone(),
        home: home.clone(),
        names: std::sync::Mutex::default(),
    };
    let config = ComputerConfig {
        image: "else-guest:2".into(),
        desktop: true,
        idle_after: Duration::ZERO,
        state_dir: dir.path().to_owned(),
        ..ComputerConfig::default()
    };
    let manager = ComputerManager::new(
        app.db.pool.clone(),
        Arc::new(Msb::detect(dir.path(), None).unwrap()),
        config,
    );
    manager.reconcile_startup().await.unwrap();
    let bot: Uuid = app.create_bot("Desk").await.parse().unwrap();
    _cleanup
        .names
        .lock()
        .unwrap()
        .push(ComputerManager::runtime_name(bot));
    let run = |command: &'static str| {
        manager.run_command(bot, "test:turn", command, Duration::from_secs(60))
    };

    // Commands run as the unprivileged guest user in its work directory.
    let out = run("whoami; pwd; echo GUEST-OK > made.txt; cat made.txt")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        out.stdout.trim(),
        "elsebot\n/home/elsebot/work\nGUEST-OK",
        "{}",
        out.stderr
    );
    // Services came up (supervised, after the boot).
    tokio::time::sleep(Duration::from_secs(4)).await;
    let services = run("ps -eo comm | sort -u | tr '\\n' ' '")
        .await
        .unwrap()
        .unwrap()
        .stdout;
    for service in ["Xvfb", "x11vnc", "websockify", "chromium", "openbox"] {
        assert!(services.contains(service), "{service} missing: {services}");
    }

    // Isolation: no host credentials or runtime tools; metadata, private,
    // and host networks are denied; the public internet works.
    let checks = run(
        "ls /root/.codex /home/elsebot/.codex /run/secrets 2>&1 | grep -cE 'No such|Permission denied'; \
         command -v msb codex claude docker || echo none; \
         for u in http://169.254.169.254/ http://10.0.0.1/ http://172.16.0.1/ http://192.168.1.1/; do \
           curl -s -m 4 \"$u\" | grep -q network_policy_denied && echo denied || echo REACHED; done; \
         curl -s -o /dev/null -m 8 -w '%{http_code}' https://example.com",
    )
    .await
    .unwrap()
    .unwrap();
    eprintln!("isolation checks:\n{}", checks.stdout);
    let lines: Vec<&str> = checks.stdout.lines().collect();
    assert_eq!(lines[0], "3", "no credential directory is readable");
    assert_eq!(lines[1], "none", "no runtime or provider tools");
    assert!(lines[2..6].iter().all(|l| *l == "denied"), "{lines:?}");
    // A host canary listening on every interface sees nothing.
    let canary = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    canary.set_nonblocking(true).unwrap();
    let port = canary.local_addr().unwrap().port();
    let route = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    route.connect("1.1.1.1:80").unwrap();
    let host_ip = route.local_addr().unwrap().ip();
    assert!(
        !host_ip.is_unspecified(),
        "the host canary needs a routed address"
    );
    let probe = format!("curl -s -m 4 http://{host_ip}:{port}/ || echo unreachable");
    let out = manager
        .run_command(bot, "test:turn", &probe, Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    assert!(
        canary.accept().is_err(),
        "the host canary was reached: {}",
        out.stdout
    );

    // A background job outlives the turn's lease, then releases the computer.
    let started = manager
        .start_job(
            bot,
            None,
            "sleep 4; echo JOB-DONE > job.txt",
            Duration::from_secs(120),
        )
        .await
        .unwrap();
    let else_api::computers::jobs::Started::Job(job) = started else {
        panic!("{started:?}")
    };
    let turn_lease: Uuid = sqlx::query_scalar(
        "SELECT id FROM computer_leases WHERE bot_id = ?1 AND holder = 'test:turn' AND released_at IS NULL",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    manager.release(turn_lease).await.unwrap();
    let never = tokio_util::sync::CancellationToken::new();
    manager.tick(&never).await.unwrap();
    assert_eq!(
        manager
            .runtime_state(&ComputerManager::runtime_name(bot))
            .await
            .unwrap(),
        Some(RuntimeState::Running),
        "the job keeps the computer"
    );
    let mut done = None;
    for _ in 0..30 {
        let state = manager.job(bot, job).await.unwrap().unwrap();
        if state.status != "running" {
            done = Some(state);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let done = done.expect("the job finished");
    assert_eq!(
        (done.status.as_str(), done.exit_code),
        ("completed", Some(0))
    );

    // Artifact transfer to the host store.
    let (lease, _) = ready(&manager, bot, "test:turn2").await;
    let (artifact, size, sha, preview) = manager
        .fetch_artifact(lease, bot, None, "/home/elsebot/work/job.txt", 1 << 20)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((size, preview.trim()), (9, "JOB-DONE"));
    let stored = std::fs::read(
        dir.path()
            .join("artifacts")
            .join(bot.to_string())
            .join(artifact.to_string()),
    )
    .unwrap();
    use sha2::Digest;
    let hex: String = sha2::Sha256::digest(&stored)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(hex, sha);

    // A directory is not an artifact, and nothing is left in the store.
    let refused = manager
        .fetch_artifact(lease, bot, None, "/home/elsebot/work", 1 << 20)
        .await
        .unwrap();
    assert!(refused.is_err(), "{refused:?}");
    let stored: Vec<_> = std::fs::read_dir(dir.path().join("artifacts").join(bot.to_string()))
        .unwrap()
        .collect();
    assert_eq!(stored.len(), 1, "only the earlier artifact");
    // Exact byte reads.
    manager.write_file(lease, "utf.txt", "éABC").await.unwrap();
    let (size, bytes) = manager
        .read_file(lease, "utf.txt", 1, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!((size, bytes), (5, vec![0xA9, b'A', b'B']));
    // Cancelling a job stops its whole process group.
    let started = manager
        .start_job(
            bot,
            None,
            "sleep 301 & sleep 302; wait",
            Duration::from_secs(600),
        )
        .await
        .unwrap();
    let else_api::computers::jobs::Started::Job(long) = started else {
        panic!("{started:?}")
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    let cancelled = manager.cancel_job(bot, long).await.unwrap().unwrap();
    assert_eq!(cancelled.status, "cancelled");
    let left = manager
        .run_command(
            bot,
            "test:turn2",
            "pgrep -f 'sleep 30[12]' || echo none",
            Duration::from_secs(30),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(left.stdout.trim(), "none", "the job's processes are gone");
    // Output is bounded in the guest: only the end comes back.
    let noisy = manager
        .run_command(bot, "test:turn2", "seq 1 200000", Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert!(noisy.stdout_bytes > 1_000_000 && noisy.stdout.len() <= 6_000);
    assert!(noisy.stdout.trim_end().ends_with("200000"));

    // The browser operates a local page; its storage persists across a stop.
    let page = "<html><head><title>Probe Page</title></head><body>\
        <input id='q'><button id='b' onclick=\"localStorage.setItem('kept', document.getElementById('q').value); \
        document.body.insertAdjacentHTML('beforeend', '<p>CLICKED-' + document.getElementById('q').value + '</p>')\">go</button>\
        </body></html>";
    manager
        .write_file(lease, "/home/elsebot/work/site/page.html", page)
        .await
        .unwrap();
    let reader = "<html><body><script>document.write('KEPT=' + localStorage.getItem('kept'))</script></body></html>";
    manager
        .write_file(lease, "/home/elsebot/work/site/read.html", reader)
        .await
        .unwrap();
    // A local site gives the pages one stable origin.
    manager
        .run_command(
            bot,
            "test:turn2",
            "cd site && (setsid python3 -m http.server 8765 --bind 127.0.0.1 >/dev/null 2>&1 &); sleep 1",
            Duration::from_secs(30),
        )
        .await
        .unwrap()
        .unwrap();
    let opened = manager
        .browser(lease, &["open", "http://127.0.0.1:8765/page.html"])
        .await
        .unwrap();
    assert!(
        opened.stdout.contains("Probe Page"),
        "{} {}",
        opened.stdout,
        opened.stderr
    );
    manager
        .browser(lease, &["type", "#q", "hello"])
        .await
        .unwrap();
    let clicked = manager.browser(lease, &["click", "#b"]).await.unwrap();
    assert!(
        clicked.stdout.contains("CLICKED-hello"),
        "{}",
        clicked.stdout
    );

    // Desktop: loopback noVNC; VNC requires the password.
    let desktop = manager.desktop(bot).await.unwrap().unwrap();
    assert!(desktop.url.starts_with("http://127.0.0.1:"));
    let port: u16 = desktop
        .url
        .split(':')
        .nth(2)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let rfb = std::process::Command::new("python3")
        .arg("-c")
        .arg(format!(
            "import socket,base64,os\n\
s=socket.create_connection(('127.0.0.1',{port}),timeout=5)\n\
k=base64.b64encode(os.urandom(16)).decode()\n\
s.send(('GET /websockify HTTP/1.1\\r\\nHost: x\\r\\nUpgrade: websocket\\r\\nConnection: Upgrade\\r\\nSec-WebSocket-Key: '+k+'\\r\\nSec-WebSocket-Version: 13\\r\\nSec-WebSocket-Protocol: binary\\r\\n\\r\\n').encode())\n\
h=b''\n\
while b'\\r\\n\\r\\n' not in h: h+=s.recv(1)\n\
def frame():\n    b=s.recv(2); n=b[1]&127\n    return s.recv(n)\n\
v=frame()\n\
m=os.urandom(4); d=v\n\
s.send(bytes([0x82,0x80|len(d)])+m+bytes(c^m[i%4] for i,c in enumerate(d)))\n\
t=frame()\n\
print(v.decode().strip(), list(t[1:1+t[0]]))"
        ))
        .output()
        .unwrap();
    let rfb = String::from_utf8_lossy(&rfb.stdout).trim().to_owned();
    eprintln!("RFB: {rfb}");
    assert!(rfb.starts_with("RFB 003.008"), "{rfb}");
    assert!(
        rfb.ends_with("[2]"),
        "only VNC password authentication is offered: {rfb}"
    );
    assert_eq!(desktop.password.len(), 8);
    manager.close_desktop(bot).await.unwrap();

    // Stop and start: files and browser storage survive.
    manager.release(lease).await.unwrap();
    manager.owner_stop(bot).await.unwrap();
    manager.owner_resume(bot).await.unwrap();
    let (lease, _) = ready(&manager, bot, "test:turn3").await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let kept = manager
        .run_command(
            bot,
            "test:turn3",
            "cat made.txt job.txt",
            Duration::from_secs(30),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(kept.stdout.trim(), "GUEST-OK\nJOB-DONE");
    manager
        .run_command(
            bot,
            "test:turn3",
            "cd site && (setsid python3 -m http.server 8765 --bind 127.0.0.1 >/dev/null 2>&1 &); sleep 1",
            Duration::from_secs(30),
        )
        .await
        .unwrap()
        .unwrap();
    let read = manager
        .browser(lease, &["open", "http://127.0.0.1:8765/read.html"])
        .await
        .unwrap();
    assert!(
        read.stdout.contains("KEPT=hello"),
        "browser state survived: {} {}",
        read.stdout,
        read.stderr
    );
    manager.release(lease).await.unwrap();

    // Cleanup of this test's own runtime home only.
    let name = ComputerManager::runtime_name(bot);
    for args in [["stop", name.as_str()], ["rm", name.as_str()]] {
        let _ = std::process::Command::new(&msb_bin)
            .env("MSB_HOME", &home)
            .args(args)
            .output();
    }
}

/// Stops and removes this test's computers from its own runtime home, even
/// when the test fails (before the temporary home is deleted).
struct Cleanup {
    msb: String,
    home: std::path::PathBuf,
    names: std::sync::Mutex<Vec<String>>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        for name in self.names.lock().unwrap().iter() {
            for args in [["stop", "--force"].as_slice(), ["rm"].as_slice()] {
                let _ = std::process::Command::new(&self.msb)
                    .env("MSB_HOME", &self.home)
                    .args(args)
                    .arg(name)
                    .output();
            }
        }
    }
}

/// The real runtime with a gate in front of `create` and `start`, so a test
/// can stop or delete a bot while its computer is booting.
struct Gated {
    inner: Msb,
    entered: tokio::sync::Notify,
    open: tokio::sync::Semaphore,
}

impl Gated {
    async fn gate(&self) {
        self.entered.notify_one();
        self.open.acquire().await.unwrap().forget();
    }
}

#[async_trait::async_trait]
impl ComputerRuntime for Gated {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        self.inner.list().await
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.gate().await;
        self.inner.create(spec).await
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.gate().await;
        self.inner.start(name).await
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        self.inner.stop(name).await
    }
    async fn pause(&self, name: &str) -> Result<(), RuntimeError> {
        self.inner.pause(name).await
    }
    async fn resume(&self, name: &str) -> Result<(), RuntimeError> {
        self.inner.resume(name).await
    }
    async fn exec(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        self.inner.exec(name, program, args, timeout).await
    }
    async fn copy_out(
        &self,
        name: &str,
        guest: &str,
        host: &std::path::Path,
    ) -> Result<(), RuntimeError> {
        self.inner.copy_out(name, guest, host).await
    }
}

/// POC-019: acquisition racing owner stop and deletion against the real
/// runtime and SQLite. The stop or deletion commits while the VM is
/// booting (held at the gate); the late start must not be granted, and the
/// computer must already be stopped, before maintenance runs. Admission
/// queues beyond the budget. Also reports VM worker memory.
#[tokio::test(flavor = "multi_thread")]
async fn acquisition_races_stop_and_deletion_with_the_real_runtime() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_VM=1 to boot real Microsandbox VMs");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let msb_bin = format!("{}/.microsandbox/bin/msb", std::env::var("HOME").unwrap());
    let home = dir.path().join("microsandbox");
    let app = TestApp::installed().await;
    let cleanup = Cleanup {
        msb: msb_bin.clone(),
        home: home.clone(),
        names: std::sync::Mutex::default(),
    };
    let config = ComputerConfig {
        idle_after: Duration::from_secs(3600),
        max_cpus: 1,
        max_memory_mib: 2048 + 256,
        state_dir: dir.path().to_owned(),
        ..ComputerConfig::default()
    };
    let gated = Arc::new(Gated {
        inner: Msb::detect(dir.path(), None).unwrap(),
        entered: tokio::sync::Notify::new(),
        open: tokio::sync::Semaphore::new(0),
    });
    let manager = Arc::new(ComputerManager::new(
        app.db.pool.clone(),
        gated.clone(),
        config,
    ));
    manager.reconcile_startup().await.unwrap();
    let a = app.create_bot("RaceA").await;
    let a_id: Uuid = a.parse().unwrap();
    let b = app.create_bot("RaceB").await;
    let b_id: Uuid = b.parse().unwrap();
    for id in [a_id, b_id] {
        cleanup
            .names
            .lock()
            .unwrap()
            .push(ComputerManager::runtime_name(id));
    }
    let held = |bot: Uuid| {
        let pool = app.db.pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM computer_leases WHERE bot_id = ?1 AND released_at IS NULL",
            )
            .bind(bot)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let name_a = ComputerManager::runtime_name(a_id);
    let name_b = ComputerManager::runtime_name(b_id);

    // Measure the worker while A runs; B queues behind the one-VM budget.
    gated.open.add_permits(1);
    let Acquire::Ready { lease_id, .. } = manager.acquire(a_id, "test:m").await.unwrap() else {
        panic!()
    };
    assert!(matches!(
        manager.acquire(b_id, "test:q").await.unwrap(),
        Acquire::Queued { .. }
    ));
    let rss = std::process::Command::new("sh")
        .args([
            "-c",
            &format!(
                "for p in $(pgrep -f 'msb machine --name {name_a}'); do grep VmRSS /proc/$p/status; done"
            ),
        ])
        .output()
        .unwrap();
    eprintln!(
        "VM worker for an idle python guest: {}",
        String::from_utf8_lossy(&rss.stdout).trim()
    );
    manager.release(lease_id).await.unwrap();
    manager.owner_stop(a_id).await.unwrap();
    manager.owner_resume(a_id).await.unwrap();

    // Owner stop commits while the start is in flight.
    let m = manager.clone();
    let acquiring = tokio::spawn(async move { m.acquire(a_id, "test:r").await.unwrap() });
    gated.entered.notified().await;
    let m = manager.clone();
    // Its intent commits at once; it then waits for the start to finish.
    let stopping = tokio::spawn(async move { m.owner_stop(a_id).await.unwrap() });
    loop {
        let desired: String =
            sqlx::query_scalar("SELECT desired_state FROM bot_computers WHERE bot_id = ?1")
                .bind(a_id)
                .fetch_one(&app.db.pool)
                .await
                .unwrap();
        if desired == "stopped" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    gated.open.add_permits(1);
    assert_eq!(acquiring.await.unwrap(), Acquire::OwnerStopped);
    assert_eq!(held(a_id).await, 0, "the late start granted nothing");
    assert_eq!(
        manager.runtime_state(&name_a).await.unwrap(),
        Some(RuntimeState::Stopped),
        "and did not stay up"
    );
    stopping.await.unwrap();

    // Deletion commits while B's first boot is in flight.
    let m = manager.clone();
    let acquiring = tokio::spawn(async move { m.acquire(b_id, "test:d").await.unwrap() });
    gated.entered.notified().await;
    let deleted = app.owner("DELETE", &format!("/v1/bots/{b}"), None).await;
    assert!(deleted.status.is_success());
    gated.open.add_permits(1);
    assert_eq!(acquiring.await.unwrap(), Acquire::BotInactive);
    assert_eq!(held(b_id).await, 0);
    let state = manager.runtime_state(&name_b).await.unwrap();
    assert!(matches!(state, Some(RuntimeState::Stopped)), "{state:?}");
    manager
        .tick(&tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(b_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "deleted");
    assert_eq!(
        manager.acquire(b_id, "test:late").await.unwrap(),
        Acquire::BotInactive
    );
}

/// POC-022 against the real runtime: one coordinated set captures two bots'
/// computers (guest files and browser storage), chats, memory, and the
/// database; the original runtime is destroyed; the set restores into a new
/// installation directory, runtime home, and database; the recovered API
/// starts twice without changing saved state or replaying anything.
#[tokio::test(flavor = "multi_thread")]
async fn a_coordinated_backup_restores_the_assistant_into_a_new_installation() {
    if !live() {
        eprintln!("SKIPPED: set ELSE_LIVE_VM=1 to boot real Microsandbox VMs");
        return;
    }
    use else_api::backup::{self, DatabaseParams};
    use sqlx::Connection;
    let dir = tempfile::tempdir().unwrap();
    let msb_bin = format!("{}/.microsandbox/bin/msb", std::env::var("HOME").unwrap());
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let home = data.join("microsandbox");
    std::fs::create_dir_all(&home).unwrap();
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
        std::process::Command::new(&msb_bin)
            .env("MSB_HOME", &home)
            .args(["load", "-q", "-t", "else-guest:2", "-i"])
            .arg(&tar)
            .status()
            .unwrap()
            .success()
    );
    let new_dir = dir.path().join("restored");
    let new_home = new_dir.join("microsandbox");
    let cleanup_old = Cleanup {
        msb: msb_bin.clone(),
        home: home.clone(),
        names: std::sync::Mutex::default(),
    };
    let cleanup_new = Cleanup {
        msb: msb_bin.clone(),
        home: new_home.clone(),
        names: std::sync::Mutex::default(),
    };
    let db = Arc::new(common::TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let config = |state: &std::path::Path| ComputerConfig {
        image: "else-guest:2".into(),
        desktop: true,
        idle_after: Duration::from_secs(3600),
        state_dir: state.to_owned(),
        ..ComputerConfig::default()
    };
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        Arc::new(Msb::detect(&data, None).unwrap()),
        config(&data),
    ));
    manager.reconcile_startup().await.unwrap();
    let registry = else_api::providers::Registry::new(vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(1)),
    )]);
    let app = TestApp::with_parts(db.clone(), 4, registry, Some(manager.clone()));
    let site = "cd site && (setsid python3 -m http.server 8765 --bind 127.0.0.1 >/dev/null 2>&1 &); sleep 1";
    let mut bots = Vec::new();
    for name in ["Alpha", "Beta"] {
        let bot: Uuid = app.create_bot(name).await.parse().unwrap();
        for list in [&cleanup_old.names, &cleanup_new.names] {
            list.lock()
                .unwrap()
                .push(ComputerManager::runtime_name(bot));
        }
        let (lease, _) = ready(&manager, bot, "test:prep").await;
        manager
            .write_file(
                lease,
                "/home/elsebot/work/note.txt",
                &format!("FILE-{name}"),
            )
            .await
            .unwrap();
        let page = format!(
            "<html><head><title>Set</title></head><body><script>localStorage.setItem('kept', '{name}')</script></body></html>"
        );
        manager
            .write_file(lease, "/home/elsebot/work/site/set.html", &page)
            .await
            .unwrap();
        let reader = "<html><body><script>document.write('KEPT=' + localStorage.getItem('kept'))</script></body></html>";
        manager
            .write_file(lease, "/home/elsebot/work/site/read.html", reader)
            .await
            .unwrap();
        manager
            .run_command(bot, "test:prep", site, Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        let opened = manager
            .browser(lease, &["open", "http://127.0.0.1:8765/set.html"])
            .await
            .unwrap();
        assert!(
            opened.stdout.contains("Set"),
            "{} {}",
            opened.stdout,
            opened.stderr
        );
        manager.release(lease).await.unwrap();
        let reply = app
            .send(
                &bot.to_string(),
                Uuid::new_v4(),
                &format!("{name} remembers ORCHID-4"),
                Some("synthetic"),
            )
            .await;
        app.wait_turn(reply.body["turn"]["id"].as_str().unwrap(), &["completed"])
            .await;
        bots.push(bot);
    }
    let (alpha, beta) = (bots[0], bots[1]);
    // Beta's owner stops its computer; Alpha's stays up, idle.
    manager.owner_stop(beta).await.unwrap();
    // An action whose delivery is unknown must never be replayed.
    let main = app.main_bot().await;
    let reply = app
        .send(
            &main,
            Uuid::new_v4(),
            "Pay the invoice [synthetic:stall-send]",
            Some("synthetic"),
        )
        .await;
    let stalled = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&stalled, &["sending"]).await;
    app.owner(
        "POST",
        &format!("/v1/bots/{main}/cancel"),
        Some(serde_json::json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    app.wait_turn(&stalled, &["uncertain"]).await;
    let alpha_password = std::fs::read_to_string(manager.desktop_password_file(alpha)).unwrap();
    // Saved state beyond chats: a deleted bot, a changed default bot, a
    // customized profile, and memory built into its tree and views.
    let gamma = app.create_bot("Gamma").await;
    assert!(
        app.owner("DELETE", &format!("/v1/bots/{gamma}"), None)
            .await
            .status
            .is_success()
    );
    sqlx::query("UPDATE installation SET default_bot_id = ?1")
        .bind(alpha)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE provider_profiles SET reasoning_effort = 'high', revision = revision + 1
         WHERE provider_id = 'codex'",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let compactor = else_api::memory_worker::Compactor::deterministic(db.pool.clone());
    for _ in 0..5000 {
        if compactor.step().await.unwrap() == else_api::memory_worker::Step::Idle {
            break;
        }
    }
    let nodes: i64 = sqlx::query_scalar("SELECT count(*) FROM memory_node")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert!(nodes > 0, "memory has a tree");

    let key = else_api::credentials::SecretKey::parse(&{
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode([3u8; 32])
    })
    .unwrap();
    let params = DatabaseParams::parse(db.url.expose()).unwrap();
    let worker = else_api::backup::worker::BackupWorker::new(
        db.pool.clone(),
        params.clone(),
        Arc::new(
            else_api::credentials::SecretKey::parse(&{
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode([3u8; 32])
            })
            .unwrap(),
        ),
        else_api::backup::capture::BackupConfig {
            dir: dir.path().join("backups"),
            keep: 7,
            max_bytes: 8 << 30,
            interval: Duration::from_secs(3600),
        },
    )
    .with_coordinator(else_api::backup::capture::Coordinator {
        scheduler: Some(app.scheduler.clone()),
        computers: Some(manager.clone()),
        drain: Duration::from_secs(30),
        exclusive: false,

        projects_dir: Some(dir.path().join("bots")),
    });
    let started = std::time::Instant::now();
    let summary = worker.run_once().await.unwrap();
    let sizes: Vec<(String, u64)> = summary
        .manifest
        .artifacts
        .iter()
        .map(|a| (a.object.clone(), a.ciphertext_bytes))
        .collect();
    eprintln!(
        "set {} in {:?}: {sizes:?}",
        summary.set_id,
        started.elapsed()
    );
    assert!(
        summary.manifest.coverage.whole_assistant,
        "{:?}",
        summary.manifest.coverage
    );
    assert_eq!(summary.manifest.artifacts.len(), 3);
    assert_eq!(
        manager
            .runtime_state(&ComputerManager::runtime_name(beta))
            .await
            .unwrap(),
        Some(RuntimeState::Stopped)
    );
    // What the restored installation must show.
    let saved = |url: String| async move {
        let mut conn = sqlx::SqliteConnection::connect(&url).await.unwrap();
        let mut out = Vec::new();
        for sql in [
            "SELECT id, name, status FROM bots",
            "SELECT default_bot_id FROM installation",
            "SELECT id, bot_id, kind, last_turn_no FROM conversations",
            "SELECT id, conversation_id, seq, kind, text FROM items",
            "SELECT id, status, error_code, effect_uncertain FROM turns",
            "SELECT provider_id, model, reasoning_effort, service_tier, revision FROM provider_profiles",
            "SELECT bot_id, seq, kind, body FROM memory_entry",
            "SELECT * FROM memory_generation",
            "SELECT * FROM memory_node",
        ] {
            let rows = common::rows(&mut conn, sql).await;
            out.push(rows);
        }
        out
    };
    let source = saved(db.url.expose().to_owned()).await;
    drop(app);

    // The original runtime is gone.
    for name in cleanup_old.names.lock().unwrap().iter() {
        for args in [["stop", name.as_str()], ["rm", name.as_str()]] {
            let _ = std::process::Command::new(&msb_bin)
                .env("MSB_HOME", &home)
                .args(args)
                .output();
        }
    }
    std::fs::remove_dir_all(&home).unwrap();

    // Restore into a new installation directory, runtime home, and database.
    std::fs::create_dir(&new_dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&new_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = format!("else_restore_{}", Uuid::new_v4().simple());
    let restored_runtime = Arc::new(Msb::detect(&new_dir, None).unwrap());
    let started = std::time::Instant::now();
    let report = backup::restore::restore(backup::restore::RestoreRequest {
        set_dir: &summary.path,
        key: &key,
        server: &params,
        target_database: &target,
        staging_parent: &dir.path().join("staging"),
        max_bytes: 8 << 30,
        computers: Some(backup::restore::ComputerTarget {
            runtime: Some(restored_runtime.clone()),
            data_dir: &new_dir,
        }),
    })
    .await;
    eprintln!("restore took {:?}", started.elapsed());
    let target_params = params.with_database(&target);
    let target_url = format!(
        "sqlite://{}",
        target_params
            .with_database(&format!("{target}"))
            .options
            .get_filename()
            .display()
    );
    let drop_target = || {
        let options = params.options.clone();
        let target = target.clone();
        async move {
            common::remove_database(&options, &target);
        }
    };
    if let Err(err) = &report {
        drop_target().await;
        panic!("restore failed: {err}");
    }
    let restored = saved(target_url.clone()).await;
    assert_eq!(
        source, restored,
        "bots, default, chats, turns, profiles, memory"
    );

    // The restored computers: files, browser storage, desktop password,
    // the owner's stop, and the network policy.
    let pool = sqlx::SqlitePool::connect(&target_url).await.unwrap();
    let recovered = ComputerManager::new(pool.clone(), restored_runtime.clone(), config(&new_dir));
    recovered.reconcile_startup().await.unwrap();
    assert_eq!(
        recovered.acquire(beta, "test:after").await.unwrap(),
        Acquire::OwnerStopped
    );
    recovered.owner_resume(beta).await.unwrap();
    for (bot, name) in [(alpha, "Alpha"), (beta, "Beta")] {
        let (lease, _) = ready(&recovered, bot, "test:after").await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        let note = recovered
            .run_command(bot, "test:after", "cat note.txt", Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(note.stdout.trim(), format!("FILE-{name}"));
        recovered
            .run_command(bot, "test:after", site, Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        let read = recovered
            .browser(lease, &["open", "http://127.0.0.1:8765/read.html"])
            .await
            .unwrap();
        assert!(
            read.stdout.contains(&format!("KEPT={name}")),
            "{} {}",
            read.stdout,
            read.stderr
        );
        recovered.release(lease).await.unwrap();
    }
    let desktop = recovered.desktop(alpha).await.unwrap().unwrap();
    assert_eq!(desktop.password, alpha_password.trim());
    recovered.close_desktop(alpha).await.unwrap();
    // The restored network policy: public egress works, internal and
    // metadata destinations get the runtime's denial, and the desktop's
    // published port accepts only password-authenticated VNC.
    let (lease, _) = ready(&recovered, alpha, "test:net").await;
    recovered
        .write_file(lease, "/home/elsebot/work/probe.py", PROBE)
        .await
        .unwrap();
    let probe = recovered
        .run_command(
            alpha,
            "test:net",
            "python3 probe.py",
            Duration::from_secs(60),
        )
        .await
        .unwrap()
        .unwrap();
    eprintln!("network after restore:\n{}", probe.stdout);
    let lines: Vec<&str> = probe.stdout.lines().collect();
    assert!(
        lines[0].to_lowercase().contains("cloudflare"),
        "public egress: {lines:?}"
    );
    for line in &lines[1..3] {
        assert!(
            line.contains("403") && line.contains("application/json"),
            "denied: {line}"
        );
    }
    let desktop = recovered.desktop(alpha).await.unwrap().unwrap();
    let port: u16 = desktop
        .url
        .split(':')
        .nth(2)
        .unwrap()
        .split('/')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let rfb = std::process::Command::new("python3")
        .arg("-c")
        .arg(RFB.replace("PORT", &port.to_string()))
        .output()
        .unwrap();
    let rfb = String::from_utf8_lossy(&rfb.stdout).trim().to_owned();
    assert!(
        rfb.starts_with("RFB 003.008") && rfb.ends_with("[2]"),
        "desktop ingress: {rfb}"
    );
    recovered.close_desktop(alpha).await.unwrap();
    recovered.release(lease).await.unwrap();
    for bot in [alpha, beta] {
        recovered.owner_stop(bot).await.unwrap();
    }
    drop(recovered);
    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM provider_sessions WHERE status = 'active'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sessions, 0, "context is rebuilt from SQLite");

    // Start the recovered API twice: nothing saved changes, nothing replays.
    else_api::credentials::write_secret_key(&new_dir.join("secret-key"), &key).unwrap();
    else_api::credentials::provision_owner_token(&new_dir.join("owner-token")).unwrap();
    let before = saved(target_url.clone()).await;
    for start in 1..=2 {
        let port = common::free_port();
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_else-api"))
            .current_dir(common::without_dotenv(new_dir.parent().unwrap()))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &new_dir)
            .env("DATABASE_URL", &target_url)
            .env("ELSE_DATA_DIR", &new_dir)
            .env("ELSE_API_BIND", format!("127.0.0.1:{port}"))
            .env("ELSE_SYNTHETIC_PROVIDER", "1")
            .env("ELSE_BACKUP_INTERVAL_SECS", "0")
            .env("ELSE_MSB_BIN", &msb_bin)
            .env("ELSE_VM_IMAGE", "else-guest:2")
            .env("ELSE_VM_DESKTOP", "1")
            .env("RUST_LOG", "warn")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let client = reqwest::Client::new();
        let mut up = false;
        for _ in 0..400 {
            if client
                .get(format!("http://127.0.0.1:{port}/readyz"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
        let status = child.wait().unwrap();
        assert!(up, "start {start} did not become ready");
        assert!(status.success(), "start {start}: {status}");
        assert_eq!(
            saved(target_url.clone()).await,
            before,
            "start {start} changed saved state"
        );
    }
    pool.close().await;
    drop_target().await;
}

/// Network probe run inside a restored computer.
const PROBE: &str = r#"import urllib.request, urllib.error
for u in ['http://1.1.1.1/', 'http://10.0.0.1/', 'http://169.254.169.254/']:
    try:
        r = urllib.request.urlopen(u, timeout=10); print(u, r.status, r.headers.get('Server'))
    except urllib.error.HTTPError as e:
        print(u, e.code, e.headers.get('Content-Type'), e.headers.get('Server'))
    except Exception as e:
        print(u, 'ERR', type(e).__name__)
"#;

/// A noVNC/RFB handshake that reports the offered security types.
const RFB: &str = r#"import socket,base64,os
s=socket.create_connection(('127.0.0.1',PORT),timeout=10)
k=base64.b64encode(os.urandom(16)).decode()
s.send(('GET /websockify HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: '+k+'\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n\r\n').encode())
h=b''
while b'\r\n\r\n' not in h: h+=s.recv(1)
def frame():
    b=s.recv(2); n=b[1]&127
    return s.recv(n)
v=frame()
m=os.urandom(4); d=v
s.send(bytes([0x82,0x80|len(d)])+m+bytes(c^m[i%4] for i,c in enumerate(d)))
t=frame()
print(v.decode().strip(), list(t[1:1+t[0]]))
"#;
