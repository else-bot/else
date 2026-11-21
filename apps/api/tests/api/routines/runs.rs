//! How a routine run shows in history.

use super::*;

#[tokio::test]
async fn a_run_is_hidden_and_its_report_is_visible() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.running(&main, GATE).await;
    const TASK: &str = "Collect the overnight build results and list failures.";
    let routine = create(&h.app, &token, digest(TASK)).await;
    let id = routine["id"].as_str().unwrap().to_owned();
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;
    let selected: Option<String> =
        sqlx::query_scalar("SELECT selected_provider_id FROM bots WHERE id = ?1")
            .bind(main.parse::<Uuid>().unwrap())
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    let owner_entries = owner_memory(&h.app, &main).await;

    let due = at(&routine["next_run_at"]);
    assert_eq!(
        fire(&h.app, due - chrono::Duration::seconds(1)).await,
        0,
        "not due yet"
    );
    assert_eq!(fire(&h.app, due).await, 1);
    assert_eq!(fire(&h.app, due).await, 0, "one occurrence runs once");
    let run = runs(&h.app, &id).await;
    assert_eq!(run.len(), 1);
    let turn = run[0].0.unwrap();
    h.app.wait_turn(turn, &["completed"]).await;

    let packet = h.packets().await.into_iter().last().unwrap();
    assert!(
        packet.contains("[else.bot routine] Scheduled run of the routine \"Morning digest\""),
        "{packet}"
    );
    assert!(packet.contains(TASK));
    assert!(
        packet.contains("Where your rules conflict with this task's wording, follow your rules."),
        "{packet}"
    );
    assert!(!packet.contains("Owner's latest message"));

    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await
        .body;
    let items = snapshot["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["kind"] != "routine"));
    let report: Vec<&Value> = items
        .iter()
        .filter(|i| i["turn_id"] == turn.to_string().as_str())
        .collect();
    assert_eq!(report.len(), 1);
    assert_eq!(report[0]["kind"], "assistant");
    assert!(report[0]["text"].as_str().unwrap().contains(TASK));
    let visible_turn = snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .unwrap();
    assert_eq!(visible_turn["routine"]["routine_id"], routine["id"]);
    assert_eq!(visible_turn["routine"]["name"], "Morning digest");
    assert_eq!(at(&visible_turn["routine"]["due_at"]), due);
    assert_eq!(visible_turn["routine"]["reminder"], false);
    assert_eq!(snapshot["routines"][0]["last_run"]["status"], "completed");
    assert_eq!(
        snapshot["routines"][0]["last_run"]["turn_id"],
        turn.to_string().as_str()
    );
    let next = at(&snapshot["routines"][0]["next_run_at"]);
    assert!(next > due, "advanced past the run");

    let journal = events(&h.app, &main).await;
    let run_events: Vec<_> = journal
        .iter()
        .filter(|(kind, data)| kind == "turn.updated" && data["turn"]["id"] == turn.to_string())
        .collect();
    assert!(!run_events.is_empty());
    assert!(
        run_events
            .iter()
            .all(|(_, data)| data["turn"]["routine"] == visible_turn["routine"])
    );
    for (kind, data) in &journal {
        if kind == "item.upsert" {
            assert_ne!(data["item"]["kind"], "routine");
            assert!(!data["item"]["text"].as_str().unwrap().starts_with(TASK));
        }
    }
    let last = journal
        .iter()
        .rev()
        .find(|(kind, _)| kind == "bot.updated")
        .unwrap();
    assert_eq!(last.1["routines"][0]["id"], id.as_str());
    assert_eq!(last.1["routines"][0]["last_run"]["status"], "completed");

    let (_, owner_token) = h.running(&main, GATE).await;
    let history = h
        .app
        .call(
            "GET",
            "/tools/v1/history?limit=100",
            None,
            Some(&owner_token),
        )
        .await;
    assert!(
        history.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] != "routine")
    );
    let hidden: Uuid = sqlx::query_scalar("SELECT owner_item_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    let item = h
        .app
        .call(
            "GET",
            &format!("/tools/v1/items/{hidden}"),
            None,
            Some(&owner_token),
        )
        .await;
    assert_eq!(item.status, StatusCode::NOT_FOUND);

    assert_eq!(
        owner_memory(&h.app, &main).await,
        owner_entries + 1,
        "only the new owner message"
    );
    let logged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_entry WHERE body LIKE '%' || ?1 || '%' AND authority = 'owner'",
    )
    .bind(TASK)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    assert_eq!(logged, 0);
    let after: Option<String> =
        sqlx::query_scalar("SELECT selected_provider_id FROM bots WHERE id = ?1")
            .bind(main.parse::<Uuid>().unwrap())
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(after, selected);
}

async fn owner_memory(app: &TestApp, bot: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM memory_entry WHERE bot_id = ?1 AND authority = 'owner'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn rebuilt_history_labels_a_run_as_a_routine() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.running(&main, GATE).await;
    let routine = create(&h.app, &token, digest("Water the plants reminder.")).await;
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;
    fire(&h.app, at(&routine["next_run_at"])).await;
    let run = runs(&h.app, routine["id"].as_str().unwrap()).await[0]
        .0
        .unwrap();
    h.app.wait_turn(run, &["completed"]).await;
    let sessions: Vec<String> =
        sqlx::query_scalar("SELECT native_session_id FROM provider_sessions")
            .fetch_all(&h.app.db.pool)
            .await
            .unwrap();
    h.recorder.forget_sessions(&sessions);
    let sent = h.app.send(&main, Uuid::new_v4(), "Did it run?", None).await;
    h.app
        .wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let packet = h.packets().await.into_iter().last().unwrap();
    assert!(
        packet.contains(
            "Routine or webhook wake (a saved task, not an owner message): Water the plants reminder."
        ),
        "{packet}"
    );
    assert!(!packet.contains("Owner: Water the plants"));
    assert!(packet.ends_with("Owner's latest message:\nDid it run?"));
}
