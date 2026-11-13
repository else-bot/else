use crate::common;

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::http::StatusCode;
use else_api::app::messages::{MessagePart, flatten};
use else_api::app::providers::{
    Availability, Profile, Provider, Stop, TurnInput, TurnOutcome, TurnSink,
};
use http_body_util::BodyExt as _;
use serde_json::Value;
use tower::ServiceExt as _;
use uuid::Uuid;

use common::{TestApp, TestDb};

struct ImageProvider {
    root: PathBuf,
    replies: Vec<String>,
    outcome: TurnOutcome,
}

#[async_trait::async_trait]
impl Provider for ImageProvider {
    fn id(&self) -> &'static str {
        "synthetic"
    }
    fn label(&self) -> &'static str {
        "Image test provider"
    }
    fn availability(&self) -> Availability {
        Availability::Available
    }
    fn uses_profile(&self) -> bool {
        false
    }
    fn validate_profile(&self, _: &Profile) -> Result<(), String> {
        Err("no profile".into())
    }
    fn project_dir(&self, bot: Uuid) -> Option<PathBuf> {
        Some(self.root.join(bot.to_string()))
    }
    async fn run(&self, _: TurnInput, sink: &dyn TurnSink, _: Stop) -> TurnOutcome {
        sink.sending().await.unwrap();
        sink.running().await.unwrap();
        for (index, reply) in self.replies.iter().enumerate() {
            for chunk in reply.chars().collect::<Vec<_>>().chunks(7) {
                sink.text(&chunk.iter().collect::<String>()).await.unwrap();
            }
            if index + 1 < self.replies.len() {
                sink.end_message().await.unwrap();
            }
        }
        self.outcome.clone()
    }
}

async fn app(root: &Path, replies: Vec<String>, outcome: TurnOutcome) -> TestApp {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    TestApp::with_registry(
        Arc::new(db),
        1,
        else_api::providers::Registry::new(vec![Arc::new(ImageProvider {
            root: root.to_owned(),
            replies,
            outcome,
        })]),
    )
}

fn png() -> Vec<u8> {
    image_bytes(image::ImageFormat::Png)
}

fn image_bytes(format: image::ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(2, 2)
        .write_to(&mut bytes, format)
        .unwrap();
    bytes.into_inner()
}

async fn send(app: &TestApp, bot: &str, status: &str) {
    let sent = app
        .send(bot, Uuid::new_v4(), "Send the images", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{:?}", sent.body);
    assert_eq!(
        app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &[status])
            .await,
        status
    );
}

async fn replies(app: &TestApp, bot: &str) -> Vec<Value> {
    app.owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["kind"] == "assistant")
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn image_replies_persist_in_order_and_remain_downloadable_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let app = app(
        directory.path(),
        vec![
            "Recap 🥔 ![first](charts/first.png) and ![second](/home/elsebot/work/second.png) done."
                .into(),
            "Final ![third](~/work/third.png)".into(),
        ],
        TurnOutcome::Completed,
    )
    .await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir_all(project.join("charts")).unwrap();
    let bytes = png();
    for name in ["charts/first.png", "second.png", "third.png"] {
        std::fs::write(project.join(name), &bytes).unwrap();
    }
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[0]["text"],
        "Recap 🥔 [File: first.png] and [File: second.png] done."
    );
    assert_eq!(messages[1]["text"], "Final [File: third.png]");
    let mut files = Vec::new();
    for message in &messages {
        let parts: Vec<MessagePart> = serde_json::from_value(message["parts"].clone()).unwrap();
        assert_eq!(flatten(&parts), message["text"].as_str().unwrap());
        files.extend(parts.into_iter().filter_map(|part| match part {
            MessagePart::File { file } => Some(file),
            _ => None,
        }));
    }
    assert_eq!(
        files
            .iter()
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>(),
        ["first.png", "second.png", "third.png"]
    );
    std::fs::remove_dir_all(project).unwrap();
    let app = app.restart().await;
    assert_eq!(replies(&app, &bot).await, messages);
    let other = app.create_bot("Other").await;
    for file in files {
        let saved = else_api::db::attachments::read(&app.db.pool, bot.parse().unwrap(), file.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved, (file.clone(), bytes.clone()));
        let response = app
            .router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/v1/bots/{bot}/attachments/{}", file.id))
                    .header("host", "127.0.0.1:7878")
                    .header("authorization", format!("Bearer {}", common::TOKEN))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "image/png");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            bytes
        );
        let denied = app
            .owner(
                "GET",
                &format!("/v1/bots/{other}/attachments/{}", file.id),
                None,
            )
            .await;
        assert_eq!(denied.status, StatusCode::NOT_FOUND);
    }
    let followup = app
        .send(
            &bot,
            Uuid::new_v4(),
            "Describe these images",
            Some("synthetic"),
        )
        .await;
    let followup_id = followup.body["turn"]["id"].as_str().unwrap();
    app.wait_turn(followup_id, &["completed"]).await;
    let input = else_api::db::turns::input(&app.db.pool, followup_id.parse().unwrap())
        .await
        .unwrap();
    assert_eq!(
        input
            .history
            .iter()
            .flat_map(|entry| &entry.parts)
            .filter(|part| matches!(part, MessagePart::File { .. }))
            .count(),
        3
    );
    let upserts: Vec<sqlx::types::Json<Value>> = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE bot_id = ?1 AND kind = 'item.upsert'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert!(upserts.iter().any(|event| {
        event["item"]["parts"]
            .as_array()
            .is_some_and(|parts| parts.iter().any(|part| part["type"] == "file"))
    }));
}

#[tokio::test(flavor = "multi_thread")]
async fn unsafe_missing_and_non_image_references_stay_readable_without_attachments() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let outside = directory.path().join("outside.png");
    std::fs::write(&outside, png()).unwrap();
    let reply = format!(
        "![traversal](../outside.png) ![encoded](%2e%2e/outside.png) ![absolute]({}) ![link](link.png) \
         ![directory](linked/outside.png) ![hard](hard.png) ![missing](missing.png) \
         ![invalid](bad.png) ![remote](https://example.com/image.png) \
         `![inline](valid.png)`\n\n```markdown\n![example](valid.png)\n```",
        outside.display()
    );
    let app = app(
        directory.path(),
        vec![reply.clone()],
        TurnOutcome::Completed,
    )
    .await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir(&project).unwrap();
    symlink(&outside, project.join("link.png")).unwrap();
    symlink(directory.path(), project.join("linked")).unwrap();
    std::fs::hard_link(&outside, project.join("hard.png")).unwrap();
    std::fs::write(project.join("bad.png"), b"not an image").unwrap();
    std::fs::write(project.join("valid.png"), png()).unwrap();
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    assert_eq!(messages[0]["text"], reply);
    assert_eq!(messages[0]["parts"], serde_json::json!([]));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attachments")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn reference_images_and_percent_encoded_file_names_attach() {
    let directory = tempfile::tempdir().unwrap();
    let app = app(
        directory.path(),
        vec!["Here is ![chart][saved].\n\n[saved]: output/r%C3%A9sum%C3%A9%20plot.png".into()],
        TurnOutcome::Completed,
    )
    .await;
    let bot = app.main_bot().await;
    let output = directory.path().join(&bot).join("output");
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join("résumé plot.png"), png()).unwrap();
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    assert_eq!(
        messages[0]["text"],
        "Here is [File: résumé plot.png].\n\n[saved]: output/r%C3%A9sum%C3%A9%20plot.png"
    );
    assert_eq!(messages[0]["parts"][1]["file"]["name"], "résumé plot.png");
}

#[tokio::test(flavor = "multi_thread")]
async fn image_types_are_sniffed_and_only_eight_images_attach_per_message() {
    let directory = tempfile::tempdir().unwrap();
    let reply = Vec::from_fn(10, |index| format!("![image {index}](image-{index}.bin)")).join(" ");
    let app = app(directory.path(), vec![reply], TurnOutcome::Completed).await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir(&project).unwrap();
    let formats = [
        (image::ImageFormat::Png, "image/png"),
        (image::ImageFormat::Jpeg, "image/jpeg"),
        (image::ImageFormat::Gif, "image/gif"),
        (image::ImageFormat::WebP, "image/webp"),
    ];
    for index in 0..10 {
        std::fs::write(
            project.join(format!("image-{index}.bin")),
            image_bytes(formats[index % 4].0),
        )
        .unwrap();
    }
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    let parts = messages[0]["parts"].as_array().unwrap();
    let files = parts
        .iter()
        .filter(|part| part["type"] == "file")
        .collect::<Vec<_>>();
    assert_eq!(files.len(), 8);
    for (index, file) in files.iter().enumerate() {
        assert_eq!(file["file"]["media_type"], formats[index % 4].1);
    }
    assert!(messages[0]["text"].as_str().unwrap().contains("![image 8]"));
    assert!(messages[0]["text"].as_str().unwrap().contains("![image 9]"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_images_stay_readable_and_valid_neighbors_attach() {
    let directory = tempfile::tempdir().unwrap();
    let reply = "![too big](big.png) ![normal](normal.png)";
    let app = app(directory.path(), vec![reply.into()], TurnOutcome::Completed).await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir(&project).unwrap();
    std::fs::File::create(project.join("big.png"))
        .unwrap()
        .set_len(10 * 1024 * 1024 + 1)
        .unwrap();
    std::fs::write(project.join("normal.png"), png()).unwrap();
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    assert_eq!(
        messages[0]["text"],
        "![too big](big.png) [File: normal.png]"
    );
    assert_eq!(
        messages[0]["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part["type"] == "file")
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unfinished_message_keeps_text_without_attaching_images() {
    let directory = tempfile::tempdir().unwrap();
    let reply = "![normal](normal.png)";
    let app = app(
        directory.path(),
        vec![reply.into()],
        TurnOutcome::Interrupted,
    )
    .await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("normal.png"), png()).unwrap();
    send(&app, &bot, "interrupted").await;
    let messages = replies(&app, &bot).await;
    assert_eq!(messages[0]["text"], reply);
    assert_eq!(messages[0]["parts"], serde_json::json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrade_preserves_owner_attachments_assistant_text_and_foreign_keys() {
    let db = TestDb::empty().await;
    else_api::db::MIGRATOR.run_to(16, &db.pool).await.unwrap();
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT b.id, c.id FROM bots b JOIN conversations c ON c.bot_id = b.id")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    let data = png();
    let (file, _) = else_api::db::attachments::store(
        &db.pool,
        bot,
        Uuid::new_v4(),
        "original.png",
        "image/png",
        &data,
    )
    .await
    .unwrap();
    let parts = vec![MessagePart::File { file: file.clone() }];
    let (turn, owner, assistant) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash,
            turn_no, owner_item_id, provider_id, status)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, 'synthetic', 'completed')",
    )
    .bind(turn)
    .bind(bot)
    .bind(conversation)
    .bind(Uuid::new_v4())
    .bind(vec![0_u8; 32])
    .bind(owner)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text, parts)
         VALUES (?1, ?2, 1, ?3, 'user', ?4, ?5)",
    )
    .bind(owner)
    .bind(conversation)
    .bind(turn)
    .bind(flatten(&parts))
    .bind(sqlx::types::Json(&parts))
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, 2, ?3, 'assistant', 'Existing assistant reply')",
    )
    .bind(assistant)
    .bind(conversation)
    .bind(turn)
    .execute(&db.pool)
    .await
    .unwrap();
    else_api::db::migrate(&db.pool).await.unwrap();
    let mut conn = db.pool.acquire().await.unwrap();
    let owner_item = else_api::db::turns::load_item(&mut conn, owner)
        .await
        .unwrap();
    assert_eq!(owner_item.text, flatten(&parts));
    assert_eq!(owner_item.parts.unwrap().0, parts);
    let assistant_item = else_api::db::turns::load_item(&mut conn, assistant)
        .await
        .unwrap();
    assert_eq!(assistant_item.text, "Existing assistant reply");
    assert!(assistant_item.parts.is_none());
    let violations = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut *conn)
        .await
        .unwrap();
    assert!(violations.is_empty());
    drop(conn);
    assert_eq!(
        else_api::db::attachments::read(&db.pool, bot, file.id)
            .await
            .unwrap(),
        Some((file, data))
    );
    assert!(
        sqlx::query("UPDATE items SET assistant_parts = '[]' WHERE id = ?1")
            .bind(owner)
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE items SET parts = '[]' WHERE id = ?1")
            .bind(assistant)
            .execute(&db.pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn repeated_references_share_one_stored_image() {
    let directory = tempfile::tempdir().unwrap();
    let app = app(
        directory.path(),
        vec!["![One](chart.png) again ![Two](./chart.png) and ![Three](~/work/chart.png)".into()],
        TurnOutcome::Completed,
    )
    .await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("chart.png"), png()).unwrap();
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    let parts: Vec<MessagePart> = serde_json::from_value(messages[0]["parts"].clone()).unwrap();
    let files: Vec<_> = parts
        .into_iter()
        .filter_map(|part| match part {
            MessagePart::File { file } => Some(file.id),
            _ => None,
        })
        .collect();
    assert_eq!(files.len(), 3);
    assert!(files.iter().all(|id| *id == files[0]));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM attachments")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn ordinary_camera_photos_become_attachments() {
    let directory = tempfile::tempdir().unwrap();
    let app = app(
        directory.path(),
        vec!["![Photo](photo.jpg)".into()],
        TurnOutcome::Completed,
    )
    .await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir_all(&project).unwrap();
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(6000, 4000)
        .write_to(&mut encoded, image::ImageFormat::Jpeg)
        .unwrap();
    std::fs::write(project.join("photo.jpg"), encoded.into_inner()).unwrap();
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    let parts: Vec<MessagePart> = serde_json::from_value(messages[0]["parts"].clone()).unwrap();
    assert!(
        matches!(&parts[0], MessagePart::File { file } if file.name == "photo.jpg" && file.media_type == "image/jpeg")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn linked_project_files_attach_and_other_links_stay() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("outside.csv"), "secret").unwrap();
    let app = app(
        directory.path(),
        vec![
            "Files: [Revised daily CSV](evidence/daily_counts_utc.csv) · [Report](~/work/report.md) \
             · [Evidence ZIP](/home/elsebot/work/evidence.zip) · [chart](chart.png) · [data][csv] \
             ![Chart](chart.png). \
             See [site](https://example.com), [missing](missing.csv), [outside](../outside.csv), \
             [folder](evidence) and `[code](report.md)`.\n\n[csv]: evidence/daily_counts_utc.csv"
                .into(),
        ],
        TurnOutcome::Completed,
    )
    .await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir_all(project.join("evidence")).unwrap();
    let csv = b"day,count\n2026-11-08,3\n".to_vec();
    std::fs::write(project.join("evidence/daily_counts_utc.csv"), &csv).unwrap();
    std::fs::write(project.join("report.md"), "# Report").unwrap();
    std::fs::write(project.join("evidence.zip"), b"PK\x03\x04").unwrap();
    std::fs::write(project.join("chart.png"), png()).unwrap();
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    assert_eq!(
        messages[0]["text"],
        "Files: [File: daily_counts_utc.csv] · [File: report.md] · [File: evidence.zip] \
         · [File: chart.png] · [File: daily_counts_utc.csv] [File: chart.png]. See \
         [site](https://example.com), \
         [missing](missing.csv), [outside](../outside.csv), [folder](evidence) and \
         `[code](report.md)`.\n\n[csv]: evidence/daily_counts_utc.csv"
    );
    let parts: Vec<MessagePart> = serde_json::from_value(messages[0]["parts"].clone()).unwrap();
    let files: Vec<_> = parts
        .into_iter()
        .filter_map(|part| match part {
            MessagePart::File { file } => Some(file),
            _ => None,
        })
        .collect();
    assert_eq!(
        files
            .iter()
            .map(|file| (file.name.as_str(), file.media_type.as_str()))
            .collect::<Vec<_>>(),
        [
            ("daily_counts_utc.csv", "text/csv"),
            ("report.md", "text/markdown"),
            ("evidence.zip", "application/zip"),
            ("chart.png", "image/png"),
            ("daily_counts_utc.csv", "text/csv"),
            ("chart.png", "image/png"),
        ]
    );
    assert_eq!(files[0].id, files[4].id);
    assert_eq!(files[3].id, files[5].id);
    let saved = else_api::db::attachments::read(&app.db.pool, bot.parse().unwrap(), files[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.1, csv);
}

#[tokio::test(flavor = "multi_thread")]
async fn reading_a_reply_as_streamed_survives_attaching_its_files() {
    let directory = tempfile::tempdir().unwrap();
    let streamed =
        "The [2026-11-09T1845-observations.md](notes/2026-11-09T1845-observations.md) is saved.";
    let app = app(
        directory.path(),
        vec![streamed.into()],
        TurnOutcome::Completed,
    )
    .await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir_all(project.join("notes")).unwrap();
    std::fs::write(
        project.join("notes/2026-11-09T1845-observations.md"),
        "# Notes",
    )
    .unwrap();
    send(&app, &bot, "completed").await;
    let reply = replies(&app, &bot).await.pop().unwrap();
    let stored = reply["text"].as_str().unwrap();
    assert_eq!(
        stored,
        "The [File: 2026-11-09T1845-observations.md] is saved."
    );

    let read = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/read"),
            Some(serde_json::json!({
                "action": "through",
                "position": { "item_seq": reply["seq"], "text_bytes": streamed.len() },
            })),
        )
        .await;
    assert_eq!(read.status, StatusCode::OK, "{:?}", read.body);
    let state = &read.body["bot"]["read_state"];
    assert_eq!(state["unread"], false, "{state}");
    assert_eq!(
        state["read_through"], state["latest"]["position"],
        "{state}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_html_file_shown_as_an_image_attaches_as_a_visual_and_a_linked_one_as_a_download() {
    let directory = tempfile::tempdir().unwrap();
    let app = app(
        directory.path(),
        vec![
            "![Digest](viz/digest.html) [Report](report.html) ![Broken](empty.html) ![Text](notes.txt)"
                .into(),
        ],
        TurnOutcome::Completed,
    )
    .await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir_all(project.join("viz")).unwrap();
    let visual = r#"<div style="padding: 12px; background: var(--card)">Revenue $70.37</div>"#;
    std::fs::write(project.join("viz/digest.html"), visual).unwrap();
    std::fs::write(project.join("report.html"), visual).unwrap();
    std::fs::write(project.join("empty.html"), "").unwrap();
    std::fs::write(project.join("notes.txt"), "plain").unwrap();
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    assert_eq!(
        messages[0]["text"],
        "[File: digest.html] [File: report.html] ![Broken](empty.html) ![Text](notes.txt)"
    );
    let parts = messages[0]["parts"].as_array().unwrap();
    assert_eq!(parts[0]["file"]["media_type"], else_visual::MEDIA_TYPE);
    assert_eq!(parts[2]["file"]["media_type"], "text/html");
    let id = parts[0]["file"]["id"].as_str().unwrap().parse().unwrap();
    let (_, saved) = else_api::db::attachments::read(&app.db.pool, bot.parse().unwrap(), id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved, visual.as_bytes());
}

#[tokio::test(flavor = "multi_thread")]
async fn one_html_file_can_be_both_a_download_and_a_visual_in_either_order() {
    let directory = tempfile::tempdir().unwrap();
    let app = app(
        directory.path(),
        vec![
            "[Source](chart.html) ![Chart](chart.html)".into(),
            "![Chart](chart.html) [Source](chart.html)".into(),
        ],
        TurnOutcome::Completed,
    )
    .await;
    let bot = app.main_bot().await;
    let project = directory.path().join(&bot);
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("chart.html"),
        r#"<div style="height: 20px">Chart</div>"#,
    )
    .unwrap();
    send(&app, &bot, "completed").await;
    let messages = replies(&app, &bot).await;
    for (message, order) in messages.iter().zip([
        ["text/html", else_visual::MEDIA_TYPE],
        [else_visual::MEDIA_TYPE, "text/html"],
    ]) {
        let files: Vec<&Value> = message["parts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|part| part["type"] == "file")
            .collect();
        let types: Vec<&str> = files
            .iter()
            .map(|part| part["file"]["media_type"].as_str().unwrap())
            .collect();
        assert_eq!(types, order, "{message}");
        assert_ne!(files[0]["file"]["id"], files[1]["file"]["id"], "{message}");
    }
}
