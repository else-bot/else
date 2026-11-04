//! The owner's read position: bot replies become unread, reads persist and
//! never rewind, and an explicit unread mark survives automatic reads.

mod common;

use std::borrow::Cow;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use serde_json::{Value, json};
use sqlx::migrate::Migrator;
use uuid::Uuid;

async fn read_state(app: &TestApp, bot: &str) -> Value {
    let reply = app.owner("GET", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.body);
    reply.body["bot"]["read_state"].clone()
}

async fn mark(app: &TestApp, bot: &str, command: Value) -> common::Reply {
    app.owner("POST", &format!("/v1/bots/{bot}/read"), Some(command))
        .await
}

/// Send `text` with the synthetic provider and wait for the reply to finish.
async fn reply(app: &TestApp, bot: &str, text: &str) -> String {
    let sent = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{:?}", sent.body);
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["completed"]).await;
    turn
}

async fn assistant_items(app: &TestApp, bot: &str) -> Vec<Value> {
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    snapshot.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["kind"] == "assistant")
        .cloned()
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_is_unread_until_read_and_the_read_survives_a_restart() {
    let app = TestApp::installed().await;
    let main = app.main_bot().await;
    assert_eq!(read_state(&app, &main).await["unread"], false);

    let turn = reply(&app, &main, "hello ✓").await;
    let state = read_state(&app, &main).await;
    assert_eq!(state["unread"], true, "{state}");
    assert_eq!(state["marked_unread"], false);
    assert_eq!(state["latest"]["turn_id"], turn.as_str());
    let item = assistant_items(&app, &main).await.pop().unwrap();
    assert_eq!(state["latest"]["item_id"], item["id"]);
    assert_eq!(
        state["latest"]["position"],
        json!({ "item_seq": item["seq"], "text_bytes": item["text"].as_str().unwrap().len() }),
        "bytes, not characters"
    );
    assert!(
        state["latest"]["preview"]
            .as_str()
            .unwrap()
            .starts_with("Synthetic reply ✓"),
        "{state}"
    );

    let read = mark(
        &app,
        &main,
        json!({ "action": "through", "position": state["latest"]["position"] }),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{:?}", read.body);
    assert_eq!(read.body["bot"]["read_state"]["unread"], false);

    let app = app.restart().await;
    let after = read_state(&app, &main).await;
    assert_eq!(after["unread"], false);
    assert_eq!(after["read_through"], state["latest"]["position"]);

    let reopened = else_api::db::connect(&app.db.url).await.unwrap();
    let listing = else_api::db::bots::list(&reopened, false).await.unwrap();
    let saved = listing
        .bots
        .iter()
        .find(|b| b.id.to_string() == main)
        .unwrap()
        .read
        .state();
    assert!(!saved.unread, "a fresh connection reads the saved position");
}

#[tokio::test(flavor = "multi_thread")]
async fn reads_only_move_forward_and_must_point_into_a_reply() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Writer").await;
    reply(&app, &bot, "first").await;
    let first = read_state(&app, &bot).await["latest"]["position"].clone();
    reply(&app, &bot, "second").await;
    let second = read_state(&app, &bot).await["latest"]["position"].clone();
    assert_ne!(first, second);

    let through = |position: &Value| json!({ "action": "through", "position": position });
    for _ in 0..2 {
        let read = mark(&app, &bot, through(&second)).await;
        assert_eq!(read.status, StatusCode::OK, "repeating is harmless");
    }
    let stale = mark(&app, &bot, through(&first)).await;
    assert_eq!(stale.status, StatusCode::OK, "a late retry is accepted");
    assert_eq!(stale.body["bot"]["read_state"]["read_through"], second);
    assert_eq!(stale.body["bot"]["read_state"]["unread"], false);

    let seq = second["item_seq"].as_i64().unwrap();
    let bytes = second["text_bytes"].as_i64().unwrap();
    let owner_seq = seq - 1;
    for bad in [
        json!({ "item_seq": seq, "text_bytes": bytes + 1 }),
        json!({ "item_seq": owner_seq, "text_bytes": 1 }),
        json!({ "item_seq": seq + 50, "text_bytes": 1 }),
        json!({ "item_seq": seq, "text_bytes": -1 }),
    ] {
        let refused = mark(&app, &bot, through(&bad)).await;
        assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}");
        assert_eq!(refused.body["error"]["code"], "invalid_position");
    }
    let unknown = mark(&app, &Uuid::new_v4().to_string(), through(&second)).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    let deleted = app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    assert!(deleted.status.is_success());
    let gone = mark(&app, &bot, json!({ "action": "mark_unread" })).await;
    assert_eq!(gone.status, StatusCode::GONE);
}

#[tokio::test(flavor = "multi_thread")]
async fn text_appended_after_a_partial_read_is_unread_again() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Streamer").await;
    let sent = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:slow] ✓✓✓✓ long",
            Some("synthetic"),
        )
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    let mut partial = Value::Null;
    for _ in 0..200 {
        let state = read_state(&app, &bot).await;
        if state["latest"].is_object() {
            partial = state["latest"]["position"].clone();
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(partial.is_object(), "the reply started");
    let read = mark(
        &app,
        &bot,
        json!({ "action": "through", "position": partial }),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK, "{:?}", read.body);
    app.wait_turn(&turn, &["completed"]).await;

    let state = read_state(&app, &bot).await;
    assert_eq!(state["latest"]["position"]["item_seq"], partial["item_seq"]);
    assert!(
        state["latest"]["position"]["text_bytes"].as_i64() > partial["text_bytes"].as_i64(),
        "the same item grew: {state}"
    );
    assert_eq!(state["unread"], true, "the rest of the reply is unread");
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_messages_task_replies_and_failures_are_not_unread_replies() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Quiet").await;
    let empty = read_state(&app, &bot).await;
    assert_eq!(empty["latest"], Value::Null);
    assert_eq!(empty["unread"], false);

    let sent = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:refuse]",
            Some("synthetic"),
        )
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["failed"]).await;
    assert_eq!(read_state(&app, &bot).await, empty, "no reply was written");

    // A reply in the bot's task conversation belongs to delegated work.
    let bot_id: Uuid = bot.parse().unwrap();
    let (task, task_turn) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO conversations (id, bot_id, kind) VALUES (?1, ?2, 'task')")
        .bind(task)
        .bind(bot_id)
        .execute(&app.db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status)
         VALUES (?1, ?2, ?3, ?4, zeroblob(32), 1, ?5, 'synthetic', 'completed')",
    )
    .bind(task_turn)
    .bind(bot_id)
    .bind(task)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(&app.db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         VALUES (?1, ?2, 1, ?3, 'assistant', 'Delegated result')",
    )
    .bind(Uuid::new_v4())
    .bind(task)
    .bind(task_turn)
    .execute(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(read_state(&app, &bot).await, empty);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mark_unread_outlasts_automatic_reads_until_marked_read() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Flagged").await;
    let flagged = mark(&app, &bot, json!({ "action": "mark_unread" })).await;
    assert_eq!(
        flagged.status,
        StatusCode::OK,
        "an empty bot can be flagged"
    );
    assert_eq!(flagged.body["bot"]["read_state"]["unread"], true);
    let cleared = mark(&app, &bot, json!({ "action": "mark_read" })).await;
    assert_eq!(cleared.body["bot"]["read_state"]["unread"], false);

    reply(&app, &bot, "news").await;
    let latest = read_state(&app, &bot).await["latest"]["position"].clone();
    mark(
        &app,
        &bot,
        json!({ "action": "through", "position": latest }),
    )
    .await;
    mark(&app, &bot, json!({ "action": "mark_unread" })).await;
    // A read the client sent before the mark arrives after it.
    let late = mark(
        &app,
        &bot,
        json!({ "action": "through", "position": latest }),
    )
    .await;
    let state = &late.body["bot"]["read_state"];
    assert_eq!(state["unread"], true, "{state}");
    assert_eq!(state["marked_unread"], true);
    assert_eq!(
        state["read_through"], latest,
        "marking unread keeps the position"
    );

    let app = app.restart().await;
    assert_eq!(read_state(&app, &bot).await["unread"], true);
    let read = mark(
        &app,
        &bot,
        json!({ "action": "mark_read", "position": latest }),
    )
    .await;
    assert_eq!(read.status, StatusCode::OK);
    assert_eq!(read.body["bot"]["read_state"]["unread"], false);
    assert_eq!(read.body["bot"]["read_state"]["marked_unread"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn upgrading_counts_existing_history_as_read() {
    let db = TestDb::empty().await;
    let before = Migrator {
        migrations: Cow::Owned(
            else_api::db::MIGRATOR
                .migrations
                .iter()
                .filter(|m| m.version < 7)
                .cloned()
                .collect(),
        ),
        ..Migrator::DEFAULT
    };
    before.run(&db.pool).await.unwrap();
    let (bot, conversation) = (Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Old')")
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO conversations (id, bot_id, last_item_seq, last_turn_no) VALUES (?1, ?2, 3, 1)",
    )
    .bind(conversation)
    .bind(bot)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO installation (default_bot_id) VALUES (?1)")
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    let turn = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status)
         VALUES (?1, ?2, ?3, ?4, zeroblob(32), 1, ?5, 'synthetic', 'completed')",
    )
    .bind(turn)
    .bind(bot)
    .bind(conversation)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(&db.pool)
    .await
    .unwrap();
    for (seq, kind, text) in [
        (1, "user", "hi"),
        (2, "assistant", "Réponse ✓"),
        (3, "status", "done"),
    ] {
        sqlx::query(
            "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(Uuid::new_v4())
        .bind(conversation)
        .bind(seq)
        .bind(turn)
        .bind(kind)
        .bind(text)
        .execute(&db.pool)
        .await
        .unwrap();
    }

    else_api::db::migrate(&db.pool).await.unwrap();
    let listing = else_api::db::bots::list(&db.pool, false).await.unwrap();
    let state = listing.bots[0].read.state();
    assert!(!state.unread, "{state:?}");
    let position = else_api::db::reads::ReplyPosition {
        item_seq: 2,
        text_bytes: "Réponse ✓".len() as i64,
    };
    assert_eq!(state.read_through, position);
    assert_eq!(state.latest.unwrap().position, position);
}
