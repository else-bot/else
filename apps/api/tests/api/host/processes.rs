//! Host process runs: environment, output limits, termination, and cleanup.

use super::*;

#[tokio::test]
async fn runs_start_in_the_project_with_the_users_session_environment() {
    let fx = fixture();
    let bot = Uuid::new_v4();
    let spec = shell(&fx, bot, "pwd; echo ---; env");
    let dirs = &spec.dirs;
    assert_eq!(dirs.project, fx.bots.join(bot.to_string()));
    assert_eq!(fx.host.project_path(bot), dirs.project);
    assert!(dirs.provider_home.starts_with(fx.data.join("bots")));
    for dir in [&dirs.project, &dirs.provider_home] {
        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
    let out = output(&fx, &spec).await;
    let (cwd, env) = out.split_once("---").unwrap();
    assert_eq!(
        Path::new(cwd.trim()).canonicalize().unwrap(),
        dirs.project.canonicalize().unwrap(),
        "the run starts in the bot's project"
    );
    let vars: Vec<(&str, &str)> = env.lines().filter_map(|l| l.split_once('=')).collect();
    let get = |name: &str| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
    assert_eq!(get("HOME"), std::env::var("HOME").ok().as_deref());
    assert_eq!(get("PATH"), std::env::var("PATH").ok().as_deref());
    assert_eq!(get("PROVIDER_SETTING"), Some("allowed"));
    assert_eq!(
        get(MARKER_VAR),
        Some(format!("{}:{}", fx.host.marker_prefix(), spec.turn_id).as_str())
    );
    let session = [
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "PATH",
        "LANG",
        "LANGUAGE",
        "TZ",
        "TMPDIR",
        "TERM",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "DBUS_SESSION_BUS_ADDRESS",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "SSH_AUTH_SOCK",
    ];
    let shell_own = ["PWD", "OLDPWD", "SHLVL", "_"];
    for (key, _) in &vars {
        assert!(
            session.contains(key)
                || shell_own.contains(key)
                || key.starts_with("LC_")
                || *key == "PROVIDER_SETTING"
                || *key == MARKER_VAR,
            "{key} leaked into the provider's environment"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn termination_reaps_the_whole_process_group() {
    let fx = fixture();
    let spec = shell(
        &fx,
        Uuid::new_v4(),
        "sleep 300 & sleep 300 & sh -c 'sleep 300' & wait",
    );
    let mut process = Supervised::spawn(fx.host.command(&spec)).unwrap();
    common::until(
        "the process group descendants to be discoverable",
        common::poll(|| {
            std::future::ready((fx.host.live_processes(spec.turn_id).len() >= 4).then_some(()))
        }),
    )
    .await;
    process.terminate(Duration::from_secs(2)).await;
    common::until(
        "the process group to be reaped",
        common::poll(|| {
            std::future::ready(
                fx.host
                    .live_processes(spec.turn_id)
                    .is_empty()
                    .then_some(()),
            )
        }),
    )
    .await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn orphans_are_swept() {
    let fx = fixture();
    // Keep the pipe open on fd 3 because the background shell's stdin is /dev/null.
    let spec = shell(
        &fx,
        Uuid::new_v4(),
        "exec 3<&0; (printf r; read -r line <&3) & wait",
    );
    let mut command = fx.host.command(&spec);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut process = Supervised::spawn(command).unwrap();
    let _stdin = process.child.stdin.take().unwrap();
    let mut stdout = process.child.stdout.take().unwrap();
    let mut ready = [0];
    common::until(
        "the orphan child to start",
        tokio::io::AsyncReadExt::read_exact(&mut stdout, &mut ready),
    )
    .await
    .unwrap();
    std::mem::forget(process);
    assert!(!fx.host.live_processes(spec.turn_id).is_empty());
    assert!(fx.host.sweep_orphans().unwrap() >= 1);
    common::until(
        "the orphan process to be swept",
        common::poll(|| {
            std::future::ready(
                fx.host
                    .live_processes(spec.turn_id)
                    .is_empty()
                    .then_some(()),
            )
        }),
    )
    .await;
}

#[tokio::test]
async fn launch_runs_stop_long_or_large_output() {
    let fx = fixture();
    let bot = Uuid::new_v4();
    let run = |script: &str, timeout: Duration, limit: usize| {
        let run = Run {
            limit,
            timeout,
            ..Run::launch(&shell(&fx, bot, script))
        };
        let host = fx.host.clone();
        async move { host.run(&run).await }
    };
    assert_eq!(run("printf ok", common::SETTLE, 2).await.unwrap(), b"ok");
    assert!(run("printf too-long", common::SETTLE, 2).await.is_err());
    assert!(run("yes", common::SETTLE, 1 << 16).await.is_err());
    assert!(run("printf ok; exit 3", common::SETTLE, 16).await.is_err());
    let long_run = run("sleep 300", Duration::from_millis(300), 16);
    assert!(
        common::until("the host to stop the long command", long_run)
            .await
            .is_err()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_metadata_export_kills_descendants_after_the_leader_exits() {
    let fx = fixture();
    let pid_file = fx.data.join("export-child.pid");
    let mut spec = shell(
        &fx,
        Uuid::new_v4(),
        "sleep 300 & printf '%s %s' \"$$\" \"$!\" > \"$CHILD_PID_FILE\"; exit 0",
    );
    spec.env
        .push(("CHILD_PID_FILE".into(), pid_file.to_str().unwrap().into()));
    let host = fx.host.clone();
    let exporting = tokio::spawn(async move {
        let run = Run {
            limit: 1024,
            timeout: common::SETTLE * 3,
            ..Run::launch(&spec)
        };
        host.run(&run).await
    });
    let (leader, child) = common::until(
        "the exporter to write both process IDs",
        common::poll(|| async {
            let text = std::fs::read_to_string(&pid_file).ok()?;
            let pids = text
                .split_whitespace()
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            let [leader, child] = pids.as_slice() else {
                return None;
            };
            Some((*leader, *child))
        }),
    )
    .await;
    common::until(
        "the exporter leader to exit while its descendant holds stdout",
        common::poll(|| async {
            let state = std::fs::read_to_string(format!("/proc/{leader}/stat")).unwrap();
            state
                .split_once(") ")
                .is_some_and(|(_, state)| state.starts_with('Z'))
                .then_some(())
        }),
    )
    .await;
    let alive = std::fs::read_to_string(format!("/proc/{child}/stat")).unwrap();
    assert!(!alive.split_once(") ").unwrap().1.starts_with('Z'));
    exporting.abort();
    assert!(exporting.await.unwrap_err().is_cancelled());
    common::until(
        "the cancelled exporter's descendant to exit",
        common::poll(|| async {
            match std::fs::read_to_string(format!("/proc/{child}/stat")) {
                Err(_) => Some(()),
                Ok(state) => state
                    .split_once(") ")
                    .is_some_and(|(_, state)| state.starts_with('Z'))
                    .then_some(()),
            }
        }),
    )
    .await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn turn_cleanup_kills_stubborn_descendants_without_touching_another_turn() {
    let fx = fixture();
    let ready = fx.data.join("child-ready");
    let mut spec = shell(
        &fx,
        Uuid::new_v4(),
        "sh -c 'trap \"\" TERM; touch \"$READY\"; while :; do sleep 1; done' & wait",
    );
    spec.env
        .push(("READY".into(), ready.to_string_lossy().into_owned()));
    let other = shell(&fx, Uuid::new_v4(), "sleep 300");
    let mut other_process = Supervised::spawn(fx.host.command(&other)).unwrap();
    let mut process = Supervised::spawn(fx.host.command(&spec)).unwrap();
    common::until(
        "the stubborn descendant to be ready",
        common::poll(|| std::future::ready(ready.is_file().then_some(()))),
    )
    .await;
    process.terminate(Duration::from_millis(100)).await;
    assert!(!fx.host.live_processes(spec.turn_id).is_empty());
    assert!(fx.host.sweep_turn(spec.turn_id).unwrap() > 0);
    assert!(fx.host.live_processes(spec.turn_id).is_empty());
    assert!(!fx.host.live_processes(other.turn_id).is_empty());
    other_process.terminate(Duration::ZERO).await;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn a_dropped_finish_still_sweeps_a_detached_worker() {
    use else_api::providers::cli::process::{Finish, Scope};
    let fx = fixture();
    let ready = fx.data.join("worker-ready");
    let mut spec = shell(
        &fx,
        Uuid::new_v4(),
        "python3 -c 'import os,signal,time,sys\nif os.fork() == 0:\n    os.setsid(); signal.signal(signal.SIGTERM, signal.SIG_IGN); open(sys.argv[1], \"w\").close()\n    while True: time.sleep(1)' \"$READY\"; while :; do sleep 1; done",
    );
    spec.env
        .push(("READY".into(), ready.to_string_lossy().into_owned()));
    let scope = Scope::spawn(
        &fx.host,
        &spec,
        std::process::Stdio::null(),
        std::process::Stdio::null(),
    )
    .unwrap();
    common::until(
        "the detached worker to be ready",
        common::poll(|| std::future::ready(ready.is_file().then_some(()))),
    )
    .await;
    assert!(fx.host.live_processes(spec.turn_id).len() >= 2);
    common::never(
        "cleanup to finish while the leader is still in its exit grace",
        Duration::from_millis(200),
        scope.finish(Finish::Exit),
    )
    .await;
    common::until(
        "the dropped cleanup to sweep the detached worker",
        common::poll(|| {
            std::future::ready(
                fx.host
                    .live_processes(spec.turn_id)
                    .is_empty()
                    .then_some(()),
            )
        }),
    )
    .await;
}
