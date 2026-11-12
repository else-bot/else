use crate::common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use common::{TestApp, TestDb};
use else_api::db;
use else_api::providers::{Registry, synthetic::Synthetic};
use sqlx::types::Json;
use uuid::Uuid;

async fn fixture() -> (TestApp, Arc<Synthetic>) {
    let db = TestDb::migrated().await;
    db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(1)));
    let registry = Registry::new(vec![synthetic.clone()]);
    (TestApp::with_registry(Arc::new(db), 4, registry), synthetic)
}

async fn send(app: &TestApp, bot: &str, text: &str) -> Uuid {
    let sent = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    app.wait_turn(turn, &["completed"]).await;
    turn.parse().unwrap()
}

async fn active(app: &TestApp, bot: &str) -> (String, BTreeMap<String, String>) {
    let (native, Json(skills)) = sqlx::query_as::<_, (String, Json<BTreeMap<String, String>>)>(
        "SELECT native_session_id, default_skills FROM provider_sessions
         WHERE bot_id = ?1 AND status = 'active'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    (native, skills)
}

#[tokio::test(flavor = "multi_thread")]
async fn skills_migrate_on_the_next_message_and_persist_across_restart() {
    let (app, synthetic) = fixture().await;
    let bot = app.main_bot().await;
    send(&app, &bot, "First message.").await;
    let (native, skills) = active(&app, &bot).await;
    assert!(skills.contains_key("unslop"));
    assert!(synthetic.packets().await[0].contains("Avoid em dashes entirely."));

    sqlx::query("UPDATE provider_sessions SET default_skills = '{}' WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .execute(&app.db.pool)
        .await
        .unwrap();
    send(&app, &bot, "Migrate this existing conversation.").await;
    assert_eq!(active(&app, &bot).await, (native.clone(), skills.clone()));
    let packets = synthetic.packets().await;
    assert!(packets[1].contains("Default skill `unslop`"));
    assert!(packets[1].ends_with("Migrate this existing conversation."));

    send(&app, &bot, "An unchanged follow-up.").await;
    assert!(
        !synthetic
            .packets()
            .await
            .last()
            .unwrap()
            .contains("[else.bot default skills]")
    );

    let app = app.restart().await;
    let turn = send(&app, &bot, "Continue after restart.").await;
    assert_eq!(active(&app, &bot).await, (native, skills.clone()));
    let input = db::turns::input(&app.db.pool, turn).await.unwrap();
    assert_eq!(input.session.unwrap().default_skills, skills);
}

#[tokio::test(flavor = "multi_thread")]
async fn skills_are_not_acknowledged_while_delivery_is_uncertain() {
    let (app, synthetic) = fixture().await;
    let bot = app.main_bot().await;
    let sent = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:stall-send]",
            Some("synthetic"),
        )
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap();
    app.wait_turn(turn, &["sending"]).await;
    let (native, skills) = active(&app, &bot).await;
    assert!(skills.is_empty());
    send(&app, &bot, "Continue my request.").await;
    let (replacement, skills) = active(&app, &bot).await;
    assert_ne!(replacement, native);
    assert!(skills.contains_key("unslop"));
    let packets = synthetic.packets().await;
    assert!(packets.last().unwrap().contains("Default skill `unslop`"));
}

#[tokio::test]
async fn upgrading_an_old_database_preserves_native_sessions_and_history_position() {
    let db = TestDb::empty().await;
    db::MIGRATOR.run_to(18, &db.pool).await.unwrap();
    db::installation::setup(&db.pool).await.unwrap();
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, id FROM conversations LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO provider_sessions
         (id, bot_id, conversation_id, provider_id, native_session_id, fingerprint,
          considered_through_turn, memory_revision)
         VALUES (?1, ?2, ?3, 'codex', 'old-native-session', 'old-fingerprint', 42, 7)",
    )
    .bind(Uuid::new_v4())
    .bind(bot)
    .bind(conversation)
    .execute(&db.pool)
    .await
    .unwrap();

    db::migrate(&db.pool).await.unwrap();
    db::migrate(&db.pool).await.unwrap();
    let row: (String, String, i64, i64, Json<BTreeMap<String, String>>) = sqlx::query_as(
        "SELECT native_session_id, fingerprint, considered_through_turn, memory_revision, default_skills
         FROM provider_sessions WHERE bot_id = ?1 AND status = 'active'",
    )
    .bind(bot).fetch_one(&db.pool).await.unwrap();
    assert_eq!(row.0, "old-native-session");
    assert_eq!(row.1, "old-fingerprint");
    assert_eq!(row.2, 42);
    assert_eq!(row.3, 7);
    assert!(row.4.0.is_empty());
}
