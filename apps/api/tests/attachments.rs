mod common;

use axum::http::StatusCode;
use base64::Engine;
use common::{TOKEN, TestApp};
use serde_json::{Value, json};
use uuid::Uuid;

const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really";

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

async fn upload(
    app: &TestApp,
    bot: &str,
    id: Uuid,
    name: &str,
    media_type: &str,
    data: &[u8],
) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/attachments"),
        Some(json!({ "id": id, "name": name, "media_type": media_type, "data": b64(data) })),
    )
    .await
}

async fn download(
    app: &TestApp,
    path: &str,
    token: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut request = axum::http::Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "127.0.0.1:7878");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .router
        .clone()
        .oneshot(request.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

async fn send_parts(
    app: &TestApp,
    bot: &str,
    request_id: Uuid,
    text: &str,
    parts: Value,
) -> common::Reply {
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/turns"),
        Some(json!({ "request_id": request_id, "text": text, "parts": parts, "provider_id": "synthetic" })),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn upload_is_idempotent_scoped_and_served_safely() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let other = app.create_bot("Other").await;
    let id = Uuid::new_v4();

    let first = upload(&app, &bot, id, "shot.png", "Image/PNG", PNG).await;
    assert_eq!(first.status, StatusCode::CREATED, "{:?}", first.body);
    let meta =
        json!({ "id": id, "name": "shot.png", "media_type": "image/png", "size": PNG.len() });
    assert_eq!(first.body, meta);
    let again = upload(&app, &bot, id, "shot.png", "image/png", PNG).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body, meta);
    for (target, name, data) in [
        (&bot, "shot.png", &b"other"[..]),
        (&bot, "renamed.png", PNG),
        (&other, "shot.png", PNG),
    ] {
        let conflict = upload(&app, target, id, name, "image/png", data).await;
        assert_eq!(conflict.status, StatusCode::CONFLICT, "{name}");
        assert_eq!(conflict.body["error"]["code"], "attachment_conflict");
    }

    let path = format!("/v1/bots/{bot}/attachments/{id}");
    let (status, headers, bytes) = download(&app, &path, Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, PNG);
    assert_eq!(headers["content-type"], "image/png");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename*=UTF-8''shot.png"
    );
    assert_eq!(
        download(&app, &path, None).await.0,
        StatusCode::UNAUTHORIZED
    );
    let foreign = format!("/v1/bots/{other}/attachments/{id}");
    assert_eq!(
        download(&app, &foreign, Some(TOKEN)).await.0,
        StatusCode::NOT_FOUND
    );

    let html = Uuid::new_v4();
    let reply = upload(
        &app,
        &bot,
        html,
        "page.html",
        "text/html",
        b"<script>1</script>",
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    let (_, headers, _) = download(
        &app,
        &format!("/v1/bots/{bot}/attachments/{html}"),
        Some(TOKEN),
    )
    .await;
    assert_eq!(headers["content-type"], "application/octet-stream");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_and_oversized_uploads_are_refused() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    for (name, media_type) in [("../x", "text/plain"), ("", "text/plain"), ("a", "nope")] {
        let reply = upload(&app, &bot, Uuid::new_v4(), name, media_type, b"x").await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{name} {media_type}");
        assert_eq!(reply.body["error"]["code"], "invalid_attachment");
    }
    let bad = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/attachments"),
            Some(json!({ "id": Uuid::new_v4(), "name": "a", "media_type": "text/plain", "data": "%%%" })),
        )
        .await;
    assert_eq!(bad.body["error"]["code"], "invalid_attachment");
    let path = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/attachments"),
            Some(json!({ "id": Uuid::new_v4(), "name": "a", "media_type": "text/plain", "data": "", "path": "/etc/passwd" })),
        )
        .await;
    assert_eq!(
        path.status,
        StatusCode::BAD_REQUEST,
        "clients cannot name a path"
    );

    let big = vec![7u8; 10 * 1024 * 1024 + 1];
    let reply = upload(
        &app,
        &bot,
        Uuid::new_v4(),
        "big.bin",
        "application/octet-stream",
        &big,
    )
    .await;
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{:?}",
        reply.body
    );
    let limit = vec![7u8; 10 * 1024 * 1024];
    let reply = upload(
        &app,
        &bot,
        Uuid::new_v4(),
        "limit.bin",
        "application/octet-stream",
        &limit,
    )
    .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.body);

    let missing = Uuid::new_v4();
    let reply = upload(
        &app,
        &missing.to_string(),
        Uuid::new_v4(),
        "a",
        "text/plain",
        b"x",
    )
    .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attachments")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1, "only the upload at the limit was stored");
}

#[tokio::test(flavor = "multi_thread")]
async fn ordered_parts_persist_through_events_restart_and_provider_input() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let image = Uuid::new_v4();
    let notes = Uuid::new_v4();
    let image_meta = upload(&app, &bot, image, "shot.png", "image/png", PNG)
        .await
        .body;
    let notes_meta = upload(&app, &bot, notes, "notes.txt", "text/plain", b"todo\n")
        .await
        .body;
    let parts = json!([
        { "type": "text", "text": "  Look at " },
        { "type": "file", "file": image_meta },
        { "type": "text", "text": "\n\nthen " },
        { "type": "file", "file": notes_meta },
    ]);
    let text = "  Look at [File: shot.png]\n\nthen [File: notes.txt]";
    let request = Uuid::new_v4();
    let sent = send_parts(&app, &bot, request, text, parts.clone()).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{:?}", sent.body);
    assert_eq!(sent.body["item"]["text"], text);
    assert_eq!(sent.body["item"]["parts"], parts);

    let retry = send_parts(&app, &bot, request, text, parts.clone()).await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["item"]["id"], sent.body["item"]["id"]);
    let mut swapped = parts.clone();
    swapped[2]["text"] = json!("\n\nthen\u{20}");
    swapped[0]["text"] = json!(" Look at ");
    for (body_text, body_parts) in [
        (
            " Look at [File: shot.png]\n\nthen [File: notes.txt]",
            swapped,
        ),
        (text, json!([])),
    ] {
        let conflict = send_parts(&app, &bot, request, body_text, body_parts).await;
        assert_eq!(conflict.status, StatusCode::CONFLICT, "{:?}", conflict.body);
    }

    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    assert_eq!(app.wait_turn(&turn, &["completed"]).await, "completed");
    let item_id = sent.body["item"]["id"].as_str().unwrap().to_owned();
    let upsert: String = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE kind = 'item.upsert' AND json_extract(data, '$.item.id') = ?1",
    )
    .bind(&item_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&upsert).unwrap()["item"]["parts"],
        parts
    );

    let input = else_api::db::turns::input(&app.db.pool, turn.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(input.text, text);
    assert_eq!(serde_json::to_value(&input.parts).unwrap(), parts);
    let names: Vec<&str> = input
        .files
        .iter()
        .map(|f| f.attachment.name.as_str())
        .collect();
    assert_eq!(names, ["shot.png", "notes.txt"]);
    assert_eq!(&*input.files[0].data, PNG);
    let packet = else_api::providers::prompt::packet(&input, 0);
    assert!(
        packet.starts_with("  Look at [File: shot.png (image/png, "),
        "{packet}"
    );
    assert!(
        packet.contains(&format!("saved at .attachments/{notes}/notes.txt]")),
        "{packet}"
    );

    let app = app.restart().await;
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let items = snapshot.body["items"].as_array().unwrap();
    let saved = items.iter().find(|i| i["id"] == item_id.as_str()).unwrap();
    assert_eq!(saved["parts"], parts);
    assert!(
        items
            .iter()
            .filter(|i| i["id"] != item_id.as_str())
            .all(|i| i["parts"] == json!([])),
        "other items expose empty parts"
    );

    let follow = app
        .send(&bot, Uuid::new_v4(), "and now?", Some("synthetic"))
        .await;
    let follow_turn = follow.body["turn"]["id"].as_str().unwrap().to_owned();
    assert_eq!(follow.body["item"]["parts"], json!([]));
    assert_eq!(
        app.wait_turn(&follow_turn, &["completed"]).await,
        "completed"
    );
    let input = else_api::db::turns::input(&app.db.pool, follow_turn.parse().unwrap())
        .await
        .unwrap();
    assert!(input.parts.is_empty());
    let packet = else_api::providers::prompt::packet(&input, 0);
    assert!(
        packet.contains(&format!("saved at .attachments/{image}/shot.png]")),
        "{packet}"
    );
    assert!(packet.ends_with("and now?"));
}

#[tokio::test(flavor = "multi_thread")]
async fn parts_are_validated_against_text_and_the_bot() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let other = app.create_bot("Other").await;
    let mine = upload(&app, &bot, Uuid::new_v4(), "a.png", "image/png", PNG)
        .await
        .body;
    let theirs = upload(&app, &other, Uuid::new_v4(), "b.png", "image/png", PNG)
        .await
        .body;
    let file = |meta: &Value| json!({ "type": "file", "file": meta });

    let only = send_parts(
        &app,
        &bot,
        Uuid::new_v4(),
        "[File: a.png]",
        json!([file(&mine)]),
    )
    .await;
    assert_eq!(only.status, StatusCode::ACCEPTED, "{:?}", only.body);

    let plain = send_parts(
        &app,
        &bot,
        Uuid::new_v4(),
        "hi ",
        json!([{ "type": "text", "text": "hi " }]),
    )
    .await;
    assert_eq!(plain.status, StatusCode::ACCEPTED);
    assert_eq!(plain.body["item"]["parts"], json!([]));

    let mut renamed = mine.clone();
    renamed["name"] = json!("evil.png");
    let mut resized = mine.clone();
    resized["size"] = json!(1);
    let unknown =
        json!({ "id": Uuid::new_v4(), "name": "a.png", "media_type": "image/png", "size": 3 });
    for (text, parts, code) in [
        ("[File: a.png] ", json!([file(&mine)]), "invalid_input"),
        (
            "[File: b.png]",
            json!([file(&theirs)]),
            "invalid_attachment",
        ),
        (
            "[File: evil.png]",
            json!([file(&renamed)]),
            "invalid_attachment",
        ),
        (
            "[File: a.png]",
            json!([file(&resized)]),
            "invalid_attachment",
        ),
        (
            "[File: a.png]",
            json!([file(&unknown)]),
            "invalid_attachment",
        ),
        (
            &"[File: a.png]".repeat(9),
            json!(vec![file(&mine); 9]),
            "invalid_input",
        ),
    ] {
        let reply = send_parts(&app, &bot, Uuid::new_v4(), text, parts).await;
        assert_eq!(
            reply.status,
            StatusCode::BAD_REQUEST,
            "{text}: {:?}",
            reply.body
        );
        assert_eq!(reply.body["error"]["code"], code, "{text}");
    }
}
