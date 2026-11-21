use crate::common;

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, harness};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::codex_home;

mod codex;
mod grants;
mod probes;
mod processes;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use else_api::providers::host::Supervised;
use else_api::providers::host::{Host, LaunchSpec, MARKER_VAR, Run};
use serde_json::json;
use uuid::Uuid;

struct Fixture {
    _dir: tempfile::TempDir,
    data: PathBuf,
    bots: PathBuf,
    host: Host,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let data = root.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let bots = root.join("bots");
    let host = Host::new(&data, &bots);
    Fixture {
        _dir: dir,
        data,
        bots,
        host,
    }
}

fn shell(fx: &Fixture, bot: Uuid, script: &str) -> LaunchSpec {
    LaunchSpec {
        turn_id: Uuid::new_v4(),
        dirs: fx.host.bot_dirs(bot, "test").unwrap(),
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".into(), script.into()],
        env: vec![("PROVIDER_SETTING".into(), "allowed".into())],
    }
}

async fn output(fx: &Fixture, spec: &LaunchSpec) -> String {
    let out = fx.host.command(spec).output().await.unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

async fn codex_supports_startup_config(host: &Host, codex: &Path) -> bool {
    for (args, flag) in [
        (&["--help"][..], "--strict-config"),
        (&["app-server", "--help"][..], "--print-startup-config"),
    ] {
        let help = host
            .probe(codex, args, &[])
            .await
            .unwrap_or_else(|why| panic!("{} {}: {why}", codex.display(), args.join(" ")));
        if !help.split_whitespace().any(|word| word == flag) {
            eprintln!(
                "SKIPPED: {} does not support {flag}; install a Codex CLI with startup-config validation",
                codex.display()
            );
            return false;
        }
    }
    true
}
