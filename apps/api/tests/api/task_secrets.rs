use crate::common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use common::guest::{Exec, Guest, Reply, ok};
use common::recorder::{GATE, Harness, harness, tool};
use else_api::computers::{ComputerConfig, ComputerManager, ExecOutput};
use serde_json::{Value, json};
use tokio::sync::Notify;
use uuid::Uuid;

mod guest;
mod secrets;
mod stops;
mod vault;

const PASSWORD: &str = "s3cr3t-Pa55word";

/// What the fake guest's foreground commands and kills wait on.
#[derive(Default)]
struct Script {
    killed: Mutex<HashMap<String, Arc<Notify>>>,
    started: Notify,
    stalled: Mutex<Vec<String>>,
    kill_stalled: Notify,
    release_stalled_kill: Notify,
    fail_desktop: AtomicBool,
}

impl Script {
    fn killed(&self, name: &str) -> Arc<Notify> {
        self.killed
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_default()
            .clone()
    }

    async fn reply(&self, e: Exec) -> Option<Reply> {
        let script = e.script();
        if script == else_api::computers::GUEST_DESKTOP && self.fail_desktop.load(SeqCst) {
            return Some(Ok(ExecOutput {
                status: Some(1),
                stdout: String::new(),
                stderr: "Xvfb did not start".into(),
                truncated: false,
            }));
        }
        let browser = script.contains("else-browser");
        let reply = if script.contains("wait $pg; c=$?") {
            if e.arg(2).is_some_and(|c| c.contains("HANG")) {
                self.started.notify_one();
                self.killed(&e.name).notified().await;
                ok("143\n0\n0\n\n\0\n")
            } else {
                ok(run_reply(&e.input))
            }
        } else if script.contains("alive()") {
            if self.stalled.lock().unwrap().contains(&e.name) {
                self.kill_stalled.notify_one();
                self.release_stalled_kill.notified().await;
            }
            self.killed(&e.name).notify_one();
            ok("stopped")
        } else if browser && e.has_arg("type-secret") {
            let typed = String::from_utf8_lossy(&e.input);
            ok(json!({ "url": "https://x.com/login", "title": "Log in",
                    "value": format!("({} characters)", typed.len()),
                    "added": [format!("Signed in as {typed}")] })
            .to_string())
        } else if browser && e.has_arg("#echoes") {
            ok(
                json!({ "error": format!("no element matches; similar: \"{PASSWORD}\"") })
                    .to_string(),
            )
        } else if browser && e.has_arg("text") {
            ok(json!({ "url": "https://mobile.x.com/home", "title": "Home",
                    "text": format!("Signed in as me@example.com with {PASSWORD}") })
            .to_string())
        } else if browser && e.has_arg("snapshot") {
            ok(json!({ "url": "https://example.com/", "title": "Projects",
                    "snapshot": "heading \"Projects\"\nbutton \"Delete project\" @kqvzb5" })
            .to_string())
        } else if script.contains("echo launched")
            && e.arg(3).is_some_and(|c| c.contains("LAUNCH_ERROR"))
        {
            Ok(ExecOutput {
                status: Some(2),
                stdout: String::new(),
                stderr: format!("sh: launch failed: {}", run_reply(&e.input)),
                truncated: false,
            })
        } else {
            return None;
        };
        Some(reply)
    }
}

/// The framed output of a foreground command that prints the last task
/// secret on its input as `NAME=value`.
fn run_reply(input: &[u8]) -> String {
    use base64::Engine;
    let mut out = String::from("no secret");
    for line in String::from_utf8_lossy(input).lines() {
        if let Some((name, value)) = line.split_once(' ') {
            let value = base64::engine::general_purpose::STANDARD
                .decode(value)
                .unwrap();
            out = format!("{name}={}", String::from_utf8(value).unwrap());
        }
    }
    format!("0\n{}\n0\n{out}\n\0\n", out.len())
}

struct Setup {
    h: Harness,
    guest: Arc<Guest>,
    script: Arc<Script>,
    manager: Arc<ComputerManager>,
}

async fn setup() -> Setup {
    let db = Arc::new(common::TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let script = Arc::new(Script::default());
    let replies = script.clone();
    let guest = Arc::new(Guest::new().on_exec(move |e| {
        let replies = replies.clone();
        async move { replies.reply(e).await }
    }));
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        guest.clone(),
        ComputerConfig {
            desktop: true,
            idle_after: Duration::from_secs(3600),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let h = harness(TestApp::builder().db(db).computers(manager.clone())).await;
    Setup {
        h,
        guest,
        script,
        manager,
    }
}

/// Stop the bot's work, so the next message starts a turn of its own.
async fn stop(app: &TestApp, bot: &str, turn: Uuid) {
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{:?}", stop.body);
    app.wait_turn(turn, &["interrupted", "completed", "failed", "uncertain"])
        .await;
}
