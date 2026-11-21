//! Guest jobs, input allowances, and browser approval cards.

use super::*;

/// While the desktop fails to start, shell commands that name the browser
/// still run, so the bot can diagnose and repair it; screen tools refuse
/// with the startup error and say how to inspect it.
#[tokio::test(flavor = "multi_thread")]
async fn shell_commands_run_while_the_desktop_fails_to_start() {
    let Setup { h, script, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Fixer").await;
    let (_, token) = h.running(&bot, GATE).await;
    script.fail_desktop.store(true, SeqCst);

    let ran = tool(
        app,
        &token,
        "computer_run",
        json!({ "command": "pgrep -af chromium", "effect": "local" }),
    )
    .await
    .result;
    assert_eq!(ran["exit_code"], 0, "{ran}");
    let job = tool(
        app,
        &token,
        "computer_job_start",
        json!({ "command": "chromium --version", "effect": "local", "purpose": "Chromium version" }),
    )
    .await.result;
    assert!(job["job_id"].is_string(), "{job}");

    let shot = tool(app, &token, "computer_screenshot", json!({})).await;
    assert!(shot.error, "{}", shot.body);
    let refusal = shot.result;
    let error = refusal["error"].as_str().unwrap();
    assert!(
        error.starts_with("DESKTOP_FAILED:") && error.contains("Xvfb did not start"),
        "{refusal}"
    );
    assert!(
        refusal["next"]
            .as_str()
            .is_some_and(|next| next.contains("computer_run")),
        "{refusal}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn category_changes_leave_guest_jobs_running() {
    let Setup { h, manager, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Jobs").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let else_api::computers::jobs::Started::Job(job) = manager
        .start_job(bot_id, None, "sleep 600", Duration::from_secs(900))
        .await
        .unwrap()
    else {
        panic!("not started")
    };
    let state = || async {
        sqlx::query_as::<_, (String, Option<chrono::DateTime<chrono::Utc>>)>(
            "SELECT j.status, l.released_at FROM guest_jobs j JOIN computer_leases l ON l.id = j.lease_id
             WHERE j.id = ?1",
        )
        .bind(job)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
    };
    assert_eq!(state().await, ("running".to_owned(), None));
    let made = app
        .owner(
            "POST",
            "/v1/categories",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Batch" })),
        )
        .await;
    let id = made.body["category"]["id"].clone();
    for category in [id.clone(), Value::Null, id.clone()] {
        let reply = app
            .owner(
                "PATCH",
                &format!("/v1/bots/{bot}/category"),
                Some(json!({ "category_id": category })),
            )
            .await;
        assert_eq!(reply.status, StatusCode::OK);
    }
    app.owner(
        "DELETE",
        &format!("/v1/categories/{}", id.as_str().unwrap()),
        None,
    )
    .await;
    assert_eq!(
        state().await,
        ("running".to_owned(), None),
        "the job and its lease are untouched"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn input_allowances_survive_counter_cleanup() {
    let Setup { h, manager, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Input").await;
    let (turn, _) = h.running(&bot, GATE).await;
    for expected in 1..=41 {
        assert_eq!(manager.count_input(turn).await.unwrap(), expected);
    }
    for _ in 0..10_001 {
        manager.count_input(Uuid::new_v4()).await.unwrap();
    }
    assert_eq!(manager.count_input(turn).await.unwrap(), 42);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_approval_card_names_the_control_a_snapshot_ref_points_at() {
    let Setup { h, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Browser").await;
    // A pending card parks its attempt, so each call gets a fresh turn,
    // stopped once its card is read.
    let card = |args: Value| {
        let (h, bot) = (&h, &bot);
        async move {
            let app = &h.app;
            let (turn, token) = h.running(bot, GATE).await;
            let question = tool(app, &token, "computer_browser", args).await.result;
            let id: Uuid = question["question_id"].as_str().unwrap().parse().unwrap();
            let context = sqlx::query_scalar::<_, String>(
                "SELECT context FROM owner_questions WHERE id = ?1",
            )
            .bind(id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
            stop(app, bot, turn).await;
            context
        }
    };
    let click = |selector: &str| json!({ "action": "click", "selector": selector, "effect": "destructive" });
    assert_eq!(
        card(click("@kqvzb5")).await,
        "Browser: click @kqvzb5",
        "before a snapshot the ref is shown as given"
    );
    let (turn, token) = h.running(&bot, GATE).await;
    let snapshot = tool(
        app,
        &token,
        "computer_browser",
        json!({ "action": "snapshot" }),
    )
    .await
    .result;
    assert!(
        snapshot["snapshot"].as_str().unwrap().contains("@kqvzb5"),
        "{snapshot}"
    );
    stop(app, &bot, turn).await;
    assert_eq!(
        card(click("@kqvzb5")).await,
        "Browser: click button \"Delete project\" (@kqvzb5)"
    );
    assert_eq!(card(click("@kqvzb6")).await, "Browser: click @kqvzb6");
}
