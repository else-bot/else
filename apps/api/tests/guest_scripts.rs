//! POC-026 review: the guest scripts themselves (the `RUN`/`LAUNCH`/`KILL`
//! shells, the stdin secret prologue, and the in-guest output filter) run
//! for real here, on this host in a private home, through a local runtime
//! that kills a timed-out command's process group the way Microsandbox
//! does. It checks what fakes cannot: exact secret values, filtered output
//! and job files, timeouts, and descendants. The scripts expect the guest's
//! Linux userland (`/usr/bin/setsid`, `/proc`), so these run only on Linux.
#![cfg(target_os = "linux")]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::TestApp;
use else_api::computers::jobs::Started;
use else_api::computers::{
    Acquire, ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, RuntimeError,
    RuntimeState, Spec,
};
use else_api::db::task_secrets::Env;
use uuid::Uuid;
use zeroize::Zeroizing;

/// Runs guest commands as local processes: `HOME` and `/var/lib/elsebot`
/// map into a temporary directory. A timeout kills the command's process
/// group, as the runtime's guest agent does.
struct Local {
    root: PathBuf,
}

impl Local {
    fn map(&self, arg: &str) -> String {
        arg.replace(
            "/var/lib/elsebot",
            &self.root.join("var-lib-elsebot").display().to_string(),
        )
    }
}

#[async_trait]
impl ComputerRuntime for Local {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        Ok(Vec::new())
    }
    async fn create(&self, _: &Spec) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn start(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn stop(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn pause(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn resume(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn copy_out(&self, _: &str, _: &str, _: &std::path::Path) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn exec(
        &self,
        name: &str,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        self.exec_input(name, program, args, &[], timeout).await
    }
    async fn exec_input(
        &self,
        _: &str,
        program: &str,
        args: &[&str],
        input: &[u8],
        timeout: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        use tokio::io::AsyncWriteExt;
        let home = self.root.join("home");
        std::fs::create_dir_all(home.join("work")).unwrap();
        let mut command = tokio::process::Command::new(program);
        command
            .args(args.iter().map(|a| self.map(a)))
            .env_clear()
            .env("HOME", &home)
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .process_group(0)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap();
        let pgid = child.id().unwrap() as i32;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input).await.unwrap();
        drop(stdin);
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(out) => {
                let out = out.unwrap();
                Ok(ExecOutput {
                    status: out.status.code(),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    truncated: false,
                })
            }
            Err(_) => {
                // SAFETY: signalling the group this call created.
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
                Err(RuntimeError::Failed {
                    operation: "exec".into(),
                    message: "timed out".into(),
                })
            }
        }
    }
}

struct Setup {
    app: TestApp,
    manager: Arc<ComputerManager>,
    root: tempfile::TempDir,
    bot: Uuid,
}

async fn setup() -> Setup {
    let app = TestApp::installed().await;
    let root = tempfile::tempdir().unwrap();
    let manager = Arc::new(ComputerManager::new(
        app.db.pool.clone(),
        Arc::new(Local {
            root: root.path().to_owned(),
        }),
        ComputerConfig {
            idle_after: Duration::from_secs(3600),
            state_dir: root.path().join("state"),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let bot: Uuid = app.create_bot("Local").await.parse().unwrap();
    assert!(matches!(
        manager.acquire(bot, "test:hold").await.unwrap(),
        Acquire::Ready { .. }
    ));
    Setup {
        app,
        manager,
        root,
        bot,
    }
}

fn env(vars: &[(&str, &str)]) -> Env {
    Env {
        vars: vars
            .iter()
            .map(|(n, v)| ((*n).to_owned(), Zeroizing::new((*v).to_owned())))
            .collect(),
    }
}

/// Our own processes whose command line contains `pattern`.
fn processes(pattern: &str) -> usize {
    let out = std::process::Command::new("pgrep")
        .args([
            "-f",
            "-u",
            &else_api::credentials::current_uid().to_string(),
            pattern,
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).lines().count()
}

async fn gone(pattern: &str) {
    let started = Instant::now();
    while processes(pattern) > 0 {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{pattern} still runs"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_values_arrive_exactly_and_never_reach_output() {
    let Setup {
        app: _app,
        manager,
        bot,
        ..
    } = setup().await;
    let value = "pa55 word\nwith a trailing newline\n";
    let big: String = (0..5000).map(|i| (b'a' + (i % 26) as u8) as char).collect();
    let secrets = env(&[("SMTP_PASSWORD", value), ("BIG_TOKEN", &big)]);
    let out = manager
        .run_command_with(
            bot,
            "test:hold",
            "printf %s \"$SMTP_PASSWORD\" | wc -c; echo \"pw=$SMTP_PASSWORD\"; \
             echo \"to stderr $SMTP_PASSWORD\" >&2",
            Duration::from_secs(30),
            &secrets,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.exit_code, Some(0), "{}", out.stderr);
    let lines: Vec<&str> = out.stdout.lines().collect();
    assert_eq!(
        lines[0],
        value.len().to_string(),
        "the value arrived byte for byte"
    );
    assert!(
        out.stdout.contains("pw=[secret SMTP_PASSWORD]"),
        "{}",
        out.stdout
    );
    assert!(
        out.stderr.contains("to stderr [secret SMTP_PASSWORD]"),
        "{}",
        out.stderr
    );
    // A long value where the returned tail starts: redacted whole.
    let out = manager
        .run_command_with(
            bot,
            "test:hold",
            "head -c 3000 /dev/zero | tr '\\0' x; printf %s \"$BIG_TOKEN\"",
            Duration::from_secs(30),
            &secrets,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.stdout.ends_with("xxx[secret BIG_TOKEN]"),
        "the tail is redacted whole"
    );
    assert!(!out.stdout.contains(&big[..64]) && !out.stdout.contains(&big[big.len() - 64..]));
    assert!(!out.stdout.contains("pa55") && !out.stderr.contains("pa55"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_jobs_output_file_never_holds_a_secret() {
    let Setup {
        app: _app,
        manager,
        root,
        bot,
    } = setup().await;
    let secrets = env(&[("API_TOKEN", "tok-0123456789")]);
    let Started::Job(job) = manager
        .start_job_with(
            bot,
            None,
            "echo \"token is $API_TOKEN\"; printf 'split %s' \"$API_TOKEN\"; exit 3",
            Duration::from_secs(60),
            &secrets,
            &["api".to_owned()],
        )
        .await
        .unwrap()
    else {
        panic!("not started")
    };
    let dir = root.path().join(format!("var-lib-elsebot/jobs/{job}"));
    let started = Instant::now();
    while !dir.join("exit").exists() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the job did not finish"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let file = std::fs::read_to_string(dir.join("out")).unwrap();
    assert_eq!(
        file,
        "token is [secret API_TOKEN]\nsplit [secret API_TOKEN]"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("exit")).unwrap().trim(),
        "3"
    );
    let state = manager.job(bot, job).await.unwrap().unwrap();
    assert_eq!(
        (state.status.as_str(), state.exit_code),
        ("failed", Some(3))
    );
    assert!(!state.output_tail.contains("0123456789"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timed_out_command_and_its_descendants_end_and_are_recorded() {
    let Setup {
        app, manager, bot, ..
    } = setup().await;
    let started = Instant::now();
    let out = manager
        .run_command(
            bot,
            "test:hold",
            "sleep 61.731 & sleep 62.731; echo late",
            Duration::from_secs(2),
        )
        .await;
    assert!(out.is_err(), "the runtime call timed out: {out:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
    gone("sleep 6[12].731").await;
    let ended: (Option<String>, bool) =
        sqlx::query_as("SELECT end_reason, ended_at IS NOT NULL FROM guest_runs WHERE bot_id = ?1")
            .bind(bot)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(ended, (Some("killed after timeout".into()), true));

    // What a command leaves running in its group ends with it.
    let out = manager
        .run_command(
            bot,
            "test:hold",
            "sleep 63.731 & echo started",
            Duration::from_secs(30),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out.stdout.trim(), "started");
    gone("sleep 63.731").await;
}
