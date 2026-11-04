//! POC-030: sidebar categories against SQLite and the real router.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};
use uuid::Uuid;

async fn create(app: &TestApp, request_id: Uuid, name: &str) -> common::Reply {
    app.owner(
        "POST",
        "/v1/categories",
        Some(json!({ "request_id": request_id, "name": name })),
    )
    .await
}

async fn assign(app: &TestApp, bot: &str, category: Value) -> common::Reply {
    app.owner(
        "PATCH",
        &format!("/v1/bots/{bot}/category"),
        Some(json!({ "category_id": category })),
    )
    .await
}

async fn listing(app: &TestApp) -> Value {
    app.owner("GET", "/v1/bots", None).await.body
}

fn category_of(listing: &Value, bot: &str) -> Value {
    listing["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == bot)
        .map(|b| b["category_id"].clone())
        .unwrap_or(Value::String("missing".into()))
}

#[tokio::test(flavor = "multi_thread")]
async fn categories_are_created_once_named_uniquely_and_kept_when_empty() {
    let app = TestApp::installed().await;
    // Existing bots start unassigned.
    let main = app.main_bot().await;
    assert_eq!(category_of(&listing(&app).await, &main), Value::Null);

    let request = Uuid::new_v4();
    let made = create(&app, request, "  Family ").await;
    assert_eq!(made.status, StatusCode::CREATED, "{:?}", made.body);
    assert_eq!(made.body["category"]["name"], "Family");
    let id = made.body["category"]["id"].clone();
    // A matching retry returns the same category; a different one conflicts.
    let again = create(&app, request, "Family").await;
    assert_eq!(
        (again.status, again.body["category"]["id"].clone()),
        (StatusCode::OK, id.clone())
    );
    assert_eq!(
        create(&app, request, "Work").await.status,
        StatusCode::CONFLICT
    );
    // Names: unique regardless of case, trimmed, bounded, no control characters.
    let dup = create(&app, Uuid::new_v4(), "FAMILY").await;
    assert_eq!(
        (dup.status, dup.body["error"]["code"].clone()),
        (StatusCode::CONFLICT, json!("duplicate_category"))
    );
    for bad in ["", "   ", &"x".repeat(81), "tab\there"] {
        assert_eq!(
            create(&app, Uuid::new_v4(), bad).await.status,
            StatusCode::BAD_REQUEST,
            "{bad:?}"
        );
    }
    assert_eq!(
        create(&app, Uuid::new_v4(), &"é".repeat(80)).await.status,
        StatusCode::CREATED
    );
    // A request ID already used to create a bot is a conflict.
    let bot_request = Uuid::new_v4();
    let reply = app
        .owner(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": bot_request, "name": "B" })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(
        create(&app, bot_request, "Other").await.status,
        StatusCode::CONFLICT
    );

    // Empty categories are listed, ordered by name.
    create(&app, Uuid::new_v4(), "archive").await;
    let names: Vec<String> = app.owner("GET", "/v1/categories", None).await.body["categories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    // By the case-folded name's code points: the same on any collation.
    assert_eq!(
        names,
        vec!["archive".to_owned(), "Family".to_owned(), "é".repeat(80)]
    );
    assert_eq!(
        listing(&app).await["categories"].as_array().unwrap().len(),
        3
    );

    // Rename (to a free name; not onto another's).
    let renamed = app
        .owner(
            "PATCH",
            &format!("/v1/categories/{}", id.as_str().unwrap()),
            Some(json!({ "name": "Home" })),
        )
        .await;
    assert_eq!(
        (renamed.status, renamed.body["category"]["name"].clone()),
        (StatusCode::OK, json!("Home"))
    );
    let clash = app
        .owner(
            "PATCH",
            &format!("/v1/categories/{}", id.as_str().unwrap()),
            Some(json!({ "name": "ARCHIVE" })),
        )
        .await;
    assert_eq!(clash.status, StatusCode::CONFLICT);

    // Unicode case folding: Straße and STRASSE are one name, on create and
    // on rename.
    assert_eq!(
        create(&app, Uuid::new_v4(), "Straße").await.status,
        StatusCode::CREATED
    );
    let folded = create(&app, Uuid::new_v4(), "STRASSE").await;
    assert_eq!(folded.status, StatusCode::CONFLICT, "{:?}", folded.body);
    let other = create(&app, Uuid::new_v4(), "Weg").await.body["category"]["id"].clone();
    let renamed = app
        .owner(
            "PATCH",
            &format!("/v1/categories/{}", other.as_str().unwrap()),
            Some(json!({ "name": "strasse" })),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::CONFLICT);
    app.owner(
        "DELETE",
        &format!("/v1/categories/{}", other.as_str().unwrap()),
        None,
    )
    .await;
    let strasse = app.owner("GET", "/v1/categories", None).await.body["categories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "Straße")
        .unwrap()["id"]
        .clone();
    app.owner(
        "DELETE",
        &format!("/v1/categories/{}", strasse.as_str().unwrap()),
        None,
    )
    .await;

    // Request IDs are one namespace, in both directions.
    let shared = Uuid::new_v4();
    assert_eq!(
        create(&app, shared, "Shared").await.status,
        StatusCode::CREATED
    );
    let bot = app
        .owner(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": shared, "name": "Late" })),
        )
        .await;
    assert_eq!(
        bot.status,
        StatusCode::CONFLICT,
        "a category's request ID cannot create a bot"
    );
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": shared })),
        )
        .await;
    assert_eq!(stop.status, StatusCode::CONFLICT, "nor stop work");
    assert_eq!(
        create(&app, shared, "Shared").await.status,
        StatusCode::OK,
        "the category retry still works"
    );

    // Repeated startup keeps everything.
    let app = app.restart().await;
    else_api::db::migrate(&app.db.pool).await.unwrap();
    assert_eq!(
        app.owner("GET", "/v1/categories", None).await.body["categories"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn assignments_move_and_deleting_a_category_keeps_its_bots() {
    let app = TestApp::installed().await;
    let a = app.create_bot("Alpha").await;
    let b = app.create_bot("Beta").await;
    let work = create(&app, Uuid::new_v4(), "Work").await.body["category"]["id"].clone();
    let home = create(&app, Uuid::new_v4(), "Home").await.body["category"]["id"].clone();

    // Assign, move, unassign.
    let reply = assign(&app, &a, work.clone()).await;
    assert_eq!(
        (reply.status, reply.body["bot"]["category_id"].clone()),
        (StatusCode::OK, work.clone())
    );
    assign(&app, &b, work.clone()).await;
    assign(&app, &b, home.clone()).await;
    assert_eq!(category_of(&listing(&app).await, &b), home);
    assign(&app, &b, Value::Null).await;
    assert_eq!(category_of(&listing(&app).await, &b), Value::Null);
    // Unknown categories, malformed bodies, and missing bots are refused.
    let unknown = assign(&app, &a, json!(Uuid::new_v4())).await;
    assert_eq!(
        (unknown.status, unknown.body["error"]["code"].clone()),
        (StatusCode::NOT_FOUND, json!("category_not_found"))
    );
    assert_eq!(category_of(&listing(&app).await, &a), work, "unchanged");
    let malformed = app
        .owner("PATCH", &format!("/v1/bots/{a}/category"), Some(json!({})))
        .await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        assign(&app, &Uuid::new_v4().to_string(), work.clone())
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    // Some chat in Alpha, to check deletion keeps it.
    let sent = app
        .send(&a, Uuid::new_v4(), "hello", Some("synthetic"))
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["completed"]).await;
    let items_before: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items i JOIN conversations c ON c.id = i.conversation_id WHERE c.bot_id = ?1",
    )
    .bind(a.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();

    // Deleting a category unassigns its bots and touches nothing else.
    let deleted = app
        .owner(
            "DELETE",
            &format!("/v1/categories/{}", work.as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(deleted.status, StatusCode::OK, "{:?}", deleted.body);
    assert_eq!(deleted.body["unassigned_bots"], json!([a]));
    let after = listing(&app).await;
    assert_eq!(category_of(&after, &a), Value::Null);
    assert_eq!(after["bots"].as_array().unwrap().len(), 3, "every bot kept");
    let items_after: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM items i JOIN conversations c ON c.id = i.conversation_id WHERE c.bot_id = ?1",
    )
    .bind(a.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(items_after, items_before);
    assert_eq!(
        app.owner(
            "DELETE",
            &format!("/v1/categories/{}", work.as_str().unwrap()),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // A deleted bot cannot be assigned (nor revived by trying).
    assign(&app, &b, home.clone()).await;
    assert_eq!(
        app.owner("DELETE", &format!("/v1/bots/{b}"), None)
            .await
            .status,
        StatusCode::OK
    );
    let gone = assign(&app, &b, home.clone()).await;
    assert_eq!(gone.status, StatusCode::GONE, "{:?}", gone.body);
    let status: String = sqlx::query_scalar("SELECT status FROM bots WHERE id = ?1")
        .bind(b.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "deleted");
    // A create retried after its category was deleted says so.
    let request = Uuid::new_v4();
    let temp = create(&app, request, "Temp").await.body["category"]["id"].clone();
    app.owner(
        "DELETE",
        &format!("/v1/categories/{}", temp.as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(create(&app, request, "Temp").await.status, StatusCode::GONE);
}

#[tokio::test(flavor = "multi_thread")]
async fn categories_change_during_a_turn_and_survive_a_restart_without_interrupting_it() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Busy").await;
    let sent = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:slow] a long answer please",
            Some("synthetic"),
        )
        .await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let team = create(&app, Uuid::new_v4(), "Team").await.body["category"]["id"].clone();
    assert_eq!(
        assign(&app, &bot, team.clone()).await.status,
        StatusCode::OK
    );
    let status = app
        .wait_turn_for(&turn, &["completed"], std::time::Duration::from_secs(60))
        .await;
    assert_eq!(status, "completed", "the turn was not interrupted");
    let app = app.restart().await;
    let after = listing(&app).await;
    assert_eq!(category_of(&after, &bot), team);
    assert_eq!(after["categories"][0]["name"], "Team");
}

#[tokio::test(flavor = "multi_thread")]
async fn task_grants_cannot_change_categories() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Bot").await;
    let sent = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let token = app
        .scheduler
        .grants()
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![
                else_api::grants::Tool::Computer,
                else_api::grants::Tool::Delegate,
            ],
            else_api::grants::DEFAULT_TTL,
        )
        .await;
    for (method, path, body) in [
        (
            "POST",
            "/v1/categories".to_owned(),
            Some(json!({ "request_id": Uuid::new_v4(), "name": "X" })),
        ),
        (
            "PATCH",
            format!("/v1/bots/{bot}/category"),
            Some(json!({ "category_id": null })),
        ),
        ("GET", "/v1/categories".to_owned(), None),
    ] {
        let reply = app.call(method, &path, body, Some(&token)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert_eq!(
        app.owner("GET", "/v1/categories", None).await.body["categories"],
        json!([])
    );
}

/// An installation migrated to 0026 with bots and chat gains categories at
/// 0027 and 0028 without losing anything; every bot starts unassigned.
#[tokio::test(flavor = "multi_thread")]
async fn migrating_a_populated_installation_keeps_every_bot_unassigned() {
    let db = common::TestDb::empty().await;
    let mut older = sqlx::migrate!("./migrations");
    older.migrations = std::borrow::Cow::Owned(
        older
            .migrations
            .iter()
            .filter(|m| m.version < 27)
            .cloned()
            .collect(),
    );
    older.run(&db.pool).await.unwrap();
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let bot = Uuid::new_v4();
    let conversation = Uuid::new_v4();
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Old')")
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id, kind) VALUES (?1, ?2, 'owner')")
        .bind(conversation)
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    let before: Vec<(Uuid, String)> = sqlx::query_as("SELECT id, name FROM bots ORDER BY id")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    else_api::db::migrate(&db.pool).await.unwrap();
    else_api::db::migrate(&db.pool).await.unwrap();
    let after: Vec<(Uuid, String, Option<Uuid>)> =
        sqlx::query_as("SELECT id, name, category_id FROM bots ORDER BY id")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(after.len(), before.len());
    for ((id, name), (id2, name2, category)) in before.iter().zip(&after) {
        assert_eq!((id, name, *category), (id2, name2, None));
    }
    let conversations: i64 = sqlx::query_scalar("SELECT count(*) FROM conversations")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(conversations, 2, "the main bot's and the old bot's");
}

/// Concurrent deletes, assignments, Stop work, and bot deletions never
/// deadlock into a server error.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_category_and_bot_changes_never_fail_with_server_errors() {
    let app = std::sync::Arc::new(TestApp::installed().await);
    for round in 0..25 {
        let a = app.create_bot(&format!("A{round}")).await;
        let b = app.create_bot(&format!("B{round}")).await;
        let c = create(&app, Uuid::new_v4(), &format!("C{round}"))
            .await
            .body["category"]["id"]
            .clone();
        assign(&app, &a, c.clone()).await;
        assign(&app, &b, c.clone()).await;
        let mut tasks = Vec::new();
        for kind in 0..5 {
            let app = app.clone();
            let (a, b, c) = (a.clone(), b.clone(), c.clone());
            tasks.push(tokio::spawn(async move {
                match kind {
                    0 => {
                        app.owner(
                            "DELETE",
                            &format!("/v1/categories/{}", c.as_str().unwrap()),
                            None,
                        )
                        .await
                        .status
                    }
                    1 => assign(&app, &a, c.clone()).await.status,
                    2 => {
                        app.owner(
                            "POST",
                            &format!("/v1/bots/{a}/cancel"),
                            Some(json!({ "request_id": Uuid::new_v4() })),
                        )
                        .await
                        .status
                    }
                    3 => {
                        app.owner("DELETE", &format!("/v1/bots/{b}"), None)
                            .await
                            .status
                    }
                    _ => assign(&app, &b, Value::Null).await.status,
                }
            }));
        }
        for task in tasks {
            let status = task.await.unwrap();
            assert!(!status.is_server_error(), "round {round}: {status}");
        }
    }
    // Whatever won, no bot points at a missing category.
    let dangling: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM bots b LEFT JOIN bot_categories c ON c.id = b.category_id
         WHERE b.category_id IS NOT NULL AND (c.id IS NULL OR c.deleting)",
    )
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(dangling, 0);
}
