//! Drafts that carry files and pictures.

use super::*;

fn png(shade: u8) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbImage::from_pixel(2, 2, image::Rgb([shade, 0, 0]))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connected_email_sends_its_approved_pdf_from_the_account() {
    let root = tempfile::tempdir().unwrap();
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone()).projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    project(root.path(), &main, &[("report.pdf", PDF)]);
    let account = connect(app, &main, "main@owner.example", true).await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;

    let mut args = connected_reply(account);
    args["attachments"] = json!([{ "path": "~/work/missing.pdf" }]);
    let refused = tool(app, &token, "draft_email", args.clone()).await;
    assert!(
        refused.error && refused.code() == "FILE_UNAVAILABLE",
        "a missing file is refused when drafting"
    );
    args["attachments"] = json!([{ "path": "report.pdf" }]);
    let id = draft(app, &token, args).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[0].clone();
    let files = files_of(&card);
    assert_eq!(files.len(), 1, "{card}");
    assert_eq!(files[0]["name"], "report.pdf");
    assert_eq!(files[0]["media_type"], "application/pdf");
    assert_eq!(files[0]["size"], PDF.len());
    assert_eq!(files[0]["sha256"].as_str().unwrap().len(), 64);
    let file_id = files[0]["id"].as_str().unwrap().to_owned();
    let bytes = app
        .owner(
            "GET",
            &format!("/v1/bots/{main}/attachments/{file_id}"),
            None,
        )
        .await;
    assert_eq!(bytes.status, StatusCode::OK, "the card can fetch the file");

    let unseen = owner_action(app, &main, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(unseen.status, StatusCode::CONFLICT, "{}", unseen.body);
    assert_eq!(unseen.body["error"]["code"], "attachments_unseen");
    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "attachments": [file_id] }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("Attachments: report.pdf (application/pdf, 1 KB)"),
        "{packet}"
    );
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    let sends = mail.sends();
    assert_eq!(sends.len(), 1);
    assert_eq!(sends[0].attachments.len(), 1);
    let attached = &sends[0].attachments[0];
    assert_eq!(
        (attached.name.as_str(), attached.media_type.as_str()),
        ("report.pdf", "application/pdf")
    );
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&attached.data_base64)
            .unwrap(),
        PDF
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_x_post_takes_pictures_the_owner_may_remove_and_stages_them_for_its_send() {
    let root = tempfile::tempdir().unwrap();
    let h = harness(TestApp::builder().projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (first, second) = (png(10), png(200));
    project(
        root.path(),
        &main,
        &[("a.png", &first), ("b.png", &second), ("report.pdf", PDF)],
    );
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;

    let mut post = ship_post();
    post["attachments"] = json!([{ "path": "report.pdf" }]);
    let refused = tool(app, &token, "draft_message", post.clone()).await;
    assert!(refused.error, "{}", refused.body);
    let refused = refused.result.as_str().unwrap();
    assert!(
        refused.contains("an X post takes only PNG, JPEG, GIF, or WebP images"),
        "{refused}"
    );
    post["attachments"] = json!([{ "path": "a.png" }, { "path": "~/work/b.png" }]);
    let id = draft(app, &token, post).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let files = files_of(&shown(app, &main, turn).await[0]);
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|f| f["media_type"] == "image/png"));
    let keep = files[1]["id"].as_str().unwrap().to_owned();
    let dir = else_api::providers::host::project_dir(root.path(), main.parse().unwrap()).unwrap();
    let staged = dir.join(".attachments").join(&keep).join("b.png");
    assert!(
        !staged.exists(),
        "nothing is staged before the owner approves"
    );
    for (change, why) in [
        (
            "json_insert(content, '$.attachments[#]', json_extract(content, '$.attachments[0]'))",
            "repeat",
        ),
        (
            "json_set(content, '$.attachments', json_array(json_extract(content, '$.attachments[1]'), \
          json_extract(content, '$.attachments[0]')))",
            "reorder",
        ),
    ] {
        let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE outgoing_drafts SET content = {change} WHERE id = ?1"
        )))
        .bind(id)
        .execute(&app.db.pool)
        .await;
        assert!(changed.is_err(), "the owner may not {why} attachments");
    }

    let added = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "attachments": [Uuid::new_v4()] }),
    )
    .await;
    assert_eq!(added.status, StatusCode::BAD_REQUEST, "{}", added.body);
    let removed = owner_action(
        app,
        &main,
        id,
        "edit",
        json!({ "version": 1, "attachments": [keep] }),
    )
    .await;
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    let files = files_of(&removed.body["draft"]);
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["id"], keep);
    let changed = sqlx::query(
        "UPDATE outgoing_drafts SET content = json_set(content, '$.attachments[0].name', 'x.png')
         WHERE id = ?1",
    )
    .bind(id)
    .execute(&app.db.pool)
    .await;
    assert!(changed.is_err(), "a stored file never changes");

    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 2, "attachments": [keep] }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
    std::fs::write(&staged, b"tampered").unwrap();
    let started = tool(app, &token, "start_draft_send", json!({ "draft_id": id })).await;
    let result = started.result;
    assert_eq!(result["status"], "sending", "{}", started.body);
    assert_eq!(
        result["draft"]["attachment_paths"],
        json!([format!("~/work/.attachments/{keep}/b.png")])
    );
    assert_eq!(
        std::fs::read(&staged).unwrap(),
        second,
        "the claim restores the approved bytes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_attachment_fails_the_send_and_nothing_leaves() {
    let root = tempfile::tempdir().unwrap();
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone()).projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    project(root.path(), &main, &[("report.pdf", PDF)]);
    let account = connect(app, &main, "main@owner.example", true).await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut args = connected_reply(account);
    args["attachments"] = json!([{ "path": "report.pdf" }]);
    let id = draft(app, &token, args).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let file_id = files_of(&shown(app, &main, turn).await[0])[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let approved = owner_action(
        app,
        &main,
        id,
        "approve",
        json!({ "version": 1, "attachments": [file_id] }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    sqlx::query("UPDATE attachments SET data = zeroblob(size) WHERE id = ?1")
        .bind(file_id.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert!(sent.error && sent.code() == "FILE_CHANGED", "{}", sent.body);
    assert!(mail.commands.lock().unwrap().is_empty(), "nothing was sent");
    let status: String = sqlx::query_scalar("SELECT status FROM outgoing_drafts WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_approval_of_a_draft_with_files_needs_the_files_the_owner_saw() {
    let root = tempfile::tempdir().unwrap();
    let mail = Arc::new(FakeMail::default());
    let h = harness(TestApp::builder().mail(mail.clone()).projects(root.path())).await;
    let app = &h.app;
    let main = app.main_bot().await;
    project(root.path(), &main, &[("report.pdf", PDF)]);
    let account = connect(app, &main, "main@owner.example", true).await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let mut args = connected_reply(account);
    args["attachments"] = json!([{ "path": "report.pdf" }]);
    let id = draft(app, &token, args).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let file_id = files_of(&shown(app, &main, turn).await[0])[0]["id"].clone();

    let unseen = send_with_draft(app, &main, "Send it.", json!({ "id": id, "version": 1 })).await;
    assert_eq!(unseen.status, StatusCode::CONFLICT, "{}", unseen.body);
    assert_eq!(unseen.body["error"]["code"], "attachments_unseen");
    let (_, token) = joined(
        &h,
        &main,
        turn,
        &token,
        "Send it.",
        Some(json!({ "id": id, "version": 1, "attachments": [file_id] })),
    )
    .await;
    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": id }),
    )
    .await;
    assert_eq!(confirmed.result["status"], "approved", "{}", confirmed.body);
    let sent = tool(app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    assert_eq!(mail.sends()[0].attachments.len(), 1);
}
