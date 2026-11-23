//! Owner messages that join a reply waiting on its tasks, and how follow-ups
//! route back up the chain.

use super::*;

async fn follow_up(db: &TestDb, bot: Uuid) -> Uuid {
    follow_up_with(db, bot, "synthetic", else_api::db::turns::Admission::Open).await
}

async fn follow_up_with(
    db: &TestDb,
    bot: Uuid,
    provider: &str,
    preemption: else_api::db::turns::Admission,
) -> Uuid {
    let accepted = else_api::db::turns::accept(
        &db.pool,
        else_api::db::turns::SendRequest {
            bot_id: bot,
            request_id: Uuid::new_v4(),
            text: "Sorry, I meant the other one.",
            hidden: false,
            parts: &[],
            reaction: None,
            requested_provider: Some(provider),
            provider_id: provider,
            uses_profile: false,
            open_draft: None,
            answers_notice: None,
        },
        preemption,
    )
    .await
    .unwrap();
    accepted.turn.id
}

#[tokio::test]
async fn owner_messages_join_a_task_waiting_on_its_card_only_on_its_provider() {
    let db = TestDb::migrated().await;
    let (_main, _root, child, _outbound) = seed(&db).await;
    common::seed::force_status(&db.pool, child, TurnStatus::Waiting).await;
    sqlx::query("UPDATE turns SET provider_id = 'synthetic' WHERE id = ?1")
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO secret_requests (id, bot_id, turn_id, attempt, name, reason, env_names)
         VALUES (?1, ?2, ?3, 1, 'api_key', 'To post.', '[\"API_KEY\"]')",
    )
    .bind(Uuid::new_v4())
    .bind(target)
    .bind(child)
    .execute(&db.pool)
    .await
    .unwrap();
    let open = else_api::db::turns::Admission::Open;

    let first = follow_up_with(&db, target, "synthetic", open).await;
    assert_eq!(steered_into(&db, first).await, Some(child));
    let child_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        child_status, "queued",
        "the task continues with the message"
    );

    let second = follow_up_with(&db, target, "synthetic", open).await;
    assert_eq!(
        steered_into(&db, second).await,
        Some(child),
        "the task still waits on its card before it is claimed again"
    );

    let other = follow_up_with(&db, target, "codex", open).await;
    assert_eq!(
        steered_into(&db, other).await,
        None,
        "a message for another provider queues"
    );
}

#[tokio::test]
async fn a_message_for_another_provider_skips_the_replys_handoff() {
    let db = TestDb::migrated().await;
    let (main, root, _child, _outbound) = seed(&db).await;
    common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
    let handoff = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO computer_handoffs (id, bot_id, turn_id, attempt, reason)
         SELECT ?1, bot_id, id, 1, 'Log in to the mail account.' FROM turns WHERE id = ?2",
    )
    .bind(handoff)
    .bind(root)
    .execute(&db.pool)
    .await
    .unwrap();
    let open = else_api::db::turns::Admission::Open;

    let codex = follow_up_with(&db, main, "codex", open).await;

    assert_eq!(steered_into(&db, codex).await, None);
    let status: String = sqlx::query_scalar("SELECT status FROM computer_handoffs WHERE id = ?1")
        .bind(handoff)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "skipped", "the owner typed over the request");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_joins_the_reply_that_receives_its_tasks_outcome() {
    let db = TestDb::migrated().await;
    let (main, root, child, delegation) = seed(&db).await;
    common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
    common::seed::force_status(&db.pool, child, TurnStatus::Completed).await;
    let follow = follow_up(&db, main).await;
    assert_eq!(steered_into(&db, follow).await, Some(root));
    let (carrier, delivered, root_status): (Uuid, Option<i64>, String) = sqlx::query_as(
        "SELECT d.root_turn_id, d.delivered_attempt, r.status
         FROM delegations d, turns r WHERE d.id = ?1 AND r.id = ?2",
    )
    .bind(delegation)
    .bind(root)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (carrier, delivered, root_status.as_str()),
        (root, None, "queued"),
        "the reply continues and receives the outcome with the message"
    );
    let own = undelivered(&db, root).await;
    assert_eq!(own.len(), 1);
    assert!(!own[0].earlier_request, "the reply's own task");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_continues_the_reply_while_a_task_it_started_asks_back() {
    let db = TestDb::migrated().await;
    let (main, root, child, outbound) = seed(&db).await;
    common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
    common::seed::force_status(&db.pool, child, TurnStatus::Waiting).await;
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let asker = common::seed::turn(target)
        .turn_no(1)
        .status(TurnStatus::Running)
        .attempt(1)
        .insert(&db.pool)
        .await;
    let inbound = else_api::db::delegations::create(
        &db.pool,
        asker,
        main,
        "check with main",
        None,
        &[],
        None,
    )
    .await
    .unwrap()
    .delegation_id;
    let asking: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(inbound)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    common::seed::force_status(&db.pool, asking, TurnStatus::Waiting).await;
    let question = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO owner_questions (id, bot_id, turn_id, attempt, kind, title, choices)
         VALUES (?1, ?2, ?3, 1, 'choice', 'Which channel?', '[]')",
    )
    .bind(question)
    .bind(main)
    .bind(asking)
    .execute(&db.pool)
    .await
    .unwrap();

    let follow = follow_up(&db, main).await;
    assert_eq!(
        steered_into(&db, follow).await,
        Some(root),
        "the owner's own unfinished reply takes the message"
    );
    let tasks: Vec<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT root_turn_id, status, takeover_turn_id FROM delegations
         WHERE id IN (?1, ?2) ORDER BY id = ?2",
    )
    .bind(outbound)
    .bind(inbound)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        tasks,
        [
            (root, "queued".to_owned(), None),
            (asker, "queued".to_owned(), None)
        ],
        "both tasks stay with the replies that started them"
    );
    let asked: String = sqlx::query_scalar("SELECT status FROM owner_questions WHERE id = ?1")
        .bind(question)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        asked, "skipped",
        "the chat shows the task's card, so the message skips it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_wakes_the_parent_while_another_task_still_runs() {
    let h = harness(TestApp::builder().max_active(3)).await;
    let app = &h.app;
    let growth = leaf(app, "Growth").await;
    let scout = leaf(app, "Scout").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let audit = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &growth, "task": "[synthetic:hang] Audit the indexing notice." }),
    )
    .await;
    let audit_id: Uuid = audit.result["task_id"].as_str().unwrap().parse().unwrap();
    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;

    let follow = app
        .send(
            &main,
            Uuid::new_v4(),
            "Also learn from Scout how to generate VIP4 redeem links.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(follow.body["turn"]["steered_into"], root.to_string());
    let token = h.continued(root, &token).await;
    let task = format!("{GATE} Explain how to generate VIP4 redeem links.");
    let links = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": scout, "task": task }),
    )
    .await;
    let links_id: Uuid = links.result["task_id"].as_str().unwrap().parse().unwrap();

    h.release(root).await;
    app.wait_turn(root, &["waiting"]).await;
    h.release(child_of(app, links_id).await).await;
    h.continued(root, &token).await;
    h.release(root).await;

    let deadline = std::time::Instant::now() + common::SETTLE;
    loop {
        let (delivered, attempt, status): (Option<i64>, i64, String) = sqlx::query_as(
            "SELECT d.delivered_attempt, t.attempt, t.status FROM delegations d, turns t
             WHERE d.id = ?1 AND t.id = ?2",
        )
        .bind(links_id)
        .bind(root)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
        if delivered.is_some() && status == "waiting" {
            assert_eq!(
                delivered,
                Some(attempt),
                "the latest attempt read Scout's reply"
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Scout's reply never reached the reply: {delivered:?} {status}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (carrier, status): (Uuid, String) =
        sqlx::query_as("SELECT root_turn_id, status FROM delegations WHERE id = ?1")
            .bind(audit_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(
        (carrier, status.as_str()),
        (root, "running"),
        "Growth keeps working"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_message_does_not_join_a_routine_run() {
    let db = TestDb::migrated().await;
    let (main, root, child, delegation) = seed_from(&db, ItemKind::Routine, "Weekly audit").await;
    common::seed::force_status(&db.pool, child, TurnStatus::Running).await;
    common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
    let follow = follow_up(&db, main).await;
    assert_eq!(
        steered_into(&db, follow).await,
        None,
        "an owner message would widen the routine's authority"
    );
    let state: (Uuid, String, String) = sqlx::query_as(
        "SELECT d.root_turn_id, c.status, r.status FROM delegations d, turns c, turns r
         WHERE d.id = ?1 AND c.id = ?2 AND r.id = ?3",
    )
    .bind(delegation)
    .bind(child)
    .bind(root)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        state,
        (root, "running".into(), "waiting".into()),
        "the routine and its task go on"
    );
}

/// Issue: Tech Manager, delegated work by Marketing Manager, could not hand
/// it to Dev and scheduled a reminder whose run delegated instead. A task
/// now hands work on once; the last hop routes follow-ups back up.
#[tokio::test(flavor = "multi_thread")]
async fn a_task_hands_work_on_once_and_the_last_hop_routes_follow_ups_back() {
    let h = harness(common::TestApp::builder().max_active(3)).await;
    let main = h.app.main_bot().await;
    let (manager, dev, reviewer) = tokio::join!(
        h.app.create_bot("Manager"),
        h.app.create_bot("Dev"),
        h.app.create_bot("Reviewer")
    );
    let pool = h.app.db.pool.clone();
    let child_of_turn = |turn: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1",
            )
            .bind(turn)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };

    let (root, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let triage = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": manager,
                "task": format!("{GATE_EVERY_ATTEMPT} Triage the checkout 500s; hand the fix to Dev.") }),
    )
    .await;
    assert_eq!(triage.result["status"], "pending", "{}", triage.body);
    let task = child_of_turn(root).await;
    h.release(root).await;

    let (_, task_token) = h.started(task).await;
    let back_up = tool(
        &h.app,
        &task_token,
        "delegate",
        json!({ "target_bot_id": main, "task": "Do the triage yourself." }),
    )
    .await;
    assert!(
        back_up.error
            && back_up
                .result
                .as_str()
                .is_some_and(|e| e.starts_with("UPSTREAM_TARGET")),
        "{}",
        back_up.body
    );
    let fix = tool(
        &h.app,
        &task_token,
        "delegate",
        json!({ "target_bot_id": dev,
                "task": format!("{GATE} Implement the checkout fix Manager's triage found.") }),
    )
    .await;
    assert_eq!(fix.result["status"], "pending", "{}", fix.body);
    let dev_task = child_of_turn(task).await;
    h.release(task).await;

    let (_, dev_token) = h.started(dev_task).await;
    let refused = tool(
        &h.app,
        &dev_token,
        "delegate",
        json!({ "target_bot_id": reviewer, "task": "Review the checkout fix." }),
    )
    .await;
    assert!(refused.error, "{}", refused.body);
    assert!(
        refused.result["error"]
            .as_str()
            .unwrap()
            .starts_with("NOT_ALLOWED"),
        "{}",
        refused.body
    );
    let next = refused.result["next"].as_str().unwrap();
    assert!(
        next.contains("bot_id") && next.contains("Manager"),
        "the last hop hands follow-ups to the bot that delegated it: {next}"
    );
    h.release(dev_task).await;

    let task_token = h.continued(task, &task_token).await;
    h.release(task).await;
    let _ = task_token;
    h.continued(root, &token).await;
    h.release(root).await;
    h.app.wait_turn(root, &["completed"]).await;

    let hops: Vec<(Uuid, Uuid, String, Option<i32>)> = sqlx::query_as(
        "SELECT parent_bot_id, target_bot_id, status, delivered_attempt FROM delegations
         ORDER BY created_at",
    )
    .fetch_all(&h.app.db.pool)
    .await
    .unwrap();
    let [main, manager, dev] = [&main, &manager, &dev].map(|id| id.parse::<Uuid>().unwrap());
    assert_eq!(
        hops,
        [
            (main, manager, "completed".to_owned(), Some(2)),
            (manager, dev, "completed".to_owned(), Some(2)),
        ],
        "each hop ran once and its result was read once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_sent_during_a_pause_joins_only_once_it_ends() {
    let db = TestDb::migrated().await;
    let (main, root, child, delegation) = seed(&db).await;
    let follow = follow_up_with(
        &db,
        main,
        "synthetic",
        else_api::db::turns::Admission::Deferred,
    )
    .await;
    let state = || async {
        sqlx::query_as::<_, (Uuid, String, String, bool)>(
            "SELECT d.root_turn_id, r.status, f.status, f.deferred
             FROM delegations d, turns r, turns f WHERE d.id = ?1 AND r.id = ?2 AND f.id = ?3",
        )
        .bind(delegation)
        .bind(root)
        .bind(follow)
        .fetch_one(&db.pool)
        .await
        .unwrap()
    };
    assert_eq!(
        state().await,
        (root, "running".into(), "queued".into(), true),
        "the running turn keeps its task"
    );
    assert_eq!(steered_into(&db, follow).await, None);
    assert!(
        else_api::db::turns::claim_next(&db.pool, &[], true)
            .await
            .unwrap()
            .is_none_or(|claimed| claimed.id != follow),
        "a deferred message is not claimed"
    );

    // During the pause the running turn parks on its task and asks the owner.
    common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
    let question = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO owner_questions (id, bot_id, turn_id, attempt, kind, title, choices)
         VALUES (?1, ?2, ?3, 1, 'choice', 'Which file?', '[]')",
    )
    .bind(question)
    .bind(main)
    .bind(root)
    .execute(&db.pool)
    .await
    .unwrap();
    common::seed::force_status(&db.pool, child, TurnStatus::Completed).await;
    sqlx::query("UPDATE delegations SET status = 'completed' WHERE id = ?1")
        .bind(delegation)
        .execute(&db.pool)
        .await
        .unwrap();

    assert_eq!(
        else_api::db::turns::settle_deferred(&db.pool)
            .await
            .unwrap()
            .0,
        1
    );
    assert_eq!(
        state().await,
        (root, "queued".into(), "queued".into(), false),
        "once the pause ends the message joins the reply, which continues"
    );
    assert_eq!(steered_into(&db, follow).await, Some(root));
    let skipped: String = sqlx::query_scalar("SELECT status FROM owner_questions WHERE id = ?1")
        .bind(question)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        skipped, "skipped",
        "the question asked during the pause no longer blocks it"
    );
    assert_eq!(
        else_api::db::turns::settle_deferred(&db.pool)
            .await
            .unwrap()
            .0,
        0,
        "settling again changes nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_while_the_owner_holds_a_delegated_bots_computer_keeps_the_task_waiting() {
    let h = harness(TestApp::builder().max_active(2)).await;
    let app = &h.app;
    let work = leaf(app, "Scout").await;
    let main = app.main_bot().await;
    let (root, token) = h.running(&main, GATE).await;
    let reply = tool(
        app,
        &token,
        "delegate",
        json!({ "target_bot_id": &work, "task": format!("{GATE} Check X replies.") }),
    )
    .await;
    let task_id: Uuid = reply.result["task_id"].as_str().unwrap().parse().unwrap();
    let child: Uuid = sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    h.release(root).await;
    let (_, child_token) = h.started(child).await;
    let asked = tool(
        app,
        &child_token,
        "computer_handoff",
        json!({ "reason": "Log in to X as @scout." }),
    )
    .await;
    let handoff = asked.result["handoff_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{}", asked.body))
        .to_owned();
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    let taken = app
        .owner(
            "POST",
            &format!("/v1/bots/{work}/computer-handoffs/{handoff}/take"),
            None,
        )
        .await;
    assert_eq!(taken.status, StatusCode::OK, "{}", taken.body);

    let sent = app
        .send(&work, Uuid::new_v4(), "Which account?", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let state: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(task_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_ne!(
        state, "completed",
        "a question is not a hand back; the task keeps waiting"
    );
    app.wait_turn(child, &["waiting"]).await;

    let back = app
        .owner(
            "POST",
            &format!("/v1/bots/{work}/computer-handoffs/{handoff}/hand-back"),
            None,
        )
        .await;
    assert_eq!(back.status, StatusCode::OK, "{}", back.body);
    app.wait_turn(child, &["completed"]).await;
    app.wait_turn(root, &["completed"]).await;
}
