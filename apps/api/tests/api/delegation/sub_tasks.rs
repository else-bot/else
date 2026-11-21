//! Tasks that hand part of their work on: cancellation, depth, the shared
//! budget, and how a child ending requeues its root.

use super::*;

/// Seed main -> Target, then Target's running task hands work to Third.
/// Returns (main, root, child, first delegation, grandchild, second delegation, Third).
async fn seed_chain(db: &TestDb) -> (Uuid, Uuid, Uuid, Uuid, Uuid, Uuid, Uuid) {
    seed_chain_from(db, ItemKind::User, "Find the form.").await
}

/// Seed the chain under main's reply to a `kind` message.
async fn seed_chain_from(
    db: &TestDb,
    kind: ItemKind,
    text: &str,
) -> (Uuid, Uuid, Uuid, Uuid, Uuid, Uuid, Uuid) {
    let (main, root, child, first) = seed_from(db, kind, text).await;
    common::seed::force_status(&db.pool, child, TurnStatus::Running).await;
    sqlx::query("UPDATE turns SET attempt = 1 WHERE id = ?1")
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
    let third = common::seed::bot(&db.pool, "Third").await.id;
    let second =
        else_api::db::delegations::create(&db.pool, child, third, "sub task", None, &[], None)
            .await
            .unwrap();
    let grandchild: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
            .bind(second.delegation_id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    (
        main,
        root,
        child,
        first,
        grandchild,
        second.delegation_id,
        third,
    )
}

async fn delegation_context(db: &TestDb, turn: Uuid) -> else_api::db::delegations::Context {
    let mut conn = db.pool.acquire().await.unwrap();
    else_api::db::delegations::context(&mut conn, turn)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_hands_part_of_its_work_on_and_reports_once_it_returns() {
    use else_api::db::delegations::{DelegateError, MAX_DEPTH, council, create};
    let db = TestDb::migrated().await;
    let (main, root, child, first, grandchild, second, third) = seed_chain(&db).await;
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();

    let middle = delegation_context(&db, child).await;
    assert_eq!(middle.task_depth, 1);
    assert!(middle.can_delegate());
    let listed: Vec<Uuid> = middle.registry.iter().map(|(id, _)| *id).collect();
    assert!(listed.contains(&third), "{listed:?}");
    assert!(
        !listed.contains(&main),
        "the bot above it is not offered: {listed:?}"
    );
    let leaf = delegation_context(&db, grandchild).await;
    assert_eq!(leaf.task_depth, MAX_DEPTH);
    assert!(!leaf.can_delegate());
    assert_eq!(leaf.delegated_by.map(|(id, _)| id), Some(target));

    let fourth = common::seed::bot(&db.pool, "Fourth").await.id;
    assert_eq!(
        create(&db.pool, child, main, "back to the top", None, &[], None).await,
        Err(DelegateError::UpstreamTarget)
    );
    assert_eq!(
        council(&db.pool, child, &[fourth], "a council from a task", None).await,
        Err(DelegateError::NotAllowed)
    );
    common::seed::force_status(&db.pool, grandchild, TurnStatus::Running).await;
    assert_eq!(
        create(&db.pool, grandchild, fourth, "a third hop", None, &[], None).await,
        Err(DelegateError::NotAllowed)
    );

    common::seed::force_status(&db.pool, child, TurnStatus::Waiting).await;
    common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
    // The task's turn ends; the delegation trigger settles its task.
    sqlx::query(
        "UPDATE turns SET status = 'completed',
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1",
    )
    .bind(grandchild)
    .execute(&db.pool)
    .await
    .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "queued", "the sub-task's end resumes the task");
    let outcomes: Vec<Uuid> = delegation_context(&db, child)
        .await
        .outcomes
        .iter()
        .map(|o| o.id)
        .collect();
    assert_eq!(outcomes, vec![second], "its continuation reads the result");
    let root_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(root)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(root_status, "waiting", "the top still waits on the task");

    // The task's turn ends; the delegation trigger settles its task.
    sqlx::query(
        "UPDATE turns SET status = 'completed',
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1",
    )
    .bind(child)
    .execute(&db.pool)
    .await
    .unwrap();
    let (root_status, first_status): (String, String) = sqlx::query_as(
        "SELECT t.status, d.status FROM delegations d JOIN turns t ON t.id = d.root_turn_id
         WHERE d.id = ?1",
    )
    .bind(first)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (root_status.as_str(), first_status.as_str()),
        ("queued", "completed")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_task_stops_the_work_it_handed_on() {
    let db = TestDb::migrated().await;
    let (_, root, child, first, grandchild, second, _) = seed_chain(&db).await;
    common::seed::force_status(&db.pool, child, TurnStatus::Waiting).await;
    else_api::db::delegations::cancel(&db.pool, root, first)
        .await
        .unwrap();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT t.status, d.status FROM delegations d JOIN turns t ON t.id = d.child_turn_id
         WHERE d.id IN (?1, ?2) ORDER BY d.created_at",
    )
    .bind(first)
    .bind(second)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("interrupted".to_owned(), "cancelled".to_owned()),
            ("interrupted".to_owned(), "cancelled".to_owned()),
        ],
        "no orphan sub-task keeps running"
    );
    let _ = grandchild;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_a_running_task_stops_its_queued_sub_task_at_once() {
    let db = TestDb::migrated().await;
    let (_, root, child, first, grandchild, second, _) = seed_chain(&db).await;
    else_api::db::delegations::cancel(&db.pool, root, first)
        .await
        .unwrap();
    let statuses: Vec<String> =
        sqlx::query_scalar("SELECT status FROM turns WHERE id IN (?1, ?2) ORDER BY id = ?2")
            .bind(child)
            .bind(grandchild)
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        statuses,
        ["cancelling", "interrupted"],
        "the sub-task never starts"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(second)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "cancelled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_task_keeps_its_depth_when_its_delegating_task_is_updated() {
    use else_api::db::delegations::{MAX_DEPTH, chain, update};
    let db = TestDb::migrated().await;
    let (_, root, child, first, grandchild, _, _) = seed_chain(&db).await;
    update(&db.pool, root, first, "Also cover the refund path.")
        .await
        .unwrap();
    let replacement: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE id = ?1")
            .bind(first)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_ne!(replacement, child);
    let mut conn = db.pool.acquire().await.unwrap();
    let leaf = chain(&mut conn, grandchild).await.unwrap();
    assert_eq!(
        leaf.depth, MAX_DEPTH,
        "the superseded task keeps its ancestry"
    );
    assert_eq!(leaf.top, root);
    assert_eq!(chain(&mut conn, replacement).await.unwrap().depth, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn delegations_at_every_depth_share_one_request_budget() {
    use else_api::db::delegations::MAX_CHILDREN;
    let db = TestDb::migrated().await;
    let (_, root, child, _, _, _, _) = seed_chain(&db).await;
    for turn in [root, child] {
        assert_eq!(
            delegation_context(&db, turn).await.remaining,
            MAX_CHILDREN - 2,
            "the top's task and the task's own both count"
        );
    }
    // An owner follow-up carries the task forward (attempt 0): the new
    // message starts with its whole budget, and the task keeps what it spent.
    sqlx::query("UPDATE delegations SET parent_attempt = 0 WHERE child_turn_id = ?1")
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
    assert_eq!(delegation_context(&db, root).await.remaining, MAX_CHILDREN);
    assert_eq!(
        delegation_context(&db, child).await.remaining,
        MAX_CHILDREN - 1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hand_off_the_owner_chose_on_a_sub_tasks_card_goes_after_the_budget_runs_out() {
    use else_api::db::delegations::{DelegateError, MAX_CHILDREN, create};
    let db = TestDb::migrated().await;
    let (_, _, child, _, grandchild, _, third) = seed_chain(&db).await;
    let next_attempt = || async {
        sqlx::query("UPDATE turns SET attempt = attempt + 1 WHERE id = ?1")
            .bind(child)
            .execute(&db.pool)
            .await
            .unwrap();
    };
    for n in 0..4 {
        next_attempt().await;
        create(
            &db.pool,
            child,
            third,
            &format!("step {n}"),
            None,
            &[],
            None,
        )
        .await
        .unwrap();
    }
    next_attempt().await;
    assert_eq!(
        create(
            &db.pool,
            child,
            third,
            "post the changelog",
            None,
            &[],
            None
        )
        .await,
        Err(DelegateError::BudgetExhausted)
    );

    let card = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO owner_questions (id, bot_id, turn_id, attempt, kind, title, choices, multiple, status, answer)
         VALUES (?1, ?2, ?3, 1, 'choice', 'Where should the changelog go?',
                 '[{\"id\":\"a\",\"label\":\"Telegram\"},{\"id\":\"b\",\"label\":\"Discord\"}]',
                 1, 'answered', '[\"a\",\"b\"]')",
    )
    .bind(card)
    .bind(third)
    .bind(grandchild)
    .execute(&db.pool)
    .await
    .unwrap();
    common::seed::force_status(&db.pool, grandchild, TurnStatus::Running).await;
    // The task's turn ends; the delegation trigger settles its task.
    sqlx::query(
        "UPDATE turns SET status = 'completed',
             finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1",
    )
    .bind(grandchild)
    .execute(&db.pool)
    .await
    .unwrap();
    let named: Vec<(Uuid, Vec<String>)> = delegation_context(&db, child)
        .await
        .outcomes
        .iter()
        .flat_map(|o| o.choices.iter().map(|c| (c.id, c.chosen.clone())))
        .collect();
    assert_eq!(
        named,
        vec![(card, vec!["Telegram".to_owned(), "Discord".to_owned()])],
        "the sub-task's result carries the owner's card"
    );
    for n in 0..6 {
        let sent = create(
            &db.pool,
            child,
            third,
            &format!("post the changelog, part {n}"),
            None,
            &[],
            Some(card),
        )
        .await;
        assert!(
            sent.as_ref().is_ok_and(|s| s.created),
            "hand-off {n}: {sent:?}"
        );
        next_attempt().await;
    }
    assert_eq!(
        delegation_context(&db, child).await.remaining,
        0,
        "the owner's hand-offs leave the request budget alone"
    );
    assert_eq!(
        create(&db.pool, child, third, "one more", None, &[], Some(card)).await,
        Err(DelegateError::AnswerBudgetExhausted),
        "a card authorizes at most {MAX_CHILDREN} hand-offs"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sub_task_of_a_routine_run_keeps_the_routine_origin() {
    use else_api::app::providers::Origin;
    let db = TestDb::migrated().await;
    let (_, root, child, _, grandchild, _, _) =
        seed_chain_from(&db, ItemKind::Routine, "Hourly triage").await;
    let origins = |expected: Origin, why: &'static str| {
        let pool = db.pool.clone();
        async move {
            let mut conn = pool.acquire().await.unwrap();
            for turn in [root, child, grandchild] {
                let origin = else_api::db::delegations::origin(&mut conn, turn)
                    .await
                    .unwrap();
                assert_eq!(origin, expected, "{why}");
            }
        }
    };
    origins(
        Origin::WebhookWake,
        "a 'routine' item without a scheduled run is a webhook wake",
    )
    .await;
    let routine = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
             created_turn_id, created_name, request_hash)
         SELECT ?1, bot_id, conversation_id, 'Triage', 'Hourly triage', '0 * * * *', 'UTC',
             id, 'Triage', zeroblob(32) FROM turns WHERE id = ?2",
    )
    .bind(routine)
    .bind(root)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id)
         VALUES (randomblob(16), ?1, '2030-01-01T09:00:00Z', 'Triage', ?2)",
    )
    .bind(routine)
    .bind(root)
    .execute(&db.pool)
    .await
    .unwrap();
    origins(
        Origin::RoutineRun,
        "a task's own request item never makes it an owner request",
    )
    .await;
    assert!(
        else_api::db::turns::claim_next(&db.pool, &[], false)
            .await
            .unwrap()
            .is_none(),
        "the routine's sub-task never takes the slot kept for owner work"
    );
    let claimed = else_api::db::turns::claim_next(&db.pool, &[], true)
        .await
        .unwrap()
        .expect("the sub-task is claimed as background work");
    assert_eq!((claimed.id, claimed.background), (grandchild, true));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_settling_while_the_parent_parks_still_requeues_it() {
    let db = TestDb::migrated().await;
    let (_, root, child, delegation) = seed(&db).await;
    let mut parent = db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("SELECT 1 FROM turns WHERE id = ?1")
        .bind(root)
        .execute(&mut *parent)
        .await
        .unwrap();
    let pool = db.pool.clone();
    let mut child_end = tokio::spawn(async move {
        // The task's turn ends; the delegation trigger settles its task.
        sqlx::query("UPDATE turns SET status = 'completed', finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1")
            .bind(child)
            .execute(&pool)
            .await
            .unwrap();
    });
    common::never(
        "the child settles before the parent decides",
        Duration::from_millis(300),
        &mut child_end,
    )
    .await;
    common::seed::force_status(&mut *parent, root, TurnStatus::Waiting).await;
    parent.commit().await.unwrap();
    common::until("the child to settle after the parent's decision", child_end)
        .await
        .unwrap();
    let (root_status, delegation_status): (String, String) = sqlx::query_as(
        "SELECT t.status, d.status FROM turns t JOIN delegations d ON d.root_turn_id = t.id
         WHERE d.id = ?1",
    )
    .bind(delegation)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        (root_status.as_str(), delegation_status.as_str()),
        ("queued", "completed")
    );
}

/// A child turn ending requeues its waiting root by the shared
/// `turns_continuable` rule: once nothing else holds the root it continues,
/// even when its task's result was already delivered; a pending secret
/// request still holds it.
#[tokio::test(flavor = "multi_thread")]
async fn a_child_ending_continues_its_root_only_when_nothing_else_holds_it() {
    for held in [false, true] {
        let db = TestDb::migrated().await;
        let (_, root, child, delegation) = seed(&db).await;
        if held {
            sqlx::query(
                "INSERT INTO secret_requests (id, bot_id, turn_id, attempt, name, reason, env_names)
                 SELECT ?1, bot_id, id, attempt, 'login', 'Authenticate', '[\"LOGIN_TOKEN\"]'
                 FROM turns WHERE id = ?2",
            )
            .bind(Uuid::new_v4())
            .bind(root)
            .execute(&db.pool)
            .await
            .unwrap();
        }
        common::seed::force_status(&db.pool, root, TurnStatus::Waiting).await;
        sqlx::query(
            "UPDATE delegations SET status = 'completed', delivered_attempt = 1,
                 finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
             WHERE id = ?1",
        )
        .bind(delegation)
        .execute(&db.pool)
        .await
        .unwrap();
        // The task's turn ends after its result was delivered; the trigger requeues an unheld root.
        sqlx::query(
            "UPDATE turns SET status = 'completed',
                 finished_at = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now') WHERE id = ?1",
        )
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
        let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
            .bind(root)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        let expected = if held { "waiting" } else { "queued" };
        assert_eq!(status, expected, "held by a secret request: {held}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_reconciliation_ends_the_children_of_an_ended_root() {
    let db = TestDb::migrated().await;
    let (_, root, child, delegation) = seed(&db).await;
    else_api::db::turns::lifecycle::recover_after_restart(&db.pool)
        .await
        .unwrap();
    let (root_status, child_status, delegation_status): (String, String, String) = sqlx::query_as(
        "SELECT r.status, c.status, d.status FROM delegations d
         JOIN turns r ON r.id = d.root_turn_id JOIN turns c ON c.id = d.child_turn_id
         WHERE d.id = ?1",
    )
    .bind(delegation)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    let _ = (root, child);
    assert_eq!(root_status, "uncertain");
    assert_eq!(child_status, "interrupted", "no orphan child runs");
    assert_eq!(delegation_status, "cancelled");
}
