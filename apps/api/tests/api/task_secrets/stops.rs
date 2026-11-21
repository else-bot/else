//! Stopping a bot's foreground commands.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn stop_work_ends_a_running_foreground_command() {
    let Setup {
        h,
        guest,
        script,
        manager,
    } = setup().await;
    let bot = h.app.create_bot("Worker").await;
    let (_, token) = h.running(&bot, GATE).await;
    let h = Arc::new(h);
    let app = &h.app;
    let call = {
        let h = h.clone();
        let token = token.clone();
        tokio::spawn(async move {
            tool(
                &h.app,
                &token,
                "computer_run",
                json!({ "command": "HANG for a long time", "timeout_secs": 300, "effect": "local" }),
            )
            .await.result
        })
    };
    common::until("the foreground command to start", script.started.notified()).await;
    manager.end_orphaned_commands().await.unwrap();
    assert!(!call.is_finished());
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{:?}", stop.body);
    common::until(
        "the running turn to stop",
        common::poll(|| async {
            let live: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM turns WHERE bot_id = ?1 AND status = 'running')",
            )
            .bind(bot.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
            (!live).then_some(())
        }),
    )
    .await;
    else_api::db::stop::settle_all(&app.db.pool).await.unwrap();
    let receipt = |state: &'static str| {
        let pool = app.db.pool.clone();
        async move {
            let states: Vec<String> =
                sqlx::query_scalar("SELECT state FROM request_receipts WHERE operation = 'cancel'")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            assert_eq!(states, vec![state.to_owned()]);
        }
    };
    receipt("pending").await;
    manager.end_orphaned_commands().await.unwrap();
    let result = common::until("the command to end", call).await.unwrap();
    assert_eq!(result["exit_code"], 143, "{result}");
    let kill = guest
        .execs()
        .into_iter()
        .find(|Exec { args, .. }| args.iter().any(|a| a.contains("alive()")))
        .unwrap();
    assert!(
        kill.args
            .last()
            .unwrap()
            .starts_with("/var/lib/elsebot/runs/"),
        "{:?}",
        kill.args
    );
    let ended: bool = sqlx::query_scalar("SELECT min(ended_at IS NOT NULL) FROM guest_runs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(ended, "the run is recorded as ended");
    common::until(
        "the cancellation receipt to settle",
        common::poll(|| async {
            else_api::db::stop::settle_all(&app.db.pool).await.unwrap();
            let done: bool = sqlx::query_scalar(
                "SELECT min(state = 'done') FROM request_receipts WHERE operation = 'cancel'",
            )
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
            done.then_some(())
        }),
    )
    .await;
    receipt("done").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_stalled_kill_does_not_delay_another_bots_stop() {
    let Setup {
        h, script, manager, ..
    } = setup().await;
    let slow = h.app.create_bot("Slow").await;
    let fast = h.app.create_bot("Fast").await;
    let h = Arc::new(h);
    let app = &h.app;
    let mut calls = Vec::new();
    for bot in [&slow, &fast] {
        let (_, token) = h.running(bot, GATE).await;
        let h = h.clone();
        calls.push(tokio::spawn(async move {
            tool(
                &h.app,
                &token,
                "computer_run",
                json!({ "command": "HANG", "timeout_secs": 300, "effect": "local" }),
            )
            .await
            .result
        }));
        common::until("the foreground command to start", script.started.notified()).await;
    }
    let slow_name = ComputerManager::runtime_name(slow.parse().unwrap());
    script.stalled.lock().unwrap().push(slow_name);
    for bot in [&slow, &fast] {
        let stop = app
            .owner(
                "POST",
                &format!("/v1/bots/{bot}/cancel"),
                Some(json!({ "request_id": Uuid::new_v4() })),
            )
            .await;
        assert!(stop.status.is_success());
        if bot == &slow {
            common::until(
                "the slow bot's kill to stall",
                script.kill_stalled.notified(),
            )
            .await;
        }
    }
    let pass = {
        let manager = manager.clone();
        tokio::spawn(async move { manager.end_orphaned_commands().await })
    };
    let fast_call = calls.pop().unwrap();
    let result = common::until("the other bot's command to end during the stall", fast_call)
        .await
        .unwrap();
    assert_eq!(result["exit_code"], 143);
    script.release_stalled_kill.notify_one();
    common::until("the orphan cleanup pass to finish", pass)
        .await
        .unwrap()
        .unwrap();
    let slow_call = calls.pop().unwrap();
    let result = common::until(
        "the stalled command to end once its kill answers",
        slow_call,
    )
    .await
    .unwrap();
    assert_eq!(result["exit_code"], 143);
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_stop_dropped_requests_and_stopped_turns_end_or_refuse_commands() {
    let Setup {
        h,
        guest,
        script,
        manager,
    } = setup().await;
    let bot = h.app.create_bot("Clicker").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let (_, token) = h.running(&bot, GATE).await;
    let h = Arc::new(h);
    let app = &h.app;
    let call = |command: &'static str| {
        let h = h.clone();
        let token = token.clone();
        tokio::spawn(async move {
            tool(
                &h.app,
                &token,
                "computer_run",
                json!({ "command": command, "timeout_secs": 300, "effect": "local" }),
            )
            .await
            .result
        })
    };
    let kills = || {
        guest
            .execs()
            .iter()
            .filter(|Exec { args, .. }| args.iter().any(|a| a.contains("alive()")))
            .count()
    };

    let first = call("HANG one");
    common::until("the foreground command to start", script.started.notified()).await;
    common::until("the owner stop to finish", manager.owner_stop(bot_id))
        .await
        .unwrap();
    let result = common::until("the stopped command to end", first)
        .await
        .unwrap();
    assert_eq!(result["exit_code"], 143, "{result}");
    manager.owner_resume(bot_id).await.unwrap();

    let before = kills();
    let second = call("HANG two");
    common::until("the foreground command to start", script.started.notified()).await;
    second.abort();
    common::until(
        "the dropped request's command to be killed",
        common::poll(|| std::future::ready((kills() != before).then_some(()))),
    )
    .await;

    let third = call("HANG three");
    common::until("the foreground command to start", script.started.notified()).await;
    let mut queued = call("echo should-not-run");
    common::never(
        "the queued command to finish while another holds the computer",
        Duration::from_millis(300),
        &mut queued,
    )
    .await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success());
    common::until(
        "the running turn to stop",
        common::poll(|| async {
            let live: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM turns WHERE bot_id = ?1 AND status = 'running')",
            )
            .bind(bot_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
            (!live).then_some(())
        }),
    )
    .await;
    manager.end_orphaned_commands().await.unwrap();
    let _ = common::until("the running command to end", third).await;
    let refused = common::until("the queued command to be refused", queued)
        .await
        .unwrap();
    assert!(
        refused.as_str().unwrap_or_default().contains("STOPPED"),
        "{refused}"
    );
    assert!(
        guest
            .execs()
            .iter()
            .all(|Exec { args, .. }| args.iter().all(|a| !a.contains("should-not-run"))),
        "the queued command never reached the guest"
    );
}
