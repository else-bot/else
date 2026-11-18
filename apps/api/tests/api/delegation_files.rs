use crate::common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TestApp, TestDb};
use else_api::app::providers::{
    Availability, Profile, Provider, Stop, TurnInput, TurnOutcome, TurnSink,
};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The synthetic provider with a project directory per bot under `root`,
/// recording every input it runs.
struct WithProject {
    inner: Synthetic,
    root: PathBuf,
    inputs: Mutex<Vec<TurnInput>>,
}

#[async_trait::async_trait]
impl Provider for WithProject {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn label(&self) -> &'static str {
        self.inner.label()
    }
    fn availability(&self) -> Availability {
        self.inner.availability()
    }
    fn uses_profile(&self) -> bool {
        self.inner.uses_profile()
    }
    fn validate_profile(&self, profile: &Profile) -> Result<(), String> {
        self.inner.validate_profile(profile)
    }
    fn project_dir(&self, bot: Uuid) -> Option<PathBuf> {
        Some(self.root.join(bot.to_string()))
    }
    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome {
        self.inputs.lock().unwrap().push(input.clone());
        self.inner.run(input, sink, stop).await
    }
}

const SLOW: &str = "[synthetic:slow] Ask Twitter to post the approved whale card.";

fn png() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(3, 2, image::Rgb([200, 40, 90]))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

async fn delegate(app: &TestApp, token: &str, arguments: Value) -> (bool, Value) {
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "delegate", "arguments": arguments } })),
            Some(token),
        )
        .await;
    let text = reply.body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    (
        reply.body["result"]["isError"] == true,
        serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
    )
}

struct Setup {
    _root: tempfile::TempDir,
    provider: Arc<WithProject>,
    app: TestApp,
    main: String,
    twitter: String,
    card: Vec<u8>,
    parent: Uuid,
    token: String,
}

/// The main bot with a card in its project, mid-request with a delegate
/// grant, and a Twitter bot with a receipt in its own.
async fn setup() -> Setup {
    let root = tempfile::tempdir().unwrap();
    let provider = Arc::new(WithProject {
        inner: Synthetic::new(Duration::from_millis(5)),
        root: root.path().to_owned(),
        inputs: Mutex::new(Vec::new()),
    });
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    sqlx::query("UPDATE installation SET default_provider_id = 'synthetic'")
        .execute(&db.pool)
        .await
        .unwrap();
    let app = TestApp::with_registry(
        Arc::new(db),
        2,
        else_api::providers::Registry::new(vec![provider.clone()]),
    );
    let main = app.main_bot().await;
    let twitter = app.create_bot("Twitter").await;
    let card = png();
    std::fs::create_dir_all(root.path().join(&main).join("cards")).unwrap();
    std::fs::write(root.path().join(&main).join("cards/whale.png"), &card).unwrap();
    std::fs::write(root.path().join(&main).join("private.txt"), b"keep out").unwrap();
    std::fs::create_dir_all(root.path().join(&twitter).join("out")).unwrap();
    std::fs::write(
        root.path().join(&twitter).join("out/receipt.txt"),
        b"posted 1842",
    )
    .unwrap();

    let sent = app.send(&main, Uuid::new_v4(), SLOW, None).await;
    let parent: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&parent.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(parent)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let token = app
        .scheduler
        .grants()
        .issue(
            main.parse().unwrap(),
            conversation,
            parent,
            vec![Tool::Delegate],
            DEFAULT_TTL,
        )
        .await;

    Setup {
        _root: root,
        provider,
        app,
        main,
        twitter,
        card,
        parent,
        token,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_delegated_file_reaches_the_other_bot_and_its_reply_files_come_back() {
    let Setup {
        _root,
        provider,
        app,
        main,
        twitter,
        card,
        parent,
        token,
    } = setup().await;
    let task = "Post the attached card, then reply with [receipt](out/receipt.txt).";
    for (path, reason) in [
        ("../../escape.png", "No regular file"),
        ("cards/missing.png", "No regular file"),
    ] {
        let (error, message) = delegate(
            &app,
            &token,
            json!({ "target_bot_id": twitter, "task": task, "files": [path] }),
        )
        .await;
        let message = message.as_str().unwrap_or_default().to_owned();
        assert!(
            error && message.starts_with("FILE_UNAVAILABLE"),
            "{message}"
        );
        assert!(message.contains(reason), "{message}");
    }
    let (error, pending) = delegate(
        &app,
        &token,
        json!({ "target_bot_id": twitter, "task": task, "files": ["cards/whale.png"] }),
    )
    .await;
    assert!(!error, "{pending}");
    assert_eq!(pending["status"], "pending", "{pending}");
    let digest = hex::encode(Sha256::digest(&card));
    assert_eq!(
        pending["files"],
        json!([{ "name": "whale.png", "media_type": "image/png",
                 "size": card.len(), "sha256": digest }]),
    );

    app.wait_turn_for(&parent.to_string(), &["completed"], Duration::from_secs(40))
        .await;
    let inputs = provider.inputs.lock().unwrap().clone();
    let child = inputs
        .iter()
        .find(|input| input.bot_id.to_string() == twitter)
        .expect("the task ran");
    assert_eq!(child.files.len(), 1, "only the named file is sent");
    assert_eq!(child.files[0].attachment.name, "whale.png");
    assert_eq!(*child.files[0].data, *card, "exact bytes");
    let request = else_api::providers::prompt::request_text(child);
    assert!(
        request.contains(task)
            && request.contains("whale.png (image/png")
            && request.contains("saved at .attachments/"),
        "{request}"
    );
    let owners: Vec<(String, String)> = sqlx::query_as(
        "SELECT lower(hex(bot_id)), name FROM attachments WHERE sha256 = ?1 ORDER BY name",
    )
    .bind(Sha256::digest(&card).to_vec())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        owners,
        vec![(twitter.replace('-', ""), "whale.png".to_owned())],
        "the copy belongs to the recipient alone"
    );

    let continuation = inputs
        .iter()
        .rfind(|input| input.turn_id == parent)
        .unwrap();
    assert_eq!(continuation.attempt, 2);
    let returned = &continuation.delegation.outcomes[0].files;
    assert_eq!(returned.len(), 1, "{:?}", continuation.delegation.outcomes);
    assert_eq!(returned[0].name, "receipt.txt");
    let staged = continuation
        .files
        .iter()
        .find(|file| file.attachment.id == returned[0].id)
        .expect("the returned file is staged for the parent");
    assert_eq!(&*staged.data, b"posted 1842");
    let owner: Uuid = sqlx::query_scalar("SELECT bot_id FROM attachments WHERE id = ?1")
        .bind(returned[0].id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(owner.to_string(), main, "the parent owns its copy");
    let results = else_api::providers::prompt::outcomes_block(continuation);
    assert!(
        results
            .contains("Returned [File: receipt.txt (text/plain, 11 bytes), saved at .attachments/"),
        "{results}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_joining_a_task_reaches_it_with_its_files() {
    owner_message_reaches_the_task_with_its_files(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_joining_an_updated_task_still_reaches_it_with_its_files() {
    owner_message_reaches_the_task_with_its_files(true).await;
}

/// A task that streams for long enough to ask the owner mid-reply.
fn slow(task: &str) -> String {
    format!(
        "[synthetic:slow] {task} {}",
        "Check the caption and the alt text. ".repeat(20)
    )
}

/// Waits until `turn` runs its attempt `attempt`.
async fn runs_attempt(app: &TestApp, turn: Uuid, attempt: i32) {
    let deadline = std::time::Instant::now() + common::SETTLE;
    loop {
        let now: (i32, String) = sqlx::query_as("SELECT attempt, status FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
        if now.0 >= attempt && now.1 == "running" {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{turn}: {now:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn owner_message_reaches_the_task_with_its_files(updated: bool) {
    let Setup {
        _root,
        provider,
        app,
        main,
        twitter,
        card,
        parent,
        token,
    } = setup().await;
    let (error, pending) = delegate(
        &app,
        &token,
        json!({ "target_bot_id": twitter, "task": slow("Post the card."),
                "files": ["cards/whale.png"] }),
    )
    .await;
    assert!(!error, "{pending}");
    let task: Uuid = pending["task_id"].as_str().unwrap().parse().unwrap();
    let child_of = || async {
        sqlx::query_scalar::<_, Uuid>("SELECT child_turn_id FROM delegations")
            .fetch_one(&app.db.pool)
            .await
            .unwrap()
    };
    let child = child_of().await;
    app.wait_turn_for(&child.to_string(), &["running"], Duration::from_secs(20))
        .await;
    if updated {
        let follow = app
            .send(&main, Uuid::new_v4(), "[synthetic:hang] Any news?", None)
            .await;
        assert_eq!(follow.body["turn"]["steered_into"], parent.to_string());
        runs_attempt(&app, parent, 2).await;
        let conversation: Uuid =
            sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
                .bind(parent)
                .fetch_one(&app.db.pool)
                .await
                .unwrap();
        let token = app
            .scheduler
            .grants()
            .issue(
                main.parse().unwrap(),
                conversation,
                parent,
                vec![Tool::Delegate],
                DEFAULT_TTL,
            )
            .await;
        let reply = app
            .call(
                "POST",
                "/tools/v1/mcp",
                Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                             "params": { "name": "delegation_update", "arguments": {
                                 "task_id": task,
                                 "message": slow("Use the blue caption.") } } })),
                Some(&token),
            )
            .await;
        assert_ne!(reply.body["result"]["isError"], true, "{}", reply.body);
        app.wait_turn(&child.to_string(), &["interrupted"]).await;
        let replacement = child_of().await;
        app.wait_turn(&replacement.to_string(), &["running"]).await;
    }
    let asking = child_of().await;
    let task_conversation: Uuid =
        sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
            .bind(asking)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    let ask_token = app
        .scheduler
        .grants()
        .issue(
            twitter.parse().unwrap(),
            task_conversation,
            asking,
            vec![Tool::AskOwner],
            DEFAULT_TTL,
        )
        .await;
    let asked = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "ask_owner", "arguments": {
                             "question": "Which caption should the card use?",
                             "choices": ["The blue one", "The red one"] } } })),
            Some(&ask_token),
        )
        .await;
    assert_ne!(asked.body["result"]["isError"], true, "{}", asked.body);

    let text = "Post it with this caption instead.";
    let sent = app.send(&twitter, Uuid::new_v4(), text, None).await;
    assert_eq!(
        sent.body["turn"]["steered_into"],
        asking.to_string(),
        "the message joins the task that asked: {}",
        sent.body
    );
    runs_attempt(&app, asking, 2).await;
    let inputs = provider.inputs.lock().unwrap().clone();
    let task = inputs
        .iter()
        .rfind(|input| input.turn_id == asking)
        .unwrap();
    assert!(
        task.steered.iter().any(|s| s.message.text == text),
        "the task continues with the owner's message"
    );
    // An update's continuation resumes the task's session, which was given
    // the file with the task; the task's own continuation is given it again.
    let given = if updated {
        inputs.iter().find(|input| input.turn_id == child).unwrap()
    } else {
        task
    };
    let staged = given
        .files
        .iter()
        .find(|file| file.attachment.name == "whale.png")
        .expect("the task's file is staged for it");
    assert_eq!(*staged.data, *card);
    let packet = else_api::providers::prompt::packet(given, 0);
    assert!(
        packet.contains(&format!(
            "saved at .attachments/{}/whale.png",
            staged.attachment.id
        )),
        "{packet}"
    );
    let packet = else_api::providers::prompt::packet(task, 0);
    assert!(packet.contains(text), "{packet}");
}
