//! Probing host CLIs and their startup config support.

use super::*;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_probe_waits_for_an_executable_writer_to_close() {
    let fx = fixture();
    let program = fx.data.join("provider");
    std::fs::write(&program, "#!/bin/sh\necho provider-version\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&program)
        .unwrap();
    let error = std::process::Command::new(&program).output().unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ETXTBSY));
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        drop(writer);
    });
    assert_eq!(
        fx.host.probe(&program, &["--version"], &[]).await.unwrap(),
        "provider-version"
    );
}

#[tokio::test]
async fn a_probe_does_not_repeat_a_command_that_exits_with_failure() {
    let fx = fixture();
    let calls = fx.data.join("probe-calls");
    assert!(
        fx.host
            .probe(
                Path::new("/bin/sh"),
                &["-c", "echo called >> \"$CALLS\"; exit 1"],
                &[("CALLS".into(), calls.to_str().unwrap().into())],
            )
            .await
            .is_err()
    );
    assert_eq!(std::fs::read_to_string(calls).unwrap(), "called\n");
}

#[tokio::test]
async fn a_failed_probe_says_why() {
    let fx = fixture();
    let probe = async |program: &str, script: &str| {
        fx.host
            .probe(Path::new(program), &["-c", script], &[])
            .await
            .map_err(|e| e.to_string())
    };
    assert_eq!(
        probe(
            "/bin/sh",
            "echo ignored >&2; echo 'error: no config' >&2; exit 3"
        )
        .await,
        Err("exit status: 3: error: no config".into())
    );
    assert_eq!(
        probe("/bin/sh", "kill -9 $$").await,
        Err("signal: 9 (SIGKILL)".into())
    );
    assert_eq!(
        probe("/bin/sh", "true").await,
        Err("printed nothing".into())
    );
    assert_eq!(
        common::until(
            "the provider version while a background child is running",
            probe("/bin/sh", "sleep 300 >/dev/null & echo provider-version"),
        )
        .await,
        Ok("provider-version".into())
    );
    let missing = probe("/nonexistent/grok", "true").await.unwrap_err();
    assert!(missing.starts_with("could not start ("), "{missing}");
}

#[tokio::test]
async fn startup_config_checks_skip_only_clis_without_the_required_flags() {
    let fx = fixture();
    for (name, strict, startup, supported) in [
        ("current", "--strict-config", "--print-startup-config", true),
        ("old-root", "--config", "--print-startup-config", false),
        ("old-server", "--strict-config", "--listen", false),
    ] {
        let codex = fx.data.join(name);
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  --help) echo '{strict}' ;;\n  'app-server --help') echo '{startup}' ;;\n  *) exit 1 ;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            codex_supports_startup_config(&fx.host, &codex).await,
            supported
        );
    }
}

#[tokio::test]
#[should_panic(expected = "/usr/bin/false --help: exit status: 1")]
async fn a_broken_codex_cli_does_not_skip_startup_config_checks() {
    let fx = fixture();
    codex_supports_startup_config(&fx.host, Path::new("/usr/bin/false")).await;
}
