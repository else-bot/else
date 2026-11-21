//! What a finished child returns to its parent: its final message, its
//! messages as a file, and the calls it made before a failure.

use super::*;

async fn settle_with_messages(db: &TestDb, child: Uuid, messages: &[&str], status: &str) {
    let (target, conversation): (Uuid, Uuid) = sqlx::query_as(
        "SELECT t.bot_id, c.id FROM turns t
         JOIN conversations c ON c.bot_id = t.bot_id AND c.kind = 'owner' WHERE t.id = ?1",
    )
    .bind(child)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    for text in messages {
        let seq: i64 = sqlx::query_scalar(
            "UPDATE conversations SET last_item_seq = last_item_seq + 1 WHERE id = ?1
             RETURNING last_item_seq",
        )
        .bind(conversation)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
             VALUES (?1, ?2, ?3, ?4, 'assistant', ?5)",
        )
        .bind(Uuid::new_v4())
        .bind(conversation)
        .bind(seq)
        .bind(child)
        .bind(text)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    common::seed::force_status(&db.pool, child, TurnStatus::Running).await;
    // End the task's turn as its provider does; the trigger records the result.
    sqlx::query(
        "UPDATE turns SET status = ?2,
             error_code = CASE WHEN ?2 = 'failed' THEN 'provider_error' END,
             error_message = CASE WHEN ?2 = 'failed' THEN 'The provider exited.' END
         WHERE id = ?1",
    )
    .bind(child)
    .bind(status)
    .execute(&db.pool)
    .await
    .unwrap();
    let mut tx = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::events::turn_changed(&mut tx, target, child)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn returned_text(db: &TestDb, owner: Uuid, file: Uuid) -> String {
    let data: Vec<u8> =
        sqlx::query_scalar("SELECT data FROM attachments WHERE id = ?1 AND bot_id = ?2")
            .bind(file)
            .bind(owner)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    String::from_utf8(data).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_multi_message_child_returns_its_final_message_and_every_message_as_a_file() {
    let db = TestDb::migrated().await;
    let (main, root, child, delegation) = seed(&db).await;
    let messages = [
        "Cloning the repo.",
        "Main, please rerun harvest.js once I finish.",
        "Final report: 3 failing tests, all in parser.rs.",
    ];
    settle_with_messages(&db, child, &messages, "completed").await;

    let mut conn = db.pool.acquire().await.unwrap();
    let outcomes = else_api::db::delegations::undelivered(&mut conn, root)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].result.as_deref(), Some(messages[2]));
    assert_eq!(outcomes[0].error, None);
    assert!(outcomes[0].files.is_empty(), "{:?}", outcomes[0].files);
    let file = outcomes[0]
        .messages
        .as_ref()
        .expect("every message as a file");
    assert_eq!(file.name, format!("task-{delegation}-messages.md"));
    assert_eq!(
        returned_text(&db, main, file.id).await,
        messages.join("\n\n"),
        "the request made mid-task is in the full text"
    );
    let shown: String = sqlx::query_scalar("SELECT text FROM items WHERE id = ?1")
        .bind(delegation)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        shown, messages[2],
        "the delegating chat shows the final report"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_child_returns_its_last_checkpoint_and_the_error() {
    let db = TestDb::migrated().await;
    let (main, root, child, _) = seed(&db).await;
    let messages = ["Run 1 done.", "Run 2 done. Starting run 3."];
    settle_with_messages(&db, child, &messages, "failed").await;

    let mut conn = db.pool.acquire().await.unwrap();
    let outcomes = else_api::db::delegations::undelivered(&mut conn, root)
        .await
        .unwrap();
    assert_eq!(
        outcomes[0].status,
        else_api::db::delegations::Status::Failed
    );
    assert_eq!(outcomes[0].result.as_deref(), Some(messages[1]));
    assert_eq!(
        outcomes[0].error.as_deref(),
        Some("provider_error: The provider exited.")
    );
    let file = outcomes[0]
        .messages
        .as_ref()
        .expect("every message as a file");
    assert_eq!(
        returned_text(&db, main, file.id).await,
        messages.join("\n\n")
    );
}

async fn approved_call_ran(
    db: &TestDb,
    turn: Uuid,
    title: &str,
    at: chrono::DateTime<chrono::Utc>,
) {
    sqlx::query(
        "INSERT INTO owner_questions (id, bot_id, turn_id, attempt, kind, title, choices, action,
             status, answer, consumed_attempt, used_at, use_outcome)
         SELECT ?1, bot_id, id, 1, 'external', ?3, '[]',
                json_object('tool', 'mcp_request', 'args', json_object()), 'answered',
                '\"approve\"', 1, ?4, 'completed'
         FROM turns WHERE id = ?2",
    )
    .bind(Uuid::new_v4())
    .bind(turn)
    .bind(title)
    .bind(at.format("%Y-%m-%dT%H:%M:%S%.3f+00:00").to_string())
    .execute(&db.pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_child_names_the_approved_calls_it_made() {
    let db = TestDb::migrated().await;
    let (_, root, child, _) = seed(&db).await;
    let at = chrono::Utc::now() - chrono::Duration::hours(1);
    approved_call_ran(&db, child, "Merge the news recap", at).await;
    approved_call_ran(
        &db,
        child,
        "Merge last week's recap",
        chrono::Utc::now() - chrono::Duration::days(8),
    )
    .await;
    settle_with_messages(&db, child, &["Merging now."], "failed").await;

    let mut conn = db.pool.acquire().await.unwrap();
    let outcomes = else_api::db::delegations::undelivered(&mut conn, root)
        .await
        .unwrap();
    let ran = &outcomes[0].ran;
    assert_eq!(
        ran.len(),
        1,
        "a call older than a week is left out: {ran:?}"
    );
    assert_eq!(ran[0].tool, "mcp_request");
    assert_eq!(ran[0].title, "Merge the news recap");
    assert_eq!(
        ran[0].used.describe(),
        format!(
            "completed at {}",
            at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        "the delegating bot learns the merge ran despite the failure"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_message_child_returns_no_extra_file() {
    let db = TestDb::migrated().await;
    let (_, root, child, _) = seed(&db).await;
    settle_with_messages(&db, child, &["Done: 3 failing tests."], "completed").await;

    let mut conn = db.pool.acquire().await.unwrap();
    let outcomes = else_api::db::delegations::undelivered(&mut conn, root)
        .await
        .unwrap();
    assert_eq!(
        outcomes[0].result.as_deref(),
        Some("Done: 3 failing tests.")
    );
    assert_eq!(outcomes[0].messages, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_updated_task_keeps_its_earlier_attempts_ask() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let work = leaf(app, "Work").await;
    let (task, first, token) =
        task_carried_to_a_follow_up(&h, &work, "[synthetic:hang] Triage the checkout 500s.").await;
    let (_, first_token) = h.started(first).await;
    let asked = tool(
        app,
        &first_token,
        "notify_owner",
        json!({ "kind": "decision", "summary": "Roll back the deploy or hotfix it?" }),
    )
    .await;
    assert_eq!(asked.result["status"], "pending", "{}", asked.body);

    let updated = tool(
        app,
        &token,
        "delegation_update",
        json!({ "task_id": task, "message": format!("{GATE} Recheck every EU checkout error from this morning.") }),
    )
    .await;
    assert!(!updated.error, "{}", updated.body);
    let next = child_of(app, task).await;
    assert_ne!(next, first);
    let (_, next_token) = h.started(next).await;
    let quiet = tool(
        app,
        &next_token,
        "notify_owner",
        json!({ "kind": "nothing" }),
    )
    .await;
    assert!(
        quiet.error,
        "the new attempt cannot hide the earlier ask: {}",
        quiet.body
    );
    assert!(quiet.code() == "ASK_PENDING", "{}", quiet.body);

    h.release(next).await;
    wait_task(app, task, "completed").await;
    let main = app.main_bot().await;
    let page = app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await;
    let reply = page.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == task.to_string())
        .unwrap_or_else(|| panic!("no reply item: {}", page.body))["bot_reply"]
        .clone();
    let asks = reply["asks"].as_array().unwrap();
    assert_eq!(asks.len(), 1, "{reply}");
    assert_eq!(asks[0]["id"], asked.result["notice_id"]);
    assert_eq!(asks[0]["status"], "pending");
}
