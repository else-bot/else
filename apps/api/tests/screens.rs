//! POC-028: the screenshot tool's outcomes against PostgreSQL, the real MCP
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
            "UPDATE turns SET profile = jsonb_build_object('model', $2::text), profile_revision = 1
             WHERE id = $1",
        )
        .bind(turn)
        .bind(model)
        .execute(&app.db.pool)
        .await
        .unwrap();
    }
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = $1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    app.scheduler.grants().issue(
        bot.parse().unwrap(),
        conversation,
        turn,
        vec![Tool::Computer],
        DEFAULT_TTL,
    )
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
        "UPDATE turns SET profile = jsonb_build_object('model', 'gpt-6.1-sol')
         WHERE bot_id = $1 AND status = 'running'",
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
    // The guest file was copied under /var/lib/elsebot/screens.
    assert!(guest.files.lock().unwrap()[0].starts_with("/var/lib/elsebot/screens/"));
    // Memory records metadata only, never the image.
    let logged: Vec<String> = sqlx::query_scalar(
        "SELECT body FROM memory_entry WHERE bot_id = $1 AND kind = 'tool_activity'",
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
    app.scheduler.grants().revoke_turn(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM turns WHERE bot_id = $1 AND status = 'running' LIMIT 1",
        )
        .bind(other.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap(),
    );
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
