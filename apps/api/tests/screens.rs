//! POC-028: the screenshot tool's outcomes against SQLite, the real MCP
//! endpoint, and a fake runtime standing in for the guest capture (the live
//! vision checks are in `vision_live.rs`).

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use base64::Engine;
use common::TestApp;
use else_api::computers::{
    ComputerConfig, ComputerManager, ComputerRuntime, ExecOutput, RuntimeError, RuntimeState, Spec,
};
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use sha2::Digest;
use uuid::Uuid;

/// A 1x1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ok,
    NoDisplay,
    TooLarge,
    /// Reports a different image than it writes.
    Mismatch,
}

struct Guest {
    state: Mutex<std::collections::HashMap<String, RuntimeState>>,
    mode: Mutex<Mode>,
    files: Mutex<Vec<String>>,
    /// Input batches the guest received, decoded.
    inputs: Mutex<Vec<Value>>,
    /// How the guest input helper ends: (exit code, stdout lines).
    input_reply: Mutex<(i32, Vec<String>)>,
}

fn png() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(PNG)
        .unwrap()
}

#[async_trait]
impl ComputerRuntime for Guest {
    async fn list(&self) -> Result<Vec<(String, RuntimeState)>, RuntimeError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .iter()
            .map(|(n, s)| (n.clone(), *s))
            .collect())
    }
    async fn create(&self, spec: &Spec) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap()
            .insert(spec.name.clone(), RuntimeState::Running);
        Ok(())
    }
    async fn start(&self, name: &str) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Running);
        Ok(())
    }
    async fn stop(&self, name: &str) -> Result<(), RuntimeError> {
        self.state
            .lock()
            .unwrap()
            .insert(name.into(), RuntimeState::Stopped);
        Ok(())
    }
    async fn pause(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn resume(&self, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn copy_out(
        &self,
        _: &str,
        guest: &str,
        host: &std::path::Path,
    ) -> Result<(), RuntimeError> {
        self.files.lock().unwrap().push(guest.to_owned());
        std::fs::write(host, png()).map_err(|e| RuntimeError::Unavailable(e.to_string()))
    }
    async fn exec_input(
        &self,
        _: &str,
        _: &str,
        args: &[&str],
        input: &[u8],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        assert!(
            args.iter().any(|a| a.contains("/usr/local/bin/else-input")),
            "only input batches take stdin here"
        );
        assert!(
            args.iter().all(|a| !a.contains("actions")),
            "the batch is never an argument"
        );
        let line = String::from_utf8_lossy(input).into_owned();
        let encoded = line
            .lines()
            .find_map(|l| l.strip_prefix("ELSE_INPUT_BATCH "))
            .expect("the batch arrives as ELSE_INPUT_BATCH on stdin");
        let batch: Value = serde_json::from_slice(
            &base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .unwrap(),
        )
        .unwrap();
        self.inputs.lock().unwrap().push(batch);
        let (code, lines) = self.input_reply.lock().unwrap().clone();
        let out = lines.join("\n");
        // The RUN wrapper's reply: code, sizes, stdout, NUL line, stderr.
        Ok(ExecOutput {
            status: Some(0),
            stdout: format!("{code}\n{}\n0\n{out}\n\0\n", out.len()),
            stderr: String::new(),
            truncated: false,
        })
    }
    async fn exec(
        &self,
        _: &str,
        _: &str,
        args: &[&str],
        _: Duration,
    ) -> Result<ExecOutput, RuntimeError> {
        let script = args.iter().find(|a| a.contains("else-screenshot")).copied();
        let stdout = match (script, *self.mode.lock().unwrap()) {
            (None, _) => String::new(),
            (Some(_), Mode::NoDisplay) => {
                r#"{"error": "DISPLAY_UNAVAILABLE: the desktop display is not running"}"#.into()
            }
            (Some(_), Mode::TooLarge) => {
                r#"{"error": "IMAGE_TOO_LARGE: the screenshot is over 1500000 bytes even at 1/4 size"}"#.into()
            }
            (Some(_), mode) => {
                let sha = hex::encode(sha2::Sha256::digest(png()));
                let w = if mode == Mode::Mismatch { 2 } else { 1 };
                json!({ "screen_width": 1280, "screen_height": 800, "image_width": w,
                        "image_height": 1, "scale": 1, "pointer": [640, 400],
                        "bytes": png().len(), "sha256": sha, "captured_at": 1.0 })
                .to_string()
            }
        };
        Ok(ExecOutput {
            status: Some(0),
            stdout,
            stderr: String::new(),
            truncated: false,
        })
    }
}

struct Setup {
    app: Arc<TestApp>,
    guest: Arc<Guest>,
    manager: Arc<ComputerManager>,
    _dir: tempfile::TempDir,
}

async fn setup(desktop: bool) -> Setup {
    let db = Arc::new(common::TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let guest = Arc::new(Guest {
        state: Mutex::default(),
        mode: Mutex::new(Mode::Ok),
        files: Mutex::default(),
        inputs: Mutex::default(),
        input_reply: Mutex::new((
            0,
            vec![
                r#"{"started": true}"#.into(),
                r#"{"done": 0}"#.into(),
                r#"{"done": 1}"#.into(),
                r#"{"ok": true, "actions": 2}"#.into(),
            ],
        )),
    });
    let manager = Arc::new(ComputerManager::new(
        db.pool.clone(),
        guest.clone(),
        ComputerConfig {
            desktop,
            idle_after: Duration::from_secs(3600),
            state_dir: dir.path().to_owned(),
            ..ComputerConfig::default()
        },
    ));
    manager.reconcile_startup().await.unwrap();
    let registry = else_api::providers::Registry::new(vec![Arc::new(
        else_api::providers::synthetic::Synthetic::new(Duration::from_millis(5)),
    )]);
    let app = Arc::new(TestApp::with_parts(db, 4, registry, Some(manager.clone())));
    Setup {
        app,
        guest,
        manager,
        _dir: dir,
    }
}

/// A running turn of `bot` whose profile names `model`, and its grant.
async fn grant(app: &TestApp, bot: &str, model: Option<&str>) -> String {
    let reply = app
        .send(bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    if let Some(model) = model {
        sqlx::query(
            "UPDATE turns SET profile = json_object('model', ?2), profile_revision = 1
             WHERE id = ?1",
        )
        .bind(turn)
        .bind(model)
        .execute(&app.db.pool)
        .await
        .unwrap();
    }
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.scheduler
        .grants()
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![Tool::Computer],
            DEFAULT_TTL,
        )
        .await
}

async fn tool(app: &TestApp, token: &str, name: &str, arguments: Value) -> Value {
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": name, "arguments": arguments } })),
            Some(token),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    reply.body["result"].clone()
}

async fn shot(app: &TestApp, token: &str, arguments: Value) -> Value {
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "computer_screenshot", "arguments": arguments } })),
            Some(token),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    reply.body["result"].clone()
}

fn error(result: &Value) -> String {
    assert_eq!(result["isError"], true, "{result}");
    result["content"][0]["text"].as_str().unwrap().to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn screenshots_reach_the_model_as_images_with_explicit_refusals() {
    let Setup {
        app,
        guest,
        manager,
        ..
    } = setup(true).await;
    let bot = app.create_bot("Seer", "looks").await;
    let other = app.create_bot("Other", "elsewhere").await;

    // A model not verified for images is refused, and nothing is captured.
    let token = grant(&app, &bot, Some("gpt-5.4-mini")).await;
    let refused = error(&shot(&app, &token, json!({})).await);
    assert!(refused.starts_with("IMAGE_UNSUPPORTED"), "{refused}");
    assert!(guest.files.lock().unwrap().is_empty());

    // A verified model gets the image as an image block, with metadata.
    sqlx::query(
        "UPDATE turns SET profile = json_object('model', 'gpt-6.1-sol')
         WHERE bot_id = ?1 AND status = 'running'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .execute(&app.db.pool)
    .await
    .unwrap();
    let result = shot(&app, &token, json!({ "save": true })).await;
    assert_ne!(result["isError"], true, "{result}");
    let content = result["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    let meta: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        (meta["screen_width"].clone(), meta["screen_height"].clone()),
        (json!(1280), json!(800))
    );
    assert_eq!(meta["pointer"], json!([640, 400]));
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["mimeType"], "image/png");
    assert_eq!(content[1]["data"], PNG);
    let artifact = meta["artifact_id"].as_str().unwrap().to_owned();
    // What left the guest is root's checked copy, not the user's file.
    assert!(guest.files.lock().unwrap()[0].starts_with("/var/lib/elsebot-screens/"));
    // Memory records metadata only, never the image.
    let logged: Vec<String> = sqlx::query_scalar(
        "SELECT body FROM memory_entry WHERE bot_id = ?1 AND kind = 'tool_activity'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert!(
        logged
            .iter()
            .any(|l| l.contains("screenshot") && l.contains("1280x800"))
    );
    assert!(logged.iter().all(|l| !l.contains(&PNG[..40])), "{logged:?}");

    // The saved screenshot can be shown again; another bot cannot see it.
    let again = shot(&app, &token, json!({ "artifact_id": artifact })).await;
    assert_eq!(again["content"][1]["data"], PNG);
    // With the original capture's coordinate mapping.
    let saved: Value = serde_json::from_str(again["content"][0]["text"].as_str().unwrap()).unwrap();
    for key in [
        "screen_width",
        "screen_height",
        "scale",
        "pointer",
        "capture_id",
        "captured_at",
    ] {
        assert_eq!(saved[key], meta[key], "{key}");
    }
    // A capture for a turn that no longer runs takes nothing.
    let lease = match manager
        .acquire(bot.parse().unwrap(), "test:late")
        .await
        .unwrap()
    {
        else_api::computers::Acquire::Ready { lease_id, .. } => lease_id,
        other => panic!("{other:?}"),
    };
    let before = guest.files.lock().unwrap().len();
    let late = manager
        .capture_screen(lease, bot.parse().unwrap(), Some(Uuid::new_v4()), true)
        .await
        .unwrap();
    assert!(late.is_err(), "a stopped turn captures nothing");
    assert_eq!(guest.files.lock().unwrap().len(), before);
    manager.release(lease).await.unwrap();
    let other_token = grant(&app, &other, Some("gpt-6.1-sol")).await;
    let foreign = error(&shot(&app, &other_token, json!({ "artifact_id": artifact })).await);
    assert!(foreign.starts_with("No such screenshot"), "{foreign}");

    // Explicit outcomes for a missing display, an image over the budget,
    // and a copy that does not match the capture.
    *guest.mode.lock().unwrap() = Mode::NoDisplay;
    assert!(error(&shot(&app, &token, json!({})).await).starts_with("DISPLAY_UNAVAILABLE"));
    *guest.mode.lock().unwrap() = Mode::TooLarge;
    assert!(error(&shot(&app, &token, json!({})).await).starts_with("IMAGE_TOO_LARGE"));
    *guest.mode.lock().unwrap() = Mode::Mismatch;
    assert!(error(&shot(&app, &token, json!({})).await).starts_with("SCREENSHOT_INVALID"));
    *guest.mode.lock().unwrap() = Mode::Ok;

    // An owner-stopped computer is neither started nor captured.
    manager.owner_stop(bot.parse().unwrap()).await.unwrap();
    let before = guest.files.lock().unwrap().len();
    assert!(error(&shot(&app, &token, json!({})).await).starts_with("COMPUTER_STOPPED"));
    assert_eq!(guest.files.lock().unwrap().len(), before);

    // A revoked grant gets no capture at all.
    app.scheduler
        .grants()
        .revoke_turn(
            sqlx::query_scalar::<_, Uuid>(
                "SELECT id FROM turns WHERE bot_id = ?1 AND status = 'running' LIMIT 1",
            )
            .bind(other.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
            .await
            .unwrap(),
        )
        .await;
    let revoked = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "computer_screenshot", "arguments": {} } })),
            Some(&other_token),
        )
        .await;
    assert_eq!(
        revoked.status,
        StatusCode::UNAUTHORIZED,
        "{:?}",
        revoked.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn computers_without_a_desktop_say_so() {
    let Setup { app, .. } = setup(false).await;
    let bot = app.create_bot("Headless", "no desktop").await;
    let token = grant(&app, &bot, Some("gpt-6.1-sol")).await;
    let refused = error(&shot(&app, &token, json!({})).await);
    assert!(refused.contains("no desktop"), "{refused}");
}

/// POC-029: the input tool's outcomes. The guest helper validates and acts
/// (checked live); here: what reaches it, typed text kept out of memory, the
/// owner's open desktop, the per-turn bound, and uncertain or refused
/// batches reported without replay.
#[tokio::test(flavor = "multi_thread")]
async fn input_batches_are_bounded_reported_and_never_logged_verbatim() {
    let Setup {
        app,
        guest,
        manager,
        ..
    } = setup(true).await;
    let bot = app.create_bot("Hands", "clicks").await;
    let token = grant(&app, &bot, Some("gpt-6.1-sol")).await;
    let batch = json!({ "frame": [1280, 800],
        "actions": [{ "click": [100, 200] }, { "type": "hunter2-password" }] });
    let done = tool(&app, &token, "computer_input", batch.clone()).await;
    assert_ne!(done["isError"], true, "{done}");
    let result: Value = serde_json::from_str(done["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        (result["done"].clone(), result["of"].clone()),
        (json!(2), json!(2))
    );
    assert_eq!(
        guest.inputs.lock().unwrap()[0],
        batch,
        "the batch reached the guest as sent"
    );
    let logged: Vec<String> = sqlx::query_scalar(
        "SELECT body FROM memory_entry WHERE bot_id = ?1 AND kind = 'tool_activity'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert!(
        logged
            .iter()
            .any(|l| l.contains("click([100,200])") && l.contains("type(16 chars)")),
        "{logged:?}"
    );
    assert!(
        logged.iter().all(|l| !l.contains("hunter2")),
        "typed text never enters memory"
    );

    // A batch the helper refused did nothing; a cut-off one is uncertain.
    *guest.input_reply.lock().unwrap() = (2, vec![r#"{"error": "SCREEN_CHANGED: the screen is now 1024x768, not 1280x800; take a new screenshot"}"#.into()]);
    let refused = tool(&app, &token, "computer_input", batch.clone()).await;
    let text = refused["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("SCREEN_CHANGED") && text.ends_with("Nothing was done."),
        "{text}"
    );
    *guest.input_reply.lock().unwrap() = (
        143,
        vec![
            r#"{"started": true}"#.into(),
            r#"{"done": 0}"#.into(),
            r#"{"error": "STOPPED: the input was stopped; what is listed as done happened"}"#
                .into(),
        ],
    );
    let cut = tool(&app, &token, "computer_input", batch.clone()).await;
    let text = cut["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("UNCERTAIN: 1 of 2"), "{text}");
    // An error after execution started (say a failure on action 2) is never
    // reported as nothing done.
    *guest.input_reply.lock().unwrap() = (
        5,
        vec![
            r#"{"started": true}"#.into(),
            r#"{"done": 0}"#.into(),
            r#"{"error": "INVALID_TEXT: no spare keycode to type a character"}"#.into(),
        ],
    );
    let late = tool(&app, &token, "computer_input", batch.clone()).await;
    let text = late["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("UNCERTAIN: 1 of 2"), "{text}");
    let sent = guest.inputs.lock().unwrap().len();
    assert_eq!(sent, 4, "nothing was replayed");

    // The owner's open desktop session has the desktop.
    *guest.input_reply.lock().unwrap() = (
        0,
        vec![
            r#"{"started": true}"#.into(),
            r#"{"done": 0}"#.into(),
            r#"{"done": 1}"#.into(),
            r#"{"ok": true, "actions": 2}"#.into(),
        ],
    );
    let desktop = manager.desktop(bot.parse().unwrap()).await.unwrap();
    assert!(desktop.is_ok());
    let busy = tool(&app, &token, "computer_input", batch.clone()).await;
    assert!(
        busy["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("OWNER_USING_DESKTOP")
    );
    assert_eq!(
        guest.inputs.lock().unwrap().len(),
        sent,
        "nothing sent while the owner is in"
    );
    manager.close_desktop(bot.parse().unwrap()).await.unwrap();

    // A bounded number of batches per turn.
    let mut limited = None;
    for _ in 0..60 {
        let r = tool(&app, &token, "computer_input", batch.clone()).await;
        if r["isError"] == true {
            limited = Some(r["content"][0]["text"].as_str().unwrap().to_owned());
            break;
        }
    }
    assert!(
        limited
            .as_deref()
            .is_some_and(|t| t.starts_with("INPUT_LIMIT")),
        "{limited:?}"
    );
    assert_eq!(
        guest.inputs.lock().unwrap().len(),
        39,
        "40 batches counted per turn (one refused while the owner was in never reached the guest)"
    );
}
