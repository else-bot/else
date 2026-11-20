use crate::common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TestApp, TestDb};
use else_api::app::providers::{
    Availability, Profile, Provider, SessionState, Stop, TurnInput, TurnOutcome, TurnSink,
};
use else_api::providers::synthetic::Synthetic;
use serde_json::json;
use uuid::Uuid;

/// What one attempt received.
#[derive(Debug, Clone)]
struct Seen {
    attempt: i32,
    outcomes: String,
}

/// The synthetic provider, except that a message naming `[limit:N]` has
/// its first N attempts cut at the attempt limit after writing progress,
/// `[limit:always]` has every attempt cut, and `[limit:stop]` waits for
/// the owner's stop and then reports the limit, as an attempt the owner
/// stopped just as its time ran out would.
struct Limited {
    inner: Arc<Synthetic>,
    seen: Arc<Mutex<Vec<Seen>>>,
}

#[async_trait::async_trait]
impl Provider for Limited {
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
        self.seen.lock().unwrap().push(Seen {
            attempt: input.attempt,
            outcomes: else_api::providers::prompt::outcomes_block(&input),
        });
        let cut = if input.text.contains("[limit:always]") || input.text.contains("[limit:stop]") {
            true
        } else {
            (1..=4).any(|n| input.text.contains(&format!("[limit:{n}]")) && input.attempt <= n)
        };
        if !cut {
            return self.inner.run(input, sink, stop).await;
        }
        let session = SessionState {
            native_id: Uuid::new_v4().to_string(),
            fingerprint: "limited".into(),
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
        let _ = sink
            .text(&format!("Benchmarked batch {}.", input.attempt))
            .await;
        if input.text.contains("[limit:stop]") {
            stop.requested().await;
        }
        else_api::providers::cli::attempt::deadline_failure()
    }
}

async fn app() -> (TestApp, Arc<Mutex<Vec<Seen>>>, Arc<Synthetic>) {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    sqlx::query("UPDATE installation SET default_provider_id = 'synthetic'")
        .execute(&db.pool)
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let inner = Arc::new(Synthetic::new(Duration::from_millis(1)));
    let provider = Arc::new(Limited {
        inner: inner.clone(),
        seen: seen.clone(),
    });
    let app = TestApp::builder()
        .db(Arc::new(db))
        .max_active(2)
        .providers(vec![provider])
        .start()
        .await;
    (app, seen, inner)
}

async fn send(app: &TestApp, bot: &str, text: &str) -> String {
    let reply = app.send(bot, Uuid::new_v4(), text, None).await;
    assert!(reply.status.is_success(), "{:?}", reply.body);
    reply.body["turn"]["id"].as_str().unwrap().to_owned()
}

async fn ended(app: &TestApp, turn: &str) -> (String, Option<String>, Option<String>, i64) {
    app.wait_turn(turn, &["completed", "failed", "interrupted", "uncertain"])
        .await;
    sqlx::query_as("SELECT status, error_code, error_message, attempt FROM turns WHERE id = ?1")
        .bind(turn.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn an_attempt_cut_at_the_limit_continues_with_its_progress() {
    let (app, seen, synthetic) = app().await;
    let bot = app.main_bot().await;
    let turn = send(&app, &bot, "Benchmark the four models. [limit:2]").await;

    let (status, code, _, attempt) = ended(&app, &turn).await;

    assert_eq!((status.as_str(), code, attempt), ("completed", None, 3));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter().map(|s| s.attempt).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert!(!seen[0].outcomes.contains("attempt limit"), "{seen:?}");
    for (attempt, continuation) in [(2, "1 of 4"), (3, "2 of 4")] {
        let outcomes = &seen[attempt - 1].outcomes;
        assert!(
            outcomes.contains("[else.bot attempt limit]")
                && outcomes.contains(&format!("continuation {continuation}")),
            "attempt {attempt}: {outcomes}"
        );
    }
    // The completing attempt runs in a fresh session: it gets the note and
    // the progress the cut attempts wrote.
    let packet = synthetic.packets().await.pop().unwrap();
    assert!(
        packet.contains("continuation 2 of 4")
            && packet.contains("Benchmarked batch 1.")
            && packet.contains("Benchmarked batch 2."),
        "{packet}"
    );
    let delivered: Vec<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT attempt, delivered_attempt FROM turn_limit_cuts WHERE turn_id = ?1 ORDER BY attempt",
    )
    .bind(turn.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(delivered, [(1, Some(2)), (2, Some(3))]);
    let view = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let shown = view.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.as_str())
        .unwrap()
        .clone();
    assert_eq!(
        shown["limit_continued"],
        json!({ "count": 2, "max": 4, "minutes": 30 }),
        "{shown}"
    );
}

#[tokio::test]
async fn the_continuation_cap_ends_the_request_as_failed_with_its_count() {
    let (app, seen, _) = app().await;
    let bot = app.main_bot().await;
    let turn = send(&app, &bot, "Benchmark until done. [limit:always]").await;

    let (status, code, message, attempt) = ended(&app, &turn).await;

    assert_eq!(
        (status.as_str(), code.as_deref(), attempt),
        ("failed", Some("turn_deadline"), 5)
    );
    assert_eq!(
        message.as_deref(),
        Some(
            "Stopped at the 30-minute attempt limit after continuing 4 times. Background jobs \
             it started may still be running."
        )
    );
    assert_eq!(seen.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn the_owner_stop_is_never_continued() {
    let (app, seen, _) = app().await;
    let bot = app.main_bot().await;
    let request = Uuid::new_v4();
    let reply = app
        .send(&bot, request, "Benchmark the models. [limit:stop]", None)
        .await;
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;

    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stopped.status.is_success(), "{:?}", stopped.body);

    let (status, code, _, attempt) = ended(&app, &turn).await;
    assert_eq!(
        (status.as_str(), code.as_deref(), attempt),
        ("interrupted", Some("stopped"), 1),
        "the stop's outcome, not the limit's"
    );
    let cuts: i64 = sqlx::query_scalar("SELECT count(*) FROM turn_limit_cuts")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(cuts, 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(seen.lock().unwrap().len(), 1, "no attempt follows a stop");
}

#[tokio::test]
async fn a_bot_takes_at_most_twelve_continuations_a_day() {
    let (app, _, _) = app().await;
    let bot = app.main_bot().await;
    for _ in 0..3 {
        let turn = send(&app, &bot, "Benchmark until done. [limit:always]").await;
        assert_eq!(ended(&app, &turn).await.3, 5);
    }

    let turn = send(&app, &bot, "One more benchmark. [limit:always]").await;

    let (status, _, message, attempt) = ended(&app, &turn).await;
    assert_eq!((status.as_str(), attempt), ("failed", 1));
    assert_eq!(
        message.as_deref(),
        Some(
            "Stopped at the 30-minute attempt limit. Background jobs it started may still be \
             running."
        )
    );
}
