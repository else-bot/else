//! Which turn runs next, for one bot and across bots.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn bots_keep_separate_histories_and_order() {
    let app = TestApp::installed().await;
    let a = app.main_bot().await;
    let b = app.create_bot("Work").await;
    for round in [[(&a, "a1"), (&b, "b1")], [(&a, "a2"), (&b, "b2")]] {
        let mut turns = Vec::new();
        for (bot, text) in round {
            let reply = app.send(bot, Uuid::new_v4(), text, Some("synthetic")).await;
            assert_eq!(reply.status, StatusCode::ACCEPTED);
            turns.push(reply.body["turn"]["id"].as_str().unwrap().to_owned());
        }
        for turn in &turns {
            app.wait_turn(turn, &["completed"]).await;
        }
    }
    let last = app.send(&a, Uuid::new_v4(), "a3", Some("synthetic")).await;
    app.wait_turn(last.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let texts = |history: Vec<(i64, String, String)>| -> Vec<String> {
        history
            .into_iter()
            .map(|(_, kind, text)| format!("{kind}:{text}"))
            .collect()
    };
    let a_history = texts(items(&app, &a).await);
    assert_eq!(a_history.len(), 6);
    let a_users: Vec<_> = a_history
        .iter()
        .filter(|l| l.starts_with("user:"))
        .collect();
    assert_eq!(a_users, vec!["user:a1", "user:a2", "user:a3"]);
    for (n, text) in [(1, "a1"), (2, "a2"), (3, "a3")] {
        let reply = format!("assistant:Synthetic reply ✓ from Assistant (turn {n}): {text}");
        assert!(a_history.contains(&reply), "{a_history:?}");
    }
    let b_history = texts(items(&app, &b).await);
    assert_eq!(b_history.len(), 4);
    assert!(
        b_history
            .iter()
            .all(|line| !line.contains(": a") && !line.contains(":a"))
    );
    assert!(b_history.contains(&"assistant:Synthetic reply ✓ from Work (turn 2): b2".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn one_active_turn_per_bot_while_newer_messages_steer() {
    let h = recorder::harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (first, _) = h.running(&bot, &format!("{GATE} m0")).await;
    let pool = app.db.pool.clone();
    let done = tokio_util::sync::CancellationToken::new();
    let monitor = tokio::spawn({
        let done = done.clone();
        async move {
            let mut max_active = 0i64;
            while !done.is_cancelled() {
                let active: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM turns WHERE status IN ('preparing','sending','running','cancelling')",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                max_active = max_active.max(active);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            max_active
        }
    });
    let second = h.send(&bot, "m1").await;
    let third = h.send(&bot, "m2").await;
    h.release(first).await;
    for turn in [first, second, third] {
        assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");
    }
    done.cancel();
    assert_eq!(
        monitor.await.unwrap(),
        1,
        "never more than one active turn for the bot"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rapid_corrections_all_join_the_running_reply() {
    let h = recorder::harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let other = app.create_bot("Other").await;
    let (elsewhere, _) = h.running(&other, &format!("{GATE} elsewhere")).await;
    let (turn, _) = h
        .running(&bot, &format!("{GATE} publish the release notes"))
        .await;
    let answer = turn.to_string();

    let corrections: Vec<String> = Vec::from_fn(6, |i| format!("correction {i}"));
    let mut requests = Vec::new();
    for text in &corrections {
        let request = Uuid::new_v4();
        let reply = app.send(&bot, request, text, Some("synthetic")).await;
        assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.body);
        assert_eq!(reply.body["turn"]["steered_into"], answer.as_str());
        requests.push((
            request,
            reply.body["turn"]["id"].as_str().unwrap().to_owned(),
        ));
    }
    h.release(turn).await;
    assert_eq!(app.wait_turn(&answer, &["completed"]).await, "completed");
    for (request, turn) in &requests {
        assert_eq!(app.wait_turn(turn, &["completed"]).await, "completed");
        let lookup = app
            .owner(
                "GET",
                &format!("/v1/bots/{bot}/turns/by-request/{request}"),
                None,
            )
            .await;
        assert_eq!(lookup.body["turn"]["steered_into"], answer.as_str());
    }
    let packet = h.packets().await.last().unwrap().clone();
    for text in &corrections {
        assert!(
            packet.contains(text.as_str()),
            "{text} missing from {packet}"
        );
    }
    let owner_messages = items(app, &bot)
        .await
        .into_iter()
        .filter(|(_, kind, _)| kind == "user")
        .count();
    assert_eq!(owner_messages, 7, "every message is kept in history");
    let other_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(elsewhere)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(other_status, "running", "another bot's work is untouched");
    h.release(elsewhere).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_of_an_older_message_does_not_preempt_the_newer_one() {
    let app = TestApp::installed().await;
    let bot = app.main_bot().await;
    let first_request = Uuid::new_v4();
    let first = app
        .send(
            &bot,
            first_request,
            "[synthetic:hang] first",
            Some("synthetic"),
        )
        .await;
    let first = first.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&first, &["running"]).await;
    let second = app
        .send(
            &bot,
            Uuid::new_v4(),
            "[synthetic:hang] second",
            Some("synthetic"),
        )
        .await;
    assert_eq!(second.body["turn"]["steered_into"], first.as_str());
    let second = second.body["turn"]["id"].as_str().unwrap().to_owned();

    let retry = app
        .send(
            &bot,
            first_request,
            "[synthetic:hang] first",
            Some("synthetic"),
        )
        .await;
    assert_eq!(retry.status, StatusCode::OK);
    assert_eq!(retry.body["turn"]["id"], first.as_str());
    let status = |turn: &str| {
        sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id = ?1")
            .bind(turn.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
    };
    common::never_polled(
        "the newer message leaves the running reply after a retry",
        Duration::from_millis(300),
        || async {
            let first_status = status(&first).await.unwrap();
            let second_status = status(&second).await.unwrap();
            (first_status != "running" || second_status != "queued")
                .then_some((first_status, second_status))
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn different_bots_run_concurrently_within_the_global_limit() {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let app = TestApp::builder().db(db).max_active(2).start().await;
    let bots = [
        app.main_bot().await,
        app.create_bot("B").await,
        app.create_bot("C").await,
    ];
    for bot in &bots {
        app.send(bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
            .await;
    }
    let counts = || {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT count(*) FILTER (WHERE status = 'running'),
                    count(*) FILTER (WHERE status = 'queued') FROM turns",
        )
        .fetch_one(&app.db.pool)
    };
    let settled = common::poll(|| async { (counts().await.unwrap() == (2, 1)).then_some(()) });
    if tokio::time::timeout(common::SETTLE, settled).await.is_err() {
        panic!(
            "timed out waiting for two running turns and one queued turn: {:?}",
            counts().await.unwrap()
        );
    }
    common::never_polled(
        "the queued turn leaves the queue while both slots are taken",
        Duration::from_millis(300),
        || async {
            let queued: i64 =
                sqlx::query_scalar("SELECT count(*) FROM turns WHERE status = 'queued'")
                    .fetch_one(&app.db.pool)
                    .await
                    .unwrap();
            (queued != 1).then_some(queued)
        },
    )
    .await;
}

#[tokio::test]
async fn a_turn_still_finishing_its_last_attempt_is_not_claimed_and_holds_its_bots_queue() {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let mut queued = Vec::new();
    for name in ["First", "Second"] {
        let bot = common::seed::bot(&db.pool, name).await;
        for turn_no in [1, 2] {
            let turn = common::seed::turn(bot.id)
                .turn_no(turn_no)
                .created_at(format!("2026-01-01T00:00:0{}.000+00:00", queued.len()))
                .insert(&db.pool)
                .await;
            queued.push(turn);
        }
    }
    let busy = [queued[0]];
    assert_eq!(
        else_api::db::turns::claim_next(&db.pool, &busy, true)
            .await
            .unwrap()
            .map(|claimed| claimed.id),
        Some(queued[2]),
        "another bot's turn starts"
    );
    assert_eq!(
        else_api::db::turns::claim_next(&db.pool, &busy, true)
            .await
            .unwrap()
            .map(|claimed| claimed.id),
        None,
        "the finishing turn and the later turn of its bot both wait"
    );
    // The claimed turn ends without running a provider.
    common::seed::force_status(&db.pool, queued[2], TurnStatus::Completed).await;
    assert_eq!(
        else_api::db::turns::claim_next(&db.pool, &[], true)
            .await
            .unwrap()
            .map(|claimed| claimed.id),
        Some(queued[0]),
        "once its task is gone the turn starts first in its bot's queue"
    );
}

#[tokio::test]
async fn an_owner_message_is_claimed_past_its_bots_queued_background_task() {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let pool = &db.pool;
    let turn = |bot: Uuid,
                conversation: Uuid,
                turn_no: i64,
                kind: ItemKind,
                status: TurnStatus,
                at: &'static str| {
        common::seed::turn(bot)
            .conversation(conversation)
            .turn_no(turn_no)
            .message(kind, "x")
            .status(status)
            .created_at(at)
            .insert(pool)
    };

    let scout = common::seed::bot(pool, "Scout").await;
    let routine = turn(
        scout.id,
        scout.owner,
        1,
        ItemKind::Routine,
        TurnStatus::Waiting,
        "2026-01-01T00:00:00.000+00:00",
    )
    .await;
    let bento = common::seed::bot(pool, "Bento").await;
    let task = common::seed::task_conversation(pool, bento.id).await;
    let delegated = turn(
        bento.id,
        task,
        1,
        ItemKind::User,
        TurnStatus::Queued,
        "2026-01-01T00:00:01.000+00:00",
    )
    .await;
    common::seed::delegation(routine, scout.id, bento.id, task, delegated)
        .task("sweep")
        .insert(pool)
        .await;
    let message = turn(
        bento.id,
        bento.owner,
        2,
        ItemKind::User,
        TurnStatus::Queued,
        "2026-01-01T00:00:02.000+00:00",
    )
    .await;
    let auto = common::seed::bot(pool, "Auto").await;
    turn(
        auto.id,
        auto.owner,
        1,
        ItemKind::Routine,
        TurnStatus::Queued,
        "2026-01-01T00:00:03.000+00:00",
    )
    .await;
    turn(
        auto.id,
        auto.owner,
        2,
        ItemKind::User,
        TurnStatus::Queued,
        "2026-01-01T00:00:04.000+00:00",
    )
    .await;

    let claim = || async {
        else_api::db::turns::claim_next(pool, &[], false)
            .await
            .unwrap()
            .map(|claimed| claimed.id)
    };
    assert_eq!(
        claim().await,
        Some(message),
        "the owner message skips the bot's queued background task"
    );
    assert_eq!(
        claim().await,
        None,
        "a message stays behind an earlier routine turn in its own conversation"
    );
}
