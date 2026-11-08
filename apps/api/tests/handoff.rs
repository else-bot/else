mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestApp, TestDb};
use else_api::providers::Registry;
use else_api::providers::synthetic::Synthetic;
use uuid::Uuid;

struct Two {
    app: TestApp,
    a: Arc<Synthetic>,
    b: Arc<Synthetic>,
}

const A: &str = "synthetic";
const B: &str = "synthetic-b";

async fn two() -> Two {
    let db = TestDb::migrated().await;
    else_api::db::installation::setup(&db.pool).await.unwrap();
    two_on(Arc::new(db))
}

fn two_on(db: Arc<TestDb>) -> Two {
    let a = Arc::new(Synthetic::named(A, "Synthetic A", Duration::from_millis(1)));
    let b = Arc::new(Synthetic::named(B, "Synthetic B", Duration::from_millis(1)));
    let app = TestApp::with_registry(db, 4, Registry::new(vec![a.clone(), b.clone()]));
    Two { app, a, b }
}

async fn say(app: &TestApp, bot: &str, provider: &str, text: &str) -> (Uuid, String) {
    let reply = app.send(bot, Uuid::new_v4(), text, Some(provider)).await;
    assert!(reply.status.is_success(), "{:?}", reply.body);
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    let status = app
        .wait_turn(&turn, &["completed", "failed", "interrupted", "uncertain"])
        .await;
    (turn.parse().unwrap(), status)
}

fn last(packets: &[String]) -> String {
    packets.last().cloned().unwrap_or_default()
}

#[derive(Debug, sqlx::FromRow)]
struct Handoff {
    source_provider_id: String,
    target_provider_id: String,
    kind: String,
    reason: Option<String>,
    checkpoint: Option<String>,
    #[sqlx(json)]
    selected_turns: Vec<i64>,
    #[sqlx(json)]
    omitted_turns: Vec<i64>,
    boundary_turn_no: i64,
    state: String,
    payload_sha256: Option<Vec<u8>>,
    target_session_id: Option<Uuid>,
}

async fn handoff(app: &TestApp, turn: Uuid) -> Option<Handoff> {
    sqlx::query_as(
        "SELECT source_provider_id, target_provider_id, kind, reason, checkpoint, selected_turns,
                omitted_turns, boundary_turn_no, state, payload_sha256, target_session_id
         FROM handoffs WHERE turn_id = ?1",
    )
    .bind(turn)
    .fetch_optional(&app.db.pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_round_trip_hands_off_once_and_returning_sessions_get_only_their_delta() {
    let Two { app, a, b } = two().await;
    let bot = app.create_bot("Planner").await;
    say(&app, &bot, A, "Owner fact: the code word is PURPLE-7.").await;
    let (t2, _) = say(&app, &bot, A, "Second message on A.").await;
    assert!(
        handoff(&app, t2).await.is_none(),
        "same provider, no handoff"
    );
    let resumed = last(&a.packets().await);
    assert!(resumed.ends_with("Second message on A."), "{resumed}");
    assert!(
        !resumed.contains("PURPLE-7"),
        "a resumed session gets no history"
    );
    assert!(
        !resumed.contains("[else.bot saved permissions]"),
        "a resumed session already holds the unchanged permissions: {resumed}"
    );

    let (t3, status) = say(&app, &bot, B, "Third, on B: what is the code word?").await;
    assert_eq!(status, "completed");
    let packet = last(&b.packets().await);
    assert!(packet.contains("[else.bot handoff]"), "{packet}");
    assert!(packet.contains("Synthetic checkpoint from synthetic covering 4 recorded messages."));
    assert!(packet.contains("[turn 1] Owner: Owner fact: the code word is PURPLE-7."));
    assert!(packet.contains("[turn 2] Assistant: Synthetic reply"));
    assert_eq!(
        packet
            .matches("Third, on B: what is the code word?")
            .count(),
        1,
        "the follow-up once"
    );
    let h = handoff(&app, t3).await.unwrap();
    assert_eq!(
        (h.source_provider_id.as_str(), h.target_provider_id.as_str()),
        (A, B)
    );
    assert_eq!(h.kind, "llm");
    assert_eq!(h.boundary_turn_no, 2);
    assert_eq!(h.selected_turns, vec![1, 2]);
    assert!(h.omitted_turns.is_empty());
    assert_eq!(h.state, "accepted");
    assert!(h.payload_sha256.is_some() && h.target_session_id.is_some());
    assert!(!h.checkpoint.unwrap().contains("Third"));

    let (t4, _) = say(&app, &bot, B, "Fourth, still B.").await;
    assert!(handoff(&app, t4).await.is_none());
    let resumed = last(&b.packets().await);
    assert!(resumed.ends_with("Fourth, still B."), "{resumed}");
    assert!(!resumed.contains("Third, on B:"), "{resumed}");
    assert!(
        !resumed.contains("[else.bot saved permissions]"),
        "a resumed session already holds the unchanged permissions: {resumed}"
    );

    let a_before = a.packets().await.len();
    let (t5, _) = say(&app, &bot, A, "Fifth, back on A.").await;
    assert_eq!(a.packets().await.len(), a_before + 1);
    let packet = last(&a.packets().await);
    assert!(
        packet.contains("Synthetic checkpoint from synthetic-b"),
        "{packet}"
    );
    assert!(packet.contains("[turn 3] Owner: Third, on B"));
    assert!(packet.contains("[turn 4] Assistant:"));
    assert!(
        !packet.contains("[turn 1]") && !packet.contains("[turn 2]"),
        "{packet}"
    );
    let h = handoff(&app, t5).await.unwrap();
    assert_eq!(
        (h.source_provider_id.as_str(), h.target_provider_id.as_str()),
        (B, A)
    );
    assert_eq!(h.selected_turns, vec![3, 4]);
    let sessions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM provider_sessions WHERE bot_id = ?1")
            .bind(bot.parse::<Uuid>().unwrap())
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(sessions, 2, "one native session per provider, resumed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_or_missing_source_uses_the_recorded_history() {
    let Two { app, a, b } = two().await;
    let bot = app.create_bot("Planner").await;
    say(&app, &bot, A, "Remember BLUE-3.").await;
    a.fail_checkpoints(Some("cannot prove a no-tool run")).await;
    let (t2, status) = say(&app, &bot, B, "On B now.").await;
    assert_eq!(status, "completed");
    let h = handoff(&app, t2).await.unwrap();
    assert_eq!(h.kind, "fallback");
    assert_eq!(h.reason.as_deref(), Some("cannot prove a no-tool run"));
    assert!(h.checkpoint.is_none());
    let packet = last(&b.packets().await);
    assert!(
        packet.contains("Task checkpoint: unavailable (cannot prove a no-tool run)"),
        "{packet}"
    );
    assert!(packet.contains("[turn 1] Owner: Remember BLUE-3."));
    assert_eq!(packet.matches("On B now.").count(), 1);

    say(&app, &bot, A, "Back to A once.").await;
    let db = app.db.clone();
    drop(app);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let b2 = Arc::new(Synthetic::named(B, "Synthetic B", Duration::from_millis(1)));
    let app = TestApp::with_registry(db, 4, Registry::new(vec![b2.clone()]));
    let (t4, status) = say(&app, &bot, B, "B after a restart without A.").await;
    assert_eq!(status, "completed");
    let h = handoff(&app, t4).await.unwrap();
    assert_eq!(h.kind, "fallback");
    assert!(h.reason.unwrap().contains("not configured"));
    let packet = last(&b2.packets().await);
    assert!(
        packet.contains("[turn 3] Owner: Back to A once."),
        "{packet}"
    );
    assert!(!packet.contains("[turn 1]"), "{packet}");
    let _ = b;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_target_session_is_rebuilt_from_the_full_conversation() {
    let Two { app, a, b } = two().await;
    let bot = app.create_bot("Planner").await;
    say(&app, &bot, B, "First on B.").await;
    say(&app, &bot, A, "Then A.").await;
    let natives: Vec<String> = sqlx::query_scalar(
        "SELECT native_session_id FROM provider_sessions WHERE provider_id = ?1",
    )
    .bind(B)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    b.forget_sessions(&natives).await;
    let (t3, status) = say(&app, &bot, B, "B again after losing its session.").await;
    assert_eq!(status, "completed");
    let packet = last(&b.packets().await);
    assert!(
        packet.contains("[turn 1] Owner: First on B."),
        "rebuilt from the start: {packet}"
    );
    assert!(packet.contains("[turn 2] Owner: Then A."));
    let h = handoff(&app, t3).await.unwrap();
    assert_eq!(h.selected_turns, vec![1, 2]);
    let active: Vec<String> = sqlx::query_scalar(
        "SELECT native_session_id FROM provider_sessions WHERE provider_id = ?1 AND status = 'active'",
    )
    .bind(B)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(active.len(), 1);
    assert!(!natives.contains(&active[0]), "a replacement session");
    let _ = a;
}

#[tokio::test(flavor = "multi_thread")]
async fn interrupted_and_uncertain_turns_are_history_not_replayed() {
    let Two { app, a, b } = two().await;
    let bot = app.create_bot("Planner").await;
    say(&app, &bot, A, "Plain first turn.").await;
    let reply = app
        .send(&bot, Uuid::new_v4(), "Long work [synthetic:hang]", Some(A))
        .await;
    let hung = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&hung, &["running"]).await;
    let stop = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(serde_json::json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success());
    assert_eq!(app.wait_turn(&hung, &["interrupted"]).await, "interrupted");
    let reply = app
        .send(
            &bot,
            Uuid::new_v4(),
            "Pay the invoice [synthetic:stall-send]",
            Some(A),
        )
        .await;
    let stalled = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&stalled, &["sending"]).await;
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(serde_json::json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    assert_eq!(app.wait_turn(&stalled, &["uncertain"]).await, "uncertain");

    let (t4, status) = say(&app, &bot, B, "What happened so far?").await;
    assert_eq!(status, "completed");
    let packet = last(&b.packets().await);
    assert!(
        packet.contains("[turn 2, interrupted] Owner: Long work [synthetic:hang]"),
        "{packet}"
    );
    assert!(
        packet.contains("[turn 3, uncertain] Owner: Pay the invoice"),
        "{packet}"
    );
    assert_eq!(packet.matches("What happened so far?").count(), 1);
    assert!(packet.ends_with("Owner's latest message:\nWhat happened so far?"));
    assert_eq!(
        handoff(&app, t4).await.unwrap().selected_turns,
        vec![1, 2, 3]
    );
    let a_before = a.packets().await.len();
    say(&app, &bot, A, "And on A?").await;
    assert_eq!(a.packets().await.len(), a_before + 1);
    let packet = last(&a.packets().await);
    assert!(
        packet.contains("[turn 1] Owner: Plain first turn."),
        "a fresh session: {packet}"
    );
    let sends = a
        .packets()
        .await
        .iter()
        .filter(|p| p.ends_with("Pay the invoice [synthetic:stall-send]"))
        .count();
    assert_eq!(sends, 1, "never sent again as a request");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_uncertain_handoff_is_recorded_and_its_session_retired() {
    let Two { app, a, b } = two().await;
    let bot = app.create_bot("Planner").await;
    say(&app, &bot, A, "On A.").await;
    let reply = app
        .send(&bot, Uuid::new_v4(), "On B [synthetic:stall-send]", Some(B))
        .await;
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["sending"]).await;
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(serde_json::json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    assert_eq!(app.wait_turn(&turn, &["uncertain"]).await, "uncertain");
    let h = handoff(&app, turn.parse().unwrap()).await.unwrap();
    assert_eq!(h.state, "uncertain");
    let (_, status) = say(&app, &bot, B, "Next on B.").await;
    assert_eq!(status, "completed");
    let packet = last(&b.packets().await);
    assert!(packet.contains("[turn 1] Owner: On A."), "{packet}");
    assert!(packet.contains("[turn 2, uncertain] Owner: On B [synthetic:stall-send]"));
    assert!(packet.ends_with("Next on B."));
    let _ = a;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_profile_provider_is_unavailable_until_an_explicit_profile_is_saved() {
    let app = TestApp::installed().await;
    sqlx::query("DELETE FROM provider_profiles WHERE provider_id = 'codex'")
        .execute(&app.db.pool)
        .await
        .unwrap();
    let entry = |caps: &common::Reply| {
        caps.body["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "codex")
            .cloned()
            .unwrap()
    };
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    let codex = entry(&caps);
    assert_eq!(codex["available"], false);
    assert_eq!(codex["requires_profile"], true);
    assert_eq!(codex["profile_configured"], false);
    assert!(
        codex["unavailable_reason"]
            .as_str()
            .unwrap()
            .contains("profile")
    );
    let bot = app.main_bot().await;
    let refused = app.send(&bot, Uuid::new_v4(), "hi", Some("codex")).await;
    assert!(!refused.status.is_success(), "{:?}", refused.body);
    let partial = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(serde_json::json!({ "model": "gpt-6.1-sol" })),
        )
        .await;
    assert_eq!(partial.status, axum::http::StatusCode::BAD_REQUEST);
    assert_eq!(partial.body["error"]["code"], "profile_required");
    let bad = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(serde_json::json!({ "model": "gpt-4", "reasoning_effort": "low", "service_tier": "default" })),
        )
        .await;
    assert_eq!(
        bad.body["error"]["code"], "invalid_profile",
        "{:?}",
        bad.body
    );
    let saved = app
        .owner(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(serde_json::json!({ "model": "gpt-6.1-sol", "reasoning_effort": "low", "service_tier": "default" })),
        )
        .await;
    assert!(saved.status.is_success(), "{:?}", saved.body);
    assert_eq!(saved.body["profile"]["revision"], 1);
    let caps = app.owner("GET", "/v1/capabilities", None).await;
    assert_eq!(entry(&caps)["available"], true);
    let (_, status) = say(&app, &bot, "codex", "now it runs").await;
    assert_eq!(status, "completed");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_switch_stops_the_other_providers_active_turn_and_carries_job_records() {
    let Two { app, a, b } = two().await;
    let bot = app.create_bot("Planner").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let job = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib)
         VALUES (?1, 'elsebot-x', 'img', 1, 2048)",
    )
    .bind(bot_id)
    .execute(&app.db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO guest_jobs (id, bot_id, command, status, deadline)
         VALUES (?1, ?2, 'make release', 'running', strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '+1 hours'))",
    )
    .bind(job)
    .bind(bot_id)
    .execute(&app.db.pool)
    .await
    .unwrap();
    let reply = app
        .send(&bot, Uuid::new_v4(), "Working [synthetic:hang]", Some(A))
        .await;
    let hung = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&hung, &["running"]).await;
    let (_, status) = say(&app, &bot, B, "Switching to B.").await;
    assert_eq!(status, "completed");
    assert_eq!(app.wait_turn(&hung, &["interrupted"]).await, "interrupted");
    let packet = last(&b.packets().await);
    assert!(
        packet.contains("[turn 1, interrupted] Owner: Working [synthetic:hang]"),
        "{packet}"
    );
    assert!(
        packet.contains(&format!("- job {job} (running, started "))
            && packet.contains("make release"),
        "the job record is carried: {packet}"
    );
    let status: String = sqlx::query_scalar("SELECT status FROM guest_jobs WHERE id = ?1")
        .bind(job)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(status, "running");
    let (_, status) = say(&app, &bot, B, "Still B.").await;
    assert_eq!(status, "completed");
    let _ = a;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_after_sending_makes_the_handoff_uncertain() {
    let Two { app, a, b } = two().await;
    let bot = app.create_bot("Planner").await;
    say(&app, &bot, A, "On A.").await;
    let reply = app
        .send(&bot, Uuid::new_v4(), "On B [synthetic:stall-send]", Some(B))
        .await;
    let turn = reply.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["sending"]).await;
    let db = app.db.clone();
    drop(app);
    let id: Uuid = turn.parse().unwrap();
    for _ in 0..100 {
        let s: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
            .bind(id)
            .fetch_one(&db.pool)
            .await
            .unwrap();
        if s != "sending" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    sqlx::query("UPDATE turns SET status = 'sending', finished_at = NULL WHERE id = ?1")
        .bind(id)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE handoffs SET state = 'sending' WHERE turn_id = ?1")
        .bind(id)
        .execute(&db.pool)
        .await
        .unwrap();
    else_api::db::turns::recover_after_restart(&db.pool)
        .await
        .unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM handoffs WHERE turn_id = ?1")
        .bind(turn.parse::<Uuid>().unwrap())
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(state, "uncertain");
    let _ = (a, b);
}
