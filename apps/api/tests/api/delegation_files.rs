use crate::common;

use common::TestApp;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, Harness, harness, tool};
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn png() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(3, 2, image::Rgb([200, 40, 90]))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

struct Setup {
    _root: tempfile::TempDir,
    h: Harness,
    main: String,
    twitter: String,
    card: Vec<u8>,
    parent: Uuid,
    token: String,
}

/// The main bot with a card in its project, mid-request with a delegate
/// grant, and a Twitter bot with a receipt in its own.
async fn setup(parent_prompt: &str) -> Setup {
    let root = tempfile::tempdir().unwrap();
    let h = harness(TestApp::builder().max_active(2).projects(root.path())).await;
    let app = &h.app;
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

    let (parent, token) = h.running(&main, parent_prompt).await;

    Setup {
        _root: root,
        h,
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
        h,
        main,
        twitter,
        card,
        parent,
        token,
    } = setup(GATE).await;
    let app = &h.app;
    let task = "Post the attached card, then reply with [receipt](out/receipt.txt).";
    for (path, reason) in [
        ("../../escape.png", "No regular file"),
        ("cards/missing.png", "No regular file"),
    ] {
        let message = tool(
            app,
            &token,
            "delegate",
            json!({ "target_bot_id": twitter, "task": task, "files": [path] }),
        )
        .await;
        let error = message.error;
        let message = message.result.as_str().unwrap_or_default();
        assert!(
            error && message.starts_with("FILE_UNAVAILABLE"),
            "{message}"
        );
        assert!(message.contains(reason), "{message}");
    }
    let pending = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": twitter, "task": task, "files": ["cards/whale.png"] }),
    )
    .await;
    assert!(!pending.error, "{}", pending.body);
    let pending = pending.result;
    assert_eq!(pending["status"], "pending", "{pending}");
    let digest = hex::encode(Sha256::digest(&card));
    assert_eq!(
        pending["files"],
        json!([{ "name": "whale.png", "media_type": "image/png",
                 "size": card.len(), "sha256": digest }]),
    );

    let task_id = pending["task_id"].as_str().unwrap().parse().unwrap();
    let child = child_of(app, task_id).await;
    h.release(parent).await;
    app.wait_turn(parent, &["completed"]).await;
    let child_inputs = h.inputs(child).await;
    let child = child_inputs.first().expect("the task ran");
    assert_eq!(child.files.len(), 1, "only the named file is sent");
    assert_eq!(child.files[0].attachment.name, "whale.png");
    assert_eq!(*child.files[0].data, *card, "exact bytes");
    let request = common::fresh_packet(child);
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

    let inputs = h.inputs(parent).await;
    let continuation = inputs.last().unwrap();
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

async fn owner_message_reaches_the_task_with_its_files(updated: bool) {
    let Setup {
        _root,
        h,
        main,
        twitter,
        card,
        parent,
        token,
    } = setup(GATE_EVERY_ATTEMPT).await;
    let app = &h.app;
    let pending = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": twitter, "task": format!("{GATE_EVERY_ATTEMPT} Post the card."),
                "files": ["cards/whale.png"] }),
    )
    .await;
    assert!(!pending.error, "{}", pending.body);
    let pending = pending.result;
    let task: Uuid = pending["task_id"].as_str().unwrap().parse().unwrap();
    let child = child_of(app, task).await;
    h.release(parent).await;
    let initial = h.started(child).await;
    let (asking, ask_token) = if updated {
        app.wait_turn(parent, &["waiting"]).await;
        let follow = app.send(&main, Uuid::new_v4(), "Any news?", None).await;
        assert_eq!(follow.body["turn"]["steered_into"], parent.to_string());
        let token = h.continued(parent, &token).await;
        let reply = tool(
            app,
            &token,
            "delegation_update",
            json!({ "task_id": task, "message": format!("{GATE_EVERY_ATTEMPT} Use the blue caption.") }),
        )
        .await;
        assert!(!reply.error, "{}", reply.body);
        app.wait_turn(child, &["interrupted"]).await;
        let replacement = child_of(app, task).await;
        h.started(replacement).await
    } else {
        initial
    };
    let asked = tool(
        app,
        &ask_token,
        "ask_owner",
        json!({ "question": "Which caption should the card use?",
                "choices": ["The blue one", "The red one"] }),
    )
    .await;
    assert!(!asked.error, "{}", asked.body);
    h.release(asking).await;
    app.wait_turn(asking, &["waiting"]).await;

    let text = "Post it with this caption instead.";
    let sent = app.send(&twitter, Uuid::new_v4(), text, None).await;
    assert_eq!(
        sent.body["turn"]["steered_into"],
        asking.to_string(),
        "the message joins the task that asked: {}",
        sent.body
    );
    h.continued(asking, &ask_token).await;
    let inputs = h.inputs(asking).await;
    let task = inputs.last().unwrap();
    assert!(
        task.steered.iter().any(|s| s.message.text == text),
        "the task continues with the owner's message"
    );
    // An update's continuation resumes the task's session, which was given
    // the file with the task; the task's own continuation is given it again.
    let original = h.inputs(child).await;
    let given = if updated {
        original.first().unwrap()
    } else {
        task
    };
    let staged = given
        .files
        .iter()
        .find(|file| file.attachment.name == "whale.png")
        .expect("the task's file is staged for it");
    assert_eq!(*staged.data, *card);
    let packet = common::fresh_packet(given);
    assert!(
        packet.contains(&format!(
            "saved at .attachments/{}/whale.png",
            staged.attachment.id
        )),
        "{packet}"
    );
    let packet = common::fresh_packet(task);
    assert!(packet.contains(text), "{packet}");
    h.release(asking).await;
}

async fn child_of(app: &TestApp, task: Uuid) -> Uuid {
    sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}
