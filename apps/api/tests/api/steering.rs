use crate::common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestApp, TestDb};
use else_api::app::providers::{
    Availability, Profile, Provider, SessionState, Stop, TurnInput, TurnOutcome, TurnSink,
};
use else_api::providers::synthetic::Synthetic;
use uuid::Uuid;

/// The synthetic provider, except that a message naming a `[steer:...]`
/// script stays running until an owner message is steered into it:
/// `take` takes it and completes, `fail` takes it and fails, `ignore`
/// completes without taking it, and `hold` takes it and keeps running.
struct Steering {
    inner: Synthetic,
    steers: bool,
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
    fn steers(&self) -> bool {
        self.steers
    }
    async fn run(&self, input: TurnInput, sink: &dyn TurnSink, stop: Stop) -> TurnOutcome {
        let script = ["take", "fail", "ignore", "hold"]
            .into_iter()
            .find(|script| input.text.contains(&format!("[steer:{script}]")));
        let Some(script) = script else {
            return self.inner.run(input, sink, stop).await;
        };
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
        let _ = sink.text("Working on the first request.").await;
        loop {
            tokio::select! {
                () = stop.requested() => return TurnOutcome::Interrupted,
                () = stop.steered() => {}
            }
            for steer in sink.steers().await {
                if script == "ignore" {
                    return TurnOutcome::Completed;
                }
                assert!(matches!(sink.steer_sending(steer.turn_id).await, Ok(true)));
                let _ = sink.steered(steer.turn_id).await;
                let _ = sink.text(&format!("Also handled: {}", steer.text)).await;
                match script {
                    "take" => return TurnOutcome::Completed,
                    "fail" => {
                        return TurnOutcome::Failed {
                            code: "provider_failed".into(),
                            message: "The scripted provider failed.".into(),
                        };
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn app(steers: bool) -> TestApp {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    sqlx::query("UPDATE installation SET default_provider_id = 'synthetic'")
        .execute(&db.pool)
        .await
        .unwrap();
    let provider = Arc::new(Steering {
        inner: Synthetic::new(Duration::from_millis(5)),
        steers,
    });
    TestApp::with_registry(
        Arc::new(db),
        2,
        else_api::providers::Registry::new(vec![provider]),
    )
}

async fn send(app: &TestApp, bot: &str, text: &str) -> String {
    let reply = app.send(bot, Uuid::new_v4(), text, None).await;
    assert!(reply.status.is_success(), "{:?}", reply.body);
    reply.body["turn"]["id"].as_str().unwrap().to_owned()
}

async fn turn(app: &TestApp, id: &str) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, error_code FROM turns WHERE id = ?1")
        .bind(id.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn steer_rows(app: &TestApp) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM turn_steers")
        .fetch_one(&app.db.pool)
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

const ENDED: &[&str] = &["completed", "failed", "interrupted", "uncertain"];

#[tokio::test]
async fn a_newer_owner_message_joins_the_running_reply() {
    let app = app(true).await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:take]").await;
    app.wait_turn(&first, &["running"]).await;
    for _ in 0..400 {
        if transcript(&app, &bot).await.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let second = send(&app, &bot, "If they agreed, send the deal.").await;

    assert_eq!(app.wait_turn(&first, ENDED).await, "completed");
    assert_eq!(app.wait_turn(&second, ENDED).await, "completed");
    assert_eq!(turn(&app, &first).await, ("completed".into(), None));
    assert_eq!(steer_rows(&app).await, 0);
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

#[tokio::test]
async fn a_provider_that_cannot_steer_is_still_stopped_by_a_newer_message() {
    let app = app(false).await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:take]").await;
    app.wait_turn(&first, &["running"]).await;

    let second = send(&app, &bot, "If they agreed, send the deal.").await;

    assert_eq!(app.wait_turn(&first, ENDED).await, "interrupted");
    assert_eq!(app.wait_turn(&second, ENDED).await, "completed");
    assert_eq!(steer_rows(&app).await, 0);
}

#[tokio::test]
async fn a_steered_message_the_reply_never_took_runs_on_its_own() {
    let app = app(true).await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:ignore]").await;
    app.wait_turn(&first, &["running"]).await;

    let second = send(&app, &bot, "If they agreed, send the deal.").await;

    assert_eq!(app.wait_turn(&first, ENDED).await, "completed");
    assert_eq!(app.wait_turn(&second, ENDED).await, "completed");
    let answered: i64 =
        sqlx::query_scalar("SELECT count(*) FROM items WHERE turn_id = ?1 AND kind = 'assistant'")
            .bind(second.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(answered, 1, "the released message got its own reply");
    assert_eq!(steer_rows(&app).await, 0);
}

#[tokio::test]
async fn a_steered_message_ends_with_the_reply_that_took_it() {
    let app = app(true).await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:fail]").await;
    app.wait_turn(&first, &["running"]).await;

    let second = send(&app, &bot, "If they agreed, send the deal.").await;

    assert_eq!(app.wait_turn(&first, ENDED).await, "failed");
    assert_eq!(app.wait_turn(&second, ENDED).await, "failed");
    assert_eq!(
        turn(&app, &second).await,
        ("failed".into(), Some("provider_failed".into()))
    );
}

#[tokio::test]
async fn stop_ends_the_reply_and_every_message_steered_into_it() {
    let app = app(true).await;
    let bot = app.main_bot().await;
    let first = send(&app, &bot, "Patch the sender script. [steer:hold]").await;
    app.wait_turn(&first, &["running"]).await;
    let second = send(&app, &bot, "If they agreed, send the deal.").await;
    for _ in 0..200 {
        let delivered: i64 =
            sqlx::query_scalar("SELECT count(*) FROM turn_steers WHERE status = 'delivered'")
                .fetch_one(&app.db.pool)
                .await
                .unwrap();
        if delivered == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(turn(&app, &second).await.0, "queued", "held while steered");

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
    assert_eq!(steer_rows(&app).await, 0);
}

async fn accept(
    pool: &sqlx::SqlitePool,
    bot: Uuid,
    text: &str,
    steers: bool,
) -> (Uuid, Option<Uuid>) {
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
            provider_id: "synthetic",
            uses_profile: false,
            steers,
            open_draft: None,
            answers_notice: None,
        },
        else_api::db::turns::Preemption::Cancel,
    )
    .await
    .unwrap();
    assert!(accepted.preempted.is_empty());
    (accepted.turn.id, accepted.steered_into)
}

async fn status(pool: &sqlx::SqlitePool, turn: Uuid) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, error_code FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn setup_running() -> (TestDb, Uuid, Uuid) {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let bot: Uuid = sqlx::query_scalar("SELECT id FROM bots LIMIT 1")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let (first, _) = accept(&db.pool, bot, "Patch the sender script.", false).await;
    sqlx::query("UPDATE turns SET status = 'running', attempt = 1 WHERE id = ?1")
        .bind(first)
        .execute(&db.pool)
        .await
        .unwrap();
    (db, bot, first)
}

async fn rows(pool: &sqlx::SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM turn_steers")
        .fetch_one(pool)
        .await
        .unwrap()
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
        let (turn, into) = accept(pool, bot, text, true).await;
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

    else_api::db::turns::recover_after_restart(pool)
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
    let (later, _) = accept(
        pool,
        bot,
        "After the job finishes, upload the report.",
        true,
    )
    .await;
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

    sqlx::query("UPDATE turns SET status = 'queued' WHERE id = ?1")
        .bind(first)
        .execute(pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    else_api::db::steers::settle(&mut conn).await.unwrap();
    drop(conn);

    assert_eq!(status(pool, later).await.0, "queued");
    assert_eq!(rows(pool).await, 1);
    let input = else_api::db::turns::input(pool, first).await.unwrap();
    assert_eq!(
        input.steered,
        ["After the job finishes, upload the report."]
    );
    let request = else_api::providers::prompt::request_text(&input);
    assert!(
        request.starts_with("Patch the sender script.")
            && request.ends_with("After the job finishes, upload the report."),
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

    sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?1")
        .bind(first)
        .execute(pool)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    else_api::db::steers::settle(&mut conn).await.unwrap();
    drop(conn);
    assert_eq!(status(pool, later).await, ("completed".into(), None));
    assert_eq!(rows(pool).await, 0);
}
