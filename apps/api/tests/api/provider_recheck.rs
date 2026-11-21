use crate::common;

use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use common::TestApp;
use common::recorder::{self, GATE, tool};
use else_api::providers::{claude::Claude, claude_home::HostConfig, host::Host};
use serde_json::{Value, json};
use uuid::Uuid;

/// Logged in only while `logged-in` exists beside it, and broken while
/// `broken` does. A turn leaves `ran` behind and fails.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
here=$(dirname "$0")
case "$1" in
--version)
    [ -e "$here/broken" ] && exit 3
    echo "2.1.999 (Claude Code)"
    ;;
auth)
    if [ -e "$here/logged-in" ]; then echo '{"loggedIn":true}'; else echo '{"loggedIn":false}'; exit 1; fi
    ;;
*)
    touch "$here/ran"
    exit 1
    ;;
esac
"#;

const SIGNED_OUT: &str = "Claude Code is not logged in on the host (run `claude` and log in).";

struct Fixture {
    _dir: tempfile::TempDir,
    fake: PathBuf,
    app: TestApp,
}

/// A Claude whose host login is signed out until `logged-in` exists in the
/// returned directory.
async fn signed_out_claude() -> (tempfile::TempDir, PathBuf, Claude) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let fake = root.join("fake");
    std::fs::create_dir(&fake).unwrap();
    let program = fake.join("claude");
    std::fs::write(&program, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = HostConfig {
        dir: root.join("config"),
        explicit: true,
    };
    let host = Host::new(&root.join("data"), &root.join("projects"));
    let claude = Claude::detect(Some(host), Some(program), Some(config), String::new()).await;
    (dir, fake, claude)
}

async fn fixture() -> Fixture {
    let (dir, fake, claude) = signed_out_claude().await;
    let app = TestApp::builder()
        .providers(vec![Arc::new(claude)])
        .start()
        .await;
    let saved = app
        .owner(
            "PUT",
            "/v1/settings/defaults",
            Some(json!({
                "provider_id": "claude",
                "model": "claude-opus-5-5",
                "reasoning_effort": "low",
                "service_tier": "default",
            })),
        )
        .await;
    assert!(saved.status.is_success(), "{}", saved.body);
    Fixture {
        _dir: dir,
        fake,
        app,
    }
}

async fn recheck(app: &TestApp) -> Value {
    let reply = app
        .owner("POST", "/v1/settings/providers/claude/recheck", None)
        .await;
    assert!(reply.status.is_success(), "{}", reply.body);
    reply.body
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_recheck_shows_a_host_login_without_a_restart() {
    let fx = fixture().await;
    let still_out = recheck(&fx.app).await;
    assert_eq!(still_out["cli_available"], false);
    assert_eq!(still_out["cli_unavailable_reason"], SIGNED_OUT);

    std::fs::write(fx.fake.join("logged-in"), "").unwrap();
    let caps = fx.app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(
        caps.body["providers"][0]["cli_available"], false,
        "nothing checks again on its own within the hour"
    );
    let back = recheck(&fx.app).await;
    assert_eq!(back["id"], "claude");
    assert_eq!(back["cli_available"], true, "{back}");
    assert_eq!(back["available"], true);
    let caps = fx.app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(caps.body["providers"][0]["cli_available"], true);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_message_refused_while_signed_out_is_accepted_once_when_sent_again_after_login() {
    let fx = fixture().await;
    let bot = fx.app.main_bot().await;
    let request = Uuid::new_v4();

    let refused = fx.app.send(&bot, request, "hello", None).await;
    assert_eq!(refused.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.body["error"]["code"], "provider_unavailable");
    assert_eq!(refused.body["error"]["message"], SIGNED_OUT);

    std::fs::write(fx.fake.join("broken"), "").unwrap();
    let again = fx.app.send(&bot, request, "hello", None).await;
    assert_eq!(again.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(again.body["error"]["code"], "provider_unavailable");
    let reason = again.body["error"]["message"].as_str().unwrap();
    assert!(
        reason.starts_with("Claude Code does not run"),
        "the retry reports the CLI as it is now: {reason}"
    );

    std::fs::remove_file(fx.fake.join("broken")).unwrap();
    std::fs::write(fx.fake.join("logged-in"), "").unwrap();
    let (first, second) = tokio::join!(
        fx.app.send(&bot, request, "hello", None),
        fx.app.send(&bot, request, "hello", None),
    );
    assert!(first.status.is_success(), "{}", first.body);
    assert!(second.status.is_success(), "{}", second.body);
    assert_eq!(first.body["turn"]["id"], second.body["turn"]["id"]);
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE request_id = ?1")
        .bind(request)
        .fetch_one(&fx.app.db.pool)
        .await
        .unwrap();
    assert_eq!(turns, 1, "two racing retries made one turn");
}

/// A routine on Claude and when it is next due, created from a recorder
/// turn that is returned still held.
async fn claude_routine(h: &recorder::Harness, bot: &str) -> (Uuid, Uuid, DateTime<Utc>) {
    let (held, token) = h.running(bot, GATE).await;
    let created = tool(
        &h.app,
        &token,
        "routine_create",
        json!({ "name": "Digest", "prompt": "Summarize my unread mail.", "cron": "0 9 1 1 *" }),
    )
    .await;
    assert!(!created.error, "{}", created.body);
    let routine = &created.result["routine"];
    let id: Uuid = routine["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE routines SET model = ?2 WHERE id = ?1")
        .bind(id)
        .bind(
            json!({ "provider_id": "claude", "model": "claude-opus-5-5",
                    "reasoning_effort": "low", "service_tier": "default" })
            .to_string(),
        )
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    let due = DateTime::parse_from_rfc3339(routine["next_run_at"].as_str().unwrap())
        .unwrap()
        .to_utc();
    (id, held, due)
}

/// Admit the routine's due run: its turn and provider, or why it skipped.
async fn fire(
    app: &TestApp,
    routine: Uuid,
    due: DateTime<Utc>,
) -> (Option<Uuid>, Option<String>, Option<String>) {
    else_api::db::routines::admit_due(&app.db.pool, &app.state.registry, None, due, 8)
        .await
        .unwrap();
    app.scheduler.wake();
    sqlx::query_as(
        "SELECT rr.turn_id, t.provider_id, rr.skip_reason
         FROM routine_runs rr LEFT JOIN turns t ON t.id = rr.turn_id WHERE rr.routine_id = ?1",
    )
    .bind(routine)
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_routine_on_claude_runs_after_a_host_login_that_nothing_rechecked() {
    let (_dir, fake, claude) = signed_out_claude().await;
    let h = recorder::harness(TestApp::builder().providers(vec![Arc::new(claude)])).await;
    let bot = h.app.main_bot().await;
    let (routine, held, due) = claude_routine(&h, &bot).await;
    h.release(held).await;
    h.app.wait_turn(held, &["completed"]).await;

    std::fs::write(fake.join("logged-in"), "").unwrap();
    let (turn, provider, skipped) = fire(&h.app, routine, due).await;
    assert_eq!(skipped, None);
    assert!(turn.is_some());
    assert_eq!(provider.as_deref(), Some("claude"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_queued_claude_turn_runs_after_a_host_login_that_nothing_rechecked() {
    let (_dir, fake, claude) = signed_out_claude().await;
    let h = recorder::harness(TestApp::builder().providers(vec![Arc::new(claude)])).await;
    std::fs::write(fake.join("logged-in"), "").unwrap();
    assert_eq!(recheck(&h.app).await["cli_available"], true);
    let bot = h.app.main_bot().await;
    let (routine, held, due) = claude_routine(&h, &bot).await;
    let (turn, _, _) = fire(&h.app, routine, due).await;
    let turn = turn.expect("the run queued a turn behind the held one");

    std::fs::remove_file(fake.join("logged-in")).unwrap();
    assert_eq!(recheck(&h.app).await["cli_available"], false);
    std::fs::write(fake.join("logged-in"), "").unwrap();
    h.release(held).await;
    h.app.wait_turn(held, &["completed"]).await;
    let ended = h
        .app
        .wait_turn(turn, &["completed", "failed", "uncertain"])
        .await;
    let code: Option<String> = sqlx::query_scalar("SELECT error_code FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_ne!(code.as_deref(), Some("provider_unavailable"), "{ended}");
    assert!(fake.join("ran").exists(), "the queued turn launched Claude");
}
