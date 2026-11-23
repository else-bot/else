use crate::common;
use else_api::app::providers::FailureCode;
use else_api::db::turns::TurnStatus;

use std::sync::Arc;
use std::time::Duration;

use common::{TestApp, TestDb};
use else_api::app::providers::{
    Availability, Profile, Provider, SessionState, Stop, TurnInput, TurnOutcome, TurnSink,
};
use else_api::db::steers::Placement;
use else_api::providers::synthetic::Synthetic;
use uuid::Uuid;

/// The synthetic provider, except that a message naming a `[steer:...]`
/// script stays running until an owner message joins it: `take` takes it
/// and completes, `fail` takes it and fails, `ignore` completes without
/// taking it (a provider that takes no input mid-turn), `drop` fails
/// without taking it, `hold` takes it and keeps running, and `park` takes
/// whatever the server still offers once an owner message joins, then ends
/// as an attempt that parked on a wait does. A continuation that carries
/// joined messages says which.
struct Steering {
    inner: Synthetic,
}

#[async_trait::async_trait]
impl Provider for Steering {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn label(&self) -> &'static str {
        self.inner.label()
    }
    fn availability(&self) -> Availability {
        self.inner.availability()
    }
    fn uses_profile(&self) -> bool {
        self.inner.uses_profile()
    }
    fn validate_profile(&self, profile: &Profile) -> Result<(), String> {
        self.inner.validate_profile(profile)
    }
    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome {
        let joined: Vec<String> = input
            .steered
            .iter()
            .filter(|steered| !steered.seen)
            .map(|steered| steered.message.text.clone())
            .collect();
        let script = ["take", "fail", "ignore", "drop", "hold", "park"]
            .into_iter()
            .find(|script| input.text.contains(&format!("[steer:{script}]")))
            .filter(|script| joined.is_empty() && (*script != "park" || input.steered.is_empty()));
        if script.is_none() && joined.is_empty() {
            return self.inner.run(input, sink, stop).await;
        }
        let session = SessionState {
            native_id: Uuid::new_v4().to_string(),
            fingerprint: "steering".into(),
            considered_through_turn: input.turn_no,
            memory_revision: None,
            default_skills: Default::default(),
            context: Default::default(),
        };
        if sink.session(&session).await.is_err()
            || sink.sending().await.is_err()
            || sink.running().await.is_err()
        {
            return TurnOutcome::Interrupted;
        }
        let Some(script) = script else {
            let _ = sink
                .text(&format!("Continued with: {}", joined.join(" | ")))
                .await;
            return TurnOutcome::Completed;
        };
        let _ = sink.text("Working on the first request.").await;
        loop {
            tokio::select! {
                () = stop.requested() => return TurnOutcome::Interrupted,
                () = stop.steered() => {}
            }
            for steer in sink.steers().await {
                match script {
                    "ignore" => return TurnOutcome::Completed,
                    "drop" => return failed(),
                    _ => {}
                }
                assert!(matches!(sink.steer_sending(steer.turn_id).await, Ok(true)));
                let _ = sink.steered(steer.turn_id).await;
                let _ = sink.text(&format!("Also handled: {}", steer.text)).await;
                match script {
                    "take" => return TurnOutcome::Completed,
                    "fail" => return failed(),
                    _ => {}
                }
            }
            if script == "park" {
                return TurnOutcome::Completed;
            }
        }
    }
}

fn failed() -> TurnOutcome {
    TurnOutcome::Failed {
        code: FailureCode::ProviderFailed,
        message: "The scripted provider failed.".into(),
    }
}

async fn app() -> TestApp {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    sqlx::query("UPDATE installation SET default_provider_id = 'synthetic'")
        .execute(&db.pool)
        .await
        .unwrap();
    let provider = Arc::new(Steering {
        inner: Synthetic::new(Duration::from_millis(5)),
    });
    TestApp::builder()
        .db(Arc::new(db))
        .max_active(2)
        .providers(vec![provider])
        .start()
        .await
}

async fn send(app: &TestApp, bot: &str, text: &str) -> String {
    let reply = app.send(bot, Uuid::new_v4(), text, None).await;
    assert!(reply.status.is_success(), "{:?}", reply.body);
    reply.body["turn"]["id"].as_str().unwrap().to_owned()
}

async fn status(pool: &sqlx::SqlitePool, turn: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, error_code FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// Steers whose message still waits on its reply.
async fn rows(pool: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM turn_steers s JOIN turns t ON t.id = s.turn_id
         WHERE t.status = 'queued'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Item kinds and turn order of the conversation, as the chat shows them.
async fn transcript(app: &TestApp, bot: &str) -> Vec<(String, String)> {
    sqlx::query_as(
        "SELECT i.kind, i.text FROM items i JOIN conversations c ON c.id = i.conversation_id
         WHERE c.bot_id = ?1 AND c.kind = 'owner' AND i.kind IN ('user', 'assistant')
         ORDER BY i.seq",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

/// Until the running reply has written its first text, so the chat order
/// is fixed before the next message.
async fn wait_for_reply_text(app: &TestApp, bot: &str) {
    common::until(
        "the first reply text",
        common::poll(|| async { (transcript(app, bot).await.len() == 2).then_some(()) }),
    )
    .await;
}

const ENDED: &[&str] = &["completed", "failed", "interrupted", "uncertain"];

#[tokio::test]
async fn a_newer_owner_message_joins_the_running_reply() {
    let app = app().await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:take]").await;
    app.wait_turn(&first, &["running"]).await;
    wait_for_reply_text(&app, &bot).await;

    let second = send(&app, &bot, "If they agreed, send the deal.").await;

    assert_eq!(app.wait_turn(&first, ENDED).await, "completed");
    assert_eq!(app.wait_turn(&second, ENDED).await, "completed");
    assert_eq!(
        status(&app.db.pool, first.parse().unwrap()).await,
        ("completed".into(), None)
    );
    assert_eq!(rows(&app.db.pool).await, 0);
    let link: (Uuid, String) =
        sqlx::query_as("SELECT into_turn_id, status FROM turn_steers WHERE turn_id = ?1")
            .bind(second.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(
        link,
        (first.parse().unwrap(), "delivered".into()),
        "the message stays part of the reply that answered it"
    );
    assert_eq!(
        transcript(&app, &bot).await,
        [
            (
                "user".into(),
                "Patch the sender script. [steer:take]".into()
            ),
            ("assistant".into(), "Working on the first request.".into()),
            ("user".into(), "If they agreed, send the deal.".into()),
            (
                "assistant".into(),
                "Also handled: If they agreed, send the deal.".into()
            ),
        ]
    );
}

/// Grok takes no input mid-turn: its reply finishes its work, then
/// continues in the same reply with the message, never stopped by it.
#[tokio::test]
async fn a_reply_that_takes_no_input_mid_turn_continues_with_the_message() {
    let app = app().await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:ignore]").await;
    app.wait_turn(&first, &["running"]).await;
    wait_for_reply_text(&app, &bot).await;

    let second = send(&app, &bot, "If they agreed, send the deal.").await;

    assert_eq!(app.wait_turn(&first, ENDED).await, "completed");
    assert_eq!(app.wait_turn(&second, ENDED).await, "completed");
    let attempt: i64 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(first.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(attempt, 2, "the reply continued once");
    assert_eq!(
        transcript(&app, &bot).await,
        [
            (
                "user".into(),
                "Patch the sender script. [steer:ignore]".into()
            ),
            ("assistant".into(), "Working on the first request.".into()),
            ("user".into(), "If they agreed, send the deal.".into()),
            (
                "assistant".into(),
                "Continued with: If they agreed, send the deal.".into()
            ),
        ]
    );
    assert_eq!(rows(&app.db.pool).await, 0);
}

#[tokio::test]
async fn a_message_a_failed_reply_never_took_runs_on_its_own() {
    let app = app().await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:drop]").await;
    app.wait_turn(&first, &["running"]).await;

    let second = send(&app, &bot, "If they agreed, send the deal.").await;

    assert_eq!(app.wait_turn(&first, ENDED).await, "failed");
    assert_eq!(app.wait_turn(&second, ENDED).await, "completed");
    let answered: i64 =
        sqlx::query_scalar("SELECT count(*) FROM items WHERE turn_id = ?1 AND kind = 'assistant'")
            .bind(second.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(answered, 1, "the released message got its own reply");
    assert_eq!(rows(&app.db.pool).await, 0);
}

/// The owner picked another provider for this message: it queues behind
/// the reply instead of joining or stopping it.
#[tokio::test]
async fn a_message_for_another_provider_queues_without_stopping_the_reply() {
    let (db, bot, first) = setup_running().await;
    let pool = &db.pool;
    let (later, into) = accept_on(pool, bot, "Ask the other model.", "other").await;
    assert_eq!(into, Some(Placement::Queued));
    assert_eq!(status(pool, first).await.0, "running");
    assert_eq!(status(pool, later).await.0, "queued");
    assert_eq!(rows(pool).await, 0);
}

#[tokio::test]
async fn a_steered_message_ends_with_the_reply_that_took_it() {
    let app = app().await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:fail]").await;
    app.wait_turn(&first, &["running"]).await;

    let second = send(&app, &bot, "If they agreed, send the deal.").await;

    assert_eq!(app.wait_turn(&first, ENDED).await, "failed");
    assert_eq!(app.wait_turn(&second, ENDED).await, "failed");
    assert_eq!(
        status(&app.db.pool, second.parse().unwrap()).await,
        ("failed".into(), Some("provider_failed".into()))
    );
}

#[tokio::test]
async fn stop_ends_the_reply_and_every_message_steered_into_it() {
    let app = app().await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:hold]").await;
    app.wait_turn(&first, &["running"]).await;
    let second = send(&app, &bot, "If they agreed, send the deal.").await;
    common::until(
        "the steer to be delivered",
        common::poll(|| async {
            let delivered: i64 =
                sqlx::query_scalar("SELECT count(*) FROM turn_steers WHERE status = 'delivered'")
                    .fetch_one(&app.db.pool)
                    .await
                    .unwrap();
            (delivered == 1).then_some(())
        }),
    )
    .await;
    assert_eq!(
        status(&app.db.pool, second.parse().unwrap()).await.0,
        "queued",
        "held while steered"
    );

    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(serde_json::json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{:?}", stop.body);

    assert_eq!(app.wait_turn(&first, ENDED).await, "interrupted");
    assert_eq!(app.wait_turn(&second, ENDED).await, "interrupted");
    assert_eq!(rows(&app.db.pool).await, 0);
}

/// Accept `text` on the synthetic provider; the reply it joined, if any.
async fn accept(pool: &sqlx::SqlitePool, bot: Uuid, text: &str) -> (Uuid, Option<Uuid>) {
    let (turn, placement) = accept_on(pool, bot, text, "synthetic").await;
    let into = match placement {
        Some(Placement::Joined { into, .. }) => Some(into),
        _ => None,
    };
    (turn, into)
}

async fn accept_on(
    pool: &sqlx::SqlitePool,
    bot: Uuid,
    text: &str,
    provider_id: &str,
) -> (Uuid, Option<Placement>) {
    let accepted = else_api::db::turns::accept(
        pool,
        else_api::db::turns::SendRequest {
            bot_id: bot,
            request_id: Uuid::new_v4(),
            text,
            hidden: false,
            parts: &[],
            reaction: None,
            requested_provider: None,
            provider_id,
            uses_profile: false,
            open_draft: None,
            answers_notice: None,
        },
        else_api::db::turns::Admission::Open,
    )
    .await
    .unwrap();
    (accepted.turn.id, accepted.placement)
}

async fn setup_running() -> (TestDb, Uuid, Uuid) {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let bot: Uuid = sqlx::query_scalar("SELECT id FROM bots LIMIT 1")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let (first, _) = accept(&db.pool, bot, "Patch the sender script.").await;
    // Start the accepted turn's first attempt without a scheduler.
    sqlx::query("UPDATE turns SET status = 'running', attempt = 1 WHERE id = ?1")
        .bind(first)
        .execute(&db.pool)
        .await
        .unwrap();
    (db, bot, first)
}

#[tokio::test]
async fn a_restart_settles_each_steer_by_how_far_it_got() {
    let (db, bot, first) = setup_running().await;
    let pool = &db.pool;
    let mut steered = Vec::new();
    for text in [
        "If they agreed, send the deal.",
        "Schedule it too.",
        "What is this?",
    ] {
        let (turn, into) = accept(pool, bot, text).await;
        assert_eq!(into, Some(first), "each message joins the same reply");
        steered.push(turn);
    }
    let [delivered, written, untouched] = steered[..] else {
        unreachable!()
    };
    for turn in [delivered, written] {
        assert!(
            else_api::db::steers::sending(pool, turn, first)
                .await
                .unwrap()
        );
    }
    assert!(
        else_api::db::steers::deliver(pool, delivered, first)
            .await
            .unwrap()
    );
    assert!(
        !else_api::db::steers::deliver(pool, untouched, first)
            .await
            .unwrap(),
        "a message never written cannot be delivered"
    );
    let claimed = else_api::db::turns::claim_next(pool, &[first], true)
        .await
        .unwrap();
    assert!(claimed.is_none(), "steered messages are not claimed");

    else_api::db::turns::lifecycle::recover_after_restart(pool)
        .await
        .unwrap();

    assert_eq!(status(pool, first).await.0, "uncertain");
    assert_eq!(
        status(pool, delivered).await,
        ("uncertain".into(), Some("api_restarted".into()))
    );
    assert_eq!(
        status(pool, written).await,
        ("uncertain".into(), Some("steer_uncertain".into())),
        "a message that may have reached the reply is never resent"
    );
    assert_eq!(status(pool, untouched).await.0, "queued");
    assert_eq!(rows(pool).await, 0);
    let claimed = else_api::db::turns::claim_next(pool, &[], true)
        .await
        .unwrap();
    assert_eq!(claimed.map(|c| c.id), Some(untouched), "it runs on its own");
}

#[tokio::test]
async fn a_parked_reply_keeps_a_delivered_message_for_its_continuation() {
    let (db, bot, first) = setup_running().await;
    let pool = &db.pool;
    let (later, _) = accept(pool, bot, "After the job finishes, upload the report.").await;
    assert!(
        else_api::db::steers::sending(pool, later, first)
            .await
            .unwrap()
    );
    assert!(
        else_api::db::steers::deliver(pool, later, first)
            .await
            .unwrap()
    );

    common::seed::force_status(pool, first, TurnStatus::Queued).await;
    let mut conn = pool.acquire().await.unwrap();
    else_api::db::steers::settle(&mut conn).await.unwrap();
    drop(conn);

    assert_eq!(status(pool, later).await.0, "queued");
    assert_eq!(rows(pool).await, 1);
    let input = else_api::db::turns::input(pool, first).await.unwrap();
    let steered: Vec<_> = input
        .steered
        .iter()
        .map(|s| (s.message.text.as_str(), s.seen))
        .collect();
    assert_eq!(
        steered,
        [("After the job finishes, upload the report.", true)]
    );
    let request = common::fresh_packet(&input);
    assert!(
        request.ends_with(
            "\n\nPatch the sender script.\n\n[else.bot] The owner sent this while you were \
             working. Take it into the work in progress: keep doing everything still asked \
             unless this message changes or cancels it, and stop only if it asks you to.\n\n\
             After the job finishes, upload the report."
        ),
        "the continuation asks for the added message too: {request}"
    );
    let claimed = else_api::db::turns::claim_next(pool, &[], true)
        .await
        .unwrap();
    assert_eq!(
        claimed.map(|c| c.id),
        Some(first),
        "only the reply continues"
    );

    common::seed::force_status(pool, first, TurnStatus::Completed).await;
    let mut conn = pool.acquire().await.unwrap();
    else_api::db::steers::settle(&mut conn).await.unwrap();
    drop(conn);
    assert_eq!(status(pool, later).await, ("completed".into(), None));
    assert_eq!(rows(pool).await, 0);
}

/// A message to a reply that waits on the owner continues it at once. The
/// question it types over is skipped; every other hold stays pending. The
/// next request carries the message, which counts as delivered once that
/// request is sent, and only once.
#[tokio::test]
async fn a_message_continues_a_waiting_reply_and_keeps_its_holds() {
    let (db, bot, first) = setup_running().await;
    let pool = &db.pool;
    common::seed::force_status(pool, first, TurnStatus::Waiting).await;
    let question = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO owner_questions (id, bot_id, turn_id, attempt, kind, title, choices)
         VALUES (?1, ?2, ?3, 1, 'choice', 'Which file?', '[]')",
    )
    .bind(question)
    .bind(bot)
    .bind(first)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO secret_requests (id, bot_id, turn_id, attempt, name, reason, env_names)
         VALUES (?1, ?2, ?3, 1, 'deploy_key', 'To deploy.', '[\"DEPLOY_KEY\"]')",
    )
    .bind(Uuid::new_v4())
    .bind(bot)
    .bind(first)
    .execute(pool)
    .await
    .unwrap();

    let (later, into) = accept(pool, bot, "Use the staging key.").await;

    assert_eq!(into, Some(first));
    assert_eq!(
        status(pool, first).await.0,
        "queued",
        "it continues at once"
    );
    let holds: (String, String) = sqlx::query_as(
        "SELECT q.status, s.status FROM owner_questions q, secret_requests s
         WHERE q.turn_id = ?1 AND s.turn_id = ?1",
    )
    .bind(first)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(holds, ("skipped".into(), "pending".into()));

    let claimed = else_api::db::turns::claim_next(pool, &[], true)
        .await
        .unwrap();
    assert_eq!(claimed.map(|c| c.id), Some(first), "only the reply runs");
    let input = else_api::db::turns::input(pool, first).await.unwrap();
    let steered: Vec<_> = input
        .steered
        .iter()
        .map(|s| (s.message.text.as_str(), s.seen))
        .collect();
    assert_eq!(steered, [("Use the staging key.", false)]);
    let steer_status = || async {
        sqlx::query_scalar::<_, String>("SELECT status FROM turn_steers WHERE turn_id = ?1")
            .bind(later)
            .fetch_one(pool)
            .await
            .unwrap()
    };
    assert_eq!(steer_status().await, "pending", "not sent yet");

    let sink = else_api::db::turn_sink::SqliteSink::new(pool.clone(), &input, "synthetic", None);
    else_api::app::providers::TurnSink::sending(&sink)
        .await
        .unwrap();

    assert_eq!(steer_status().await, "delivered");
    let input = else_api::db::turns::input(pool, first).await.unwrap();
    assert!(input.steered[0].seen, "a later attempt has seen it");
    common::seed::force_status(pool, first, TurnStatus::Running).await;
    assert!(
        !else_api::db::steers::refuse(pool, later, first)
            .await
            .unwrap(),
        "a delivered message cannot be refused back to pending"
    );
    assert!(
        else_api::db::steers::pending(pool, first)
            .await
            .unwrap()
            .is_empty(),
        "nor written into the reply again"
    );
}

/// A card stamped in the same millisecond as the owner's message (or
/// later, after the clock steps back) was still open when the message was
/// accepted, so the message skips it.
#[tokio::test]
async fn a_message_skips_a_card_stamped_no_earlier_than_itself() {
    let (db, bot, first) = setup_running().await;
    let pool = &db.pool;
    common::seed::force_status(pool, first, TurnStatus::Waiting).await;
    let question = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO owner_questions (id, bot_id, turn_id, attempt, kind, title, choices, created_at)
         VALUES (?1, ?2, ?3, 1, 'choice', 'Which file?', '[]',
                 strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+1 minute'))",
    )
    .bind(question)
    .bind(bot)
    .bind(first)
    .execute(pool)
    .await
    .unwrap();

    let (_, placement) = accept_on(pool, bot, "Use the staging key.", "codex").await;

    assert_eq!(placement, Some(Placement::Queued));
    let asked: String = sqlx::query_scalar("SELECT status FROM owner_questions WHERE id = ?1")
        .bind(question)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(asked, "skipped");
    assert_eq!(status(pool, first).await.0, "queued", "the reply continues");
}

/// The provider answered that it took nothing (Codex after its turn
/// ended): the message is pending again and the parked reply continues
/// with it.
#[tokio::test]
async fn a_refused_message_waits_for_the_reply_to_continue() {
    let (db, bot, first) = setup_running().await;
    let pool = &db.pool;
    let (later, _) = accept(pool, bot, "Schedule it too.").await;
    assert!(
        else_api::db::steers::sending(pool, later, first)
            .await
            .unwrap()
    );
    assert!(
        else_api::db::steers::refuse(pool, later, first)
            .await
            .unwrap()
    );

    common::seed::force_status(pool, first, TurnStatus::Queued).await;
    let mut conn = pool.acquire().await.unwrap();
    else_api::db::steers::settle(&mut conn).await.unwrap();
    drop(conn);

    assert_eq!(status(pool, later).await.0, "queued");
    let input = else_api::db::turns::input(pool, first).await.unwrap();
    assert!(!input.steered[0].seen, "the next request carries it");
}

#[tokio::test]
async fn a_message_the_reply_took_is_history_of_that_reply() {
    let (db, bot, first) = setup_running().await;
    let pool = &db.pool;
    let (taken, _) = accept(pool, bot, "Schedule it too.").await;
    assert!(
        else_api::db::steers::sending(pool, taken, first)
            .await
            .unwrap()
    );
    assert!(
        else_api::db::steers::deliver(pool, taken, first)
            .await
            .unwrap()
    );
    common::seed::force_status(pool, first, TurnStatus::Completed).await;
    else_api::db::steers::settle(&mut pool.acquire().await.unwrap())
        .await
        .unwrap();
    assert_eq!(status(pool, taken).await.0, "completed");

    let (next, into) = accept(pool, bot, "What did they say?").await;
    assert_eq!(into, None);
    let first_no: i64 = sqlx::query_scalar("SELECT turn_no FROM turns WHERE id = ?1")
        .bind(first)
        .fetch_one(pool)
        .await
        .unwrap();
    let input = else_api::db::turns::input(pool, next).await.unwrap();
    let entry = input
        .history
        .iter()
        .find(|entry| entry.text == "Schedule it too.")
        .expect("the taken message is history");
    assert_eq!(
        entry.turn_no, first_no,
        "a session that considered the reply has seen it"
    );
}

/// A message that joins a reply after its attempt parked on a long job is
/// not written into that attempt, which refuses every tool call. The
/// attempt ends, the reply continues at once with the message while the
/// job's wait stays held, and the job's end still continues it once.
#[tokio::test]
async fn a_message_to_a_parked_attempt_runs_now_and_the_wait_still_reports() {
    let app = app().await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Run the release build. [steer:park]").await;
    app.wait_turn(&first, &["running"]).await;
    wait_for_reply_text(&app, &bot).await;
    let pool = &app.db.pool;
    let into: Uuid = first.parse().unwrap();
    let bot_id: Uuid = bot.parse().unwrap();
    let job = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib)
         VALUES (?1, 'elsebot-x', 'img', 1, 2048)",
    )
    .bind(bot_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO guest_jobs (id, bot_id, command, status, deadline)
         VALUES (?1, ?2, 'make release', 'running',
                 strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+1 hours'))",
    )
    .bind(job)
    .bind(bot_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO computer_job_waits (id, bot_id, turn_id, attempt, job_id)
         VALUES (?1, ?2, ?3, 1, ?4)",
    )
    .bind(Uuid::new_v4())
    .bind(bot_id)
    .bind(into)
    .bind(job)
    .execute(pool)
    .await
    .unwrap();

    let second = send(&app, &bot, "Also file the flaky test issue.").await;

    assert_eq!(app.wait_turn(&first, &["waiting"]).await, "waiting");
    let attempt: i32 = sqlx::query_scalar("SELECT attempt FROM turns WHERE id = ?1")
        .bind(into)
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(attempt, 2, "a fresh attempt took the message");
    assert_eq!(
        transcript(&app, &bot).await.last().unwrap(),
        &(
            "assistant".into(),
            "Continued with: Also file the flaky test issue.".into()
        )
    );
    let wait: (String, Option<i32>) = sqlx::query_as(
        "SELECT status, delivered_attempt FROM computer_job_waits WHERE turn_id = ?1",
    )
    .bind(into)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(wait, ("pending".into(), None), "the wait stays held");

    sqlx::query("UPDATE guest_jobs SET status = 'completed' WHERE id = ?1")
        .bind(job)
        .execute(pool)
        .await
        .unwrap();

    assert_eq!(app.wait_turn(&first, ENDED).await, "completed");
    assert_eq!(app.wait_turn(&second, ENDED).await, "completed");
    let ended: (i32, String, Option<i32>) = sqlx::query_as(
        "SELECT t.attempt, w.status, w.delivered_attempt
         FROM turns t JOIN computer_job_waits w ON w.turn_id = t.id WHERE t.id = ?1",
    )
    .bind(into)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        ended,
        (3, "settled".into(), Some(3)),
        "the job's end continued the reply once"
    );
}

/// A parked attempt refuses every tool call, so it never takes an owner
/// message: none is written into it, and one the provider echoes after the
/// park goes back to pending for the next attempt. A message it took
/// before parking, while its tools still worked, stays taken.
#[tokio::test]
async fn a_parked_attempt_takes_no_owner_message() {
    let (db, bot, first) = setup_running().await;
    let pool = &db.pool;
    let sending = else_api::db::steers::sending;
    let (before, _) = accept(pool, bot, "Use the staging key.").await;
    let (in_flight, _) = accept(pool, bot, "Also file the flaky test issue.").await;
    let (after, _) = accept(pool, bot, "And tag the release.").await;
    for turn in [before, in_flight] {
        assert!(sending(pool, turn, first).await.unwrap());
    }
    assert!(
        else_api::db::steers::deliver(pool, before, first)
            .await
            .unwrap()
    );
    sqlx::query(
        "INSERT INTO secret_requests (id, bot_id, turn_id, attempt, name, reason, env_names)
         VALUES (?1, ?2, ?3, 1, 'deploy_key', 'To deploy.', '[\"DEPLOY_KEY\"]')",
    )
    .bind(Uuid::new_v4())
    .bind(bot)
    .bind(first)
    .execute(pool)
    .await
    .unwrap();

    assert!(
        !sending(pool, after, first).await.unwrap(),
        "nothing is written into a parked attempt"
    );
    assert!(
        else_api::db::steers::deliver(pool, in_flight, first)
            .await
            .unwrap()
    );

    let statuses: Vec<(String, String)> = sqlx::query_as(
        "SELECT i.text, s.status FROM turn_steers s JOIN turns t ON t.id = s.turn_id
         JOIN items i ON i.id = t.owner_item_id ORDER BY t.turn_no",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert_eq!(
        statuses,
        [
            ("Use the staging key.".into(), "delivered".into()),
            ("Also file the flaky test issue.".into(), "pending".into()),
            ("And tag the release.".into(), "pending".into()),
        ]
    );
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        else_api::db::steers::waiting_for(&mut conn, first)
            .await
            .unwrap(),
        2,
        "the attempt's end continues the reply with both"
    );
}
