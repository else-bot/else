mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use chrono::DateTime;
use common::TestApp;
use common::recorder::{self, GATE, Harness, tool};
use else_api::app::providers::{
    Availability, Catalog, Profile, Provider, TurnInput, TurnOutcome, TurnSink,
};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const YEARLY: &str = "0 9 1 1 *";
const MODELS: &[&str] = &["gpt-6.1-sol", "gpt-6-sol", "gpt-6-luna"];
const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
const TIERS: &[&str] = &["default", "priority"];

/// A second provider with a model catalog, so routines can choose a model
/// other than the recorder's.
struct Catalogued;

#[async_trait::async_trait]
impl Provider for Catalogued {
    fn id(&self) -> &'static str {
        "codex"
    }
    fn label(&self) -> &'static str {
        "Codex (catalog double)"
    }
    fn availability(&self) -> Availability {
        Availability::Available
    }
    fn uses_profile(&self) -> bool {
        true
    }
    fn validate_profile(&self, p: &Profile) -> Result<(), String> {
        let ok = MODELS.contains(&p.model.as_str())
            && EFFORTS.contains(&p.reasoning_effort.as_str())
            && TIERS.contains(&p.service_tier.as_str());
        ok.then_some(())
            .ok_or_else(|| format!("unsupported profile {p:?}"))
    }
    fn catalog(&self) -> Catalog {
        Catalog {
            models: MODELS,
            featured_models: &[],
            efforts: EFFORTS,
            tiers: TIERS,
        }
    }
    async fn run(
        &self,
        input: TurnInput,
        sink: &dyn TurnSink,
        stop: else_api::app::providers::Stop,
    ) -> TurnOutcome {
        Synthetic::new(Duration::from_millis(5))
            .run(input, sink, stop)
            .await
    }
}

async fn harness() -> Harness {
    recorder::harness_with_providers(4, None, vec![Arc::new(Catalogued)]).await
}

fn digest(name: &str, extra: Value) -> Value {
    let mut args = json!({ "name": name, "prompt": "Summarize my unread mail.", "cron": YEARLY });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    args
}

fn sol(effort: &str, tier: &str) -> Value {
    json!({ "provider_id": "codex", "model": "gpt-6.1-sol",
            "reasoning_effort": effort, "service_tier": tier })
}

async fn create(app: &TestApp, token: &str, args: Value) -> Value {
    let called = tool(app, token, "routine_create", args).await;
    assert!(!called.error, "{}", called.body);
    assert_eq!(called.result["created"], true, "{}", called.body);
    called.result
}

async fn patch(app: &TestApp, bot: &str, routine: &Value, body: Value) -> common::Reply {
    let id = routine["id"].as_str().unwrap();
    app.owner(
        "PATCH",
        &format!("/v1/bots/{bot}/routines/{id}"),
        Some(body),
    )
    .await
}

async fn fire(app: &TestApp, routine: &Value) -> Option<Uuid> {
    let due = DateTime::parse_from_rfc3339(routine["next_run_at"].as_str().unwrap())
        .unwrap()
        .to_utc();
    else_api::db::routines::admit_due(&app.db.pool, app.scheduler.registry(), due, 8)
        .await
        .unwrap();
    app.scheduler.wake();
    let id: Uuid = routine["id"].as_str().unwrap().parse().unwrap();
    sqlx::query_scalar("SELECT turn_id FROM routine_runs WHERE routine_id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn finish(h: &Harness, turn: Uuid) {
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
}

async fn count(app: &TestApp, sql: &str, bind: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .bind(bind.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_named_model_runs_the_routine_without_touching_the_bot_or_its_chat_session() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let created = create(
        &h.app,
        &token,
        digest("Digest", json!({ "model": "GPT 6.1 Sol" })),
    )
    .await;
    let routine = &created["routine"];
    assert_eq!(routine["model"]["provider_id"], "codex", "{created}");
    assert_eq!(routine["model"]["model"], "gpt-6.1-sol");
    assert!(EFFORTS.contains(&routine["model"]["reasoning_effort"].as_str().unwrap()));
    assert!(TIERS.contains(&routine["model"]["service_tier"].as_str().unwrap()));
    assert_eq!(routine["requested_model"], Value::Null);
    assert!(
        created["model_note"]
            .as_str()
            .unwrap()
            .contains("gpt-6.1-sol")
    );
    let retry = tool(
        &h.app,
        &token,
        "routine_create",
        digest("Digest", json!({ "model": "GPT 6.1 Sol" })),
    )
    .await;
    assert_eq!(retry.result["created"], false, "{}", retry.body);
    let changed = tool(
        &h.app,
        &token,
        "routine_create",
        digest("Digest", json!({ "model": "gpt-6-luna" })),
    )
    .await;
    assert!(
        changed
            .result
            .as_str()
            .unwrap()
            .starts_with("CREATE_CONFLICT"),
        "a retry asking for another model conflicts"
    );
    finish(&h, turn).await;

    let run = fire(&h.app, routine).await.expect("the run was queued");
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    let (provider, profile, lane): (String, String, String) =
        sqlx::query_as("SELECT provider_id, profile, session_lane FROM turns WHERE id = ?1")
            .bind(run)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(provider, "codex");
    let profile: Value = serde_json::from_str(&profile).unwrap();
    assert_eq!(profile["model"], "gpt-6.1-sol");
    assert_eq!(lane, format!("routine:{}", routine["id"].as_str().unwrap()));
    assert_eq!(
        count(
            &h.app,
            "SELECT count(*) FROM bot_profiles WHERE bot_id = ?1",
            &bot
        )
        .await,
        0,
        "a routine's model never becomes the bot's"
    );
    let sessions: Vec<(String, String)> = sqlx::query_as(
        "SELECT provider_id, lane FROM provider_sessions WHERE status = 'active' ORDER BY lane",
    )
    .fetch_all(&h.app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        sessions,
        vec![
            ("synthetic".to_owned(), String::new()),
            ("codex".to_owned(), lane.clone())
        ],
        "the chat session stays active beside the routine's own"
    );

    let (next, _) = h.running(&bot, GATE).await;
    finish(&h, next).await;
    let (provider,): (String,) = sqlx::query_as("SELECT provider_id FROM turns WHERE id = ?1")
        .bind(next)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(provider, "synthetic", "chat keeps the bot's own provider");
    assert_eq!(
        count(
            &h.app,
            "SELECT count(*) FROM handoffs WHERE bot_id = ?1",
            &bot
        )
        .await,
        0,
        "a routine run on its own model never triggers a provider handoff"
    );
    assert_eq!(
        count(
            &h.app,
            "SELECT count(*) FROM provider_sessions WHERE bot_id = ?1 AND status = 'retired'",
            &bot
        )
        .await,
        0
    );
}

#[tokio::test]
async fn create_retry_describes_the_saved_model_after_an_owner_edit() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let args = digest("Digest", json!({ "model": "GPT 6.1 Sol" }));
    let saved = create(&h.app, &token, args.clone()).await;
    let routine = &saved["routine"];
    assert_eq!(
        patch(&h.app, &bot, routine, json!({ "model": null }))
            .await
            .status,
        StatusCode::OK
    );
    let retry = tool(&h.app, &token, "routine_create", args.clone()).await;
    assert_eq!(retry.result["created"], false);
    assert_eq!(retry.result["routine"]["model"], Value::Null);
    assert!(
        retry.result["model_note"]
            .as_str()
            .unwrap()
            .contains("current model"),
        "{}",
        retry.body
    );

    let luna = json!({ "provider_id": "codex", "model": "gpt-6-luna",
                       "reasoning_effort": "low", "service_tier": "default" });
    assert_eq!(
        patch(&h.app, &bot, routine, json!({ "model": luna }))
            .await
            .status,
        StatusCode::OK
    );
    let retry = tool(&h.app, &token, "routine_create", args).await;
    assert_eq!(retry.result["routine"]["model"]["model"], "gpt-6-luna");
    assert!(
        retry.result["model_note"]
            .as_str()
            .unwrap()
            .contains("gpt-6-luna"),
        "{}",
        retry.body
    );
    finish(&h, turn).await;
}

#[tokio::test]
async fn two_models_on_one_provider_keep_separate_native_sessions() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let routine = create(
        &h.app,
        &token,
        digest("Digest", json!({ "model": "gpt-6-luna" })),
    )
    .await["routine"]
        .clone();
    finish(&h, turn).await;
    let saved = h
        .app
        .owner(
            "PUT",
            &format!("/v1/bots/{bot}/profile"),
            Some(sol("medium", "priority")),
        )
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);

    let chat = h.send(&bot, "Hello on the chat model.").await;
    h.app.wait_turn(&chat.to_string(), &["completed"]).await;
    let session_for = |id| {
        sqlx::query_scalar::<_, Uuid>("SELECT provider_session_id FROM turns WHERE id=?1").bind(id)
    };
    let chat_session = session_for(chat).fetch_one(&h.app.db.pool).await.unwrap();
    let run = fire(&h.app, &routine).await.unwrap();
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    let routine_session = session_for(run).fetch_one(&h.app.db.pool).await.unwrap();
    assert_ne!(
        chat_session, routine_session,
        "routine uses a separate native session even on the chat provider"
    );
    let next = h.send(&bot, "Continue on the chat model.").await;
    h.app.wait_turn(&next.to_string(), &["completed"]).await;
    assert_eq!(
        session_for(next).fetch_one(&h.app.db.pool).await.unwrap(),
        chat_session,
        "chat resumes its original native session"
    );
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provider_sessions WHERE id IN (?1, ?2) AND status='active'",
    )
    .bind(chat_session)
    .bind(routine_session)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    assert_eq!(active, 2, "neither model retires the other's session");
    let chat_profile: String = sqlx::query_scalar("SELECT profile FROM turns WHERE id=?1")
        .bind(next)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&chat_profile).unwrap()["model"],
        "gpt-6.1-sol"
    );
}

#[tokio::test]
async fn an_admitted_run_keeps_its_model_after_the_routine_is_edited() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let routine = create(
        &h.app,
        &token,
        digest("Digest", json!({ "model": "GPT 6.1 Sol" })),
    )
    .await["routine"]
        .clone();
    let run = fire(&h.app, &routine).await.unwrap();
    h.app.wait_turn(&run.to_string(), &["queued"]).await;
    let reset = patch(&h.app, &bot, &routine, json!({ "model": null })).await;
    assert_eq!(reset.status, StatusCode::OK);
    finish(&h, turn).await;
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    let (provider, profile): (String, String) =
        sqlx::query_as("SELECT provider_id, profile FROM turns WHERE id=?1")
            .bind(run)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(provider, "codex");
    assert_eq!(
        serde_json::from_str::<Value>(&profile).unwrap()["model"],
        "gpt-6.1-sol"
    );
}

#[tokio::test]
async fn unknown_or_omitted_models_never_block_creation() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;

    let omitted = create(&h.app, &token, digest("Plain", json!({}))).await;
    assert_eq!(omitted["routine"]["model"], Value::Null);
    assert_eq!(omitted["routine"]["requested_model"], Value::Null);
    assert_eq!(omitted["model_note"], Value::Null);

    for (name, extra, requested) in [
        ("Unknown", json!({ "model": "Gemini 3" }), "Gemini 3"),
        ("Vague", json!({ "model": "Sol" }), "Sol"),
        (
            "Effort",
            json!({ "model": "gpt-6.1-sol", "reasoning_effort": "ultra" }),
            "gpt-6.1-sol",
        ),
        (
            "Provider",
            json!({ "model": "gpt-6.1-sol", "provider_id": "claude" }),
            "gpt-6.1-sol",
        ),
    ] {
        let created = create(&h.app, &token, digest(name, extra)).await;
        assert_eq!(created["routine"]["model"], Value::Null, "{created}");
        assert_eq!(created["routine"]["requested_model"], requested);
        let note = created["model_note"].as_str().unwrap();
        assert!(note.contains("Bot details > Routines"), "{note}");
    }
    finish(&h, turn).await;

    let routines = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/routines"), None)
        .await;
    let routines = routines.body["routines"].as_array().unwrap();
    for other in routines.iter().filter(|r| r["name"] != "Unknown") {
        let paused = patch(&h.app, &bot, other, json!({ "enabled": false })).await;
        assert_eq!(paused.status, StatusCode::OK);
    }
    let unknown = routines.iter().find(|r| r["name"] == "Unknown").unwrap();
    assert_eq!(unknown["requested_model"], "Gemini 3");
    let run = fire(&h.app, unknown).await.expect("queued");
    let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM routine_runs")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(runs, 1, "only the unknown-model routine was due");
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    let (provider, lane): (String, String) =
        sqlx::query_as("SELECT provider_id, session_lane FROM turns WHERE id = ?1")
            .bind(run)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(
        (provider.as_str(), lane.as_str()),
        ("synthetic", ""),
        "an inheriting routine runs exactly as before"
    );
}

#[tokio::test]
async fn the_owner_sets_keeps_and_resets_a_routine_model() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let routine = create(
        &h.app,
        &token,
        digest("Digest", json!({ "model": "Gemini 3" })),
    )
    .await["routine"]
        .clone();
    finish(&h, turn).await;

    let set = patch(
        &h.app,
        &bot,
        &routine,
        json!({ "model": sol("high", "priority") }),
    )
    .await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    assert_eq!(set.body["routine"]["model"], sol("high", "priority"));
    assert_eq!(
        set.body["routine"]["requested_model"],
        Value::Null,
        "a valid choice clears the unresolved request"
    );

    let paused = patch(&h.app, &bot, &routine, json!({ "enabled": false })).await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);
    assert_eq!(paused.body["routine"]["enabled"], false);
    assert_eq!(paused.body["routine"]["model"], sol("high", "priority"));

    for (body, code) in [
        (
            json!({ "model": sol("ultra", "priority") }),
            Some("invalid_profile"),
        ),
        (
            json!({ "model": { "provider_id": "gemini", "model": "g",
                               "reasoning_effort": "low", "service_tier": "default" } }),
            Some("unknown_provider"),
        ),
        (json!({ "model": { "model": "gpt-6.1-sol" } }), None),
    ] {
        let rejected = patch(&h.app, &bot, &routine, body).await;
        assert!(rejected.status.is_client_error(), "{}", rejected.body);
        if let Some(code) = code {
            assert_eq!(rejected.body["error"]["code"], code, "{}", rejected.body);
        }
    }

    let reset = patch(&h.app, &bot, &routine, json!({ "model": null })).await;
    assert_eq!(reset.status, StatusCode::OK, "{}", reset.body);
    assert_eq!(reset.body["routine"]["model"], Value::Null);
    assert_eq!(reset.body["routine"]["enabled"], false);
}

#[tokio::test]
async fn the_bot_keeps_resets_or_changes_a_routine_model() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let routine = create(
        &h.app,
        &token,
        digest(
            "Digest",
            json!({ "model": "gpt-6-luna", "service_tier": "default" }),
        ),
    )
    .await["routine"]
        .clone();
    assert_eq!(routine["model"]["model"], "gpt-6-luna");
    assert_eq!(routine["model"]["service_tier"], "default");
    let id = routine["id"].clone();
    let update = |args: Value| tool(&h.app, &token, "routine_update", args);

    let renamed = update(json!({ "routine_id": id, "name": "Morning digest" })).await;
    assert!(!renamed.error, "{}", renamed.body);
    assert_eq!(renamed.result["routine"]["model"], routine["model"]);
    assert_eq!(renamed.result["model_note"], Value::Null);

    let changed = update(json!({ "routine_id": id, "model": "6.1 sol" })).await;
    assert_eq!(changed.result["routine"]["model"]["model"], "gpt-6.1-sol");

    let unknown = update(json!({ "routine_id": id, "model": "Gemini 3" })).await;
    assert!(!unknown.error, "an unresolved model never fails the update");
    assert_eq!(unknown.result["routine"]["model"], Value::Null);
    assert_eq!(unknown.result["routine"]["requested_model"], "Gemini 3");

    let reset = update(json!({ "routine_id": id, "model": null })).await;
    assert_eq!(reset.result["routine"]["model"], Value::Null);
    assert_eq!(reset.result["routine"]["requested_model"], Value::Null);
    finish(&h, turn).await;
}

#[tokio::test]
async fn an_unavailable_saved_model_skips_runs_and_keeps_the_choice() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let routine = create(&h.app, &token, digest("Digest", json!({}))).await["routine"].clone();
    finish(&h, turn).await;
    let gone = json!({ "provider_id": "claude", "model": "claude-opus-5-5",
                       "reasoning_effort": "high", "service_tier": "default" });
    sqlx::query("UPDATE routines SET model = ?2 WHERE id = ?1")
        .bind(routine["id"].as_str().unwrap().parse::<Uuid>().unwrap())
        .bind(gone.to_string())
        .execute(&h.app.db.pool)
        .await
        .unwrap();

    assert_eq!(fire(&h.app, &routine).await, None, "the run was skipped");
    let listed = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/routines"), None)
        .await;
    let saved = &listed.body["routines"][0];
    assert_eq!(saved["last_run"]["skip_reason"], "provider_unavailable");
    assert_eq!(saved["model"], gone, "the owner's choice is never switched");
}

#[tokio::test]
async fn a_bot_created_with_a_named_model_runs_on_it() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let args = json!({ "name": "Sol", "purpose": "Answers with Sol.", "model": "GPT 6.1 Sol" });
    let created = tool(&h.app, &token, "create_bot", args.clone()).await;
    assert!(!created.error, "{}", created.body);
    assert_eq!(
        created.result["model"]["provider_id"], "codex",
        "{}",
        created.body
    );
    assert_eq!(created.result["model"]["model"], "gpt-6.1-sol");
    let bot = created.result["bot_id"].as_str().unwrap().to_owned();

    let effort = created.result["model"]["reasoning_effort"]
        .as_str()
        .unwrap();
    let other = if effort == "xhigh" { "low" } else { "xhigh" };
    sqlx::query(
        "INSERT INTO provider_profiles (provider_id, model, reasoning_effort, service_tier)
         VALUES ('codex', 'gpt-6-luna', ?1, 'default')
         ON CONFLICT (provider_id) DO UPDATE SET reasoning_effort = excluded.reasoning_effort",
    )
    .bind(other)
    .execute(&h.app.db.pool)
    .await
    .unwrap();
    let edited = h
        .app
        .owner(
            "PUT",
            &format!("/v1/bots/{bot}/profile"),
            Some(json!({ "provider_id": "codex", "model": "gpt-6-luna",
                         "reasoning_effort": "high", "service_tier": "default" })),
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK, "{}", edited.body);

    let retry = tool(&h.app, &token, "create_bot", args).await;
    assert_eq!(
        retry.result["created"], false,
        "a retry matches after the defaults change: {}",
        retry.body
    );
    assert_eq!(
        retry.result["model"]["model"], "gpt-6-luna",
        "a retry reports the bot's saved model"
    );
    let changed = tool(
        &h.app,
        &token,
        "create_bot",
        json!({ "name": "Sol", "purpose": "Answers with Sol.", "model": "gpt-6-luna" }),
    )
    .await;
    assert!(
        changed
            .result
            .as_str()
            .unwrap()
            .starts_with("REQUEST_CONFLICT"),
        "{}",
        changed.body
    );
    finish(&h, turn).await;

    let profile = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/profile"), None)
        .await;
    assert_eq!(profile.status, StatusCode::OK);
    assert_eq!(profile.body["profile"]["own"], true, "{}", profile.body);
    assert_eq!(profile.body["profile"]["model"], "gpt-6-luna");

    let first = h.send(&bot, "hello").await;
    h.app.wait_turn(&first.to_string(), &["completed"]).await;
    let (provider, profile): (String, String) =
        sqlx::query_as("SELECT provider_id, profile FROM turns WHERE id = ?1")
            .bind(first)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(provider, "codex");
    let profile: Value = serde_json::from_str(&profile).unwrap();
    assert_eq!(profile["model"], "gpt-6-luna");
}

#[tokio::test]
async fn a_bot_is_not_created_on_a_model_that_is_not_available() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let before = count(&h.app, "SELECT count(*) FROM bots WHERE id != ?1", &main).await;
    let refused = tool(
        &h.app,
        &token,
        "create_bot",
        json!({ "name": "Grok", "purpose": "Answers with Grok.", "model": "Grok 4.7" }),
    )
    .await;
    let text = refused.result.as_str().unwrap();
    assert!(text.starts_with("MODEL_NOT_AVAILABLE"), "{}", refused.body);
    assert!(
        text.contains("gpt-6.1-sol"),
        "the error lists the models: {text}"
    );
    assert_eq!(
        count(&h.app, "SELECT count(*) FROM bots WHERE id != ?1", &main).await,
        before,
        "no bot was created"
    );

    let extras = tool(
        &h.app,
        &token,
        "create_bot",
        json!({ "name": "Fast", "purpose": "Answers fast.", "service_tier": "priority" }),
    )
    .await;
    assert!(
        extras
            .result
            .as_str()
            .unwrap()
            .starts_with("INVALID_ARGUMENTS"),
        "{}",
        extras.body
    );
    finish(&h, turn).await;
}

#[tokio::test]
async fn a_bot_sets_another_bots_model_and_its_next_turn_runs_on_it() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let dev = h.app.create_bot("Dev").await;
    let profile = |bot: String| {
        let app = &h.app;
        async move {
            app.owner("GET", &format!("/v1/bots/{bot}/profile"), None)
                .await
                .body["profile"]
                .clone()
        }
    };
    let before = profile(dev.clone()).await;
    let (turn, token) = h.running(&main, GATE).await;

    let unknown = tool(
        &h.app,
        &token,
        "set_bot_model",
        json!({ "target_bot_id": dev, "model": "Grok 4.7" }),
    )
    .await;
    let text = unknown.result.as_str().unwrap_or_default();
    assert!(text.starts_with("MODEL_NOT_AVAILABLE"), "{}", unknown.body);
    assert!(
        text.contains("gpt-6.1-sol"),
        "the error lists models: {text}"
    );
    assert_eq!(profile(dev.clone()).await, before, "nothing was saved");

    let args = json!({ "target_bot_id": dev, "model": "GPT 6.1 Sol",
                       "reasoning_effort": "high", "service_tier": "default" });
    let set = tool(&h.app, &token, "set_bot_model", args.clone()).await;
    assert!(!set.error, "{}", set.body);
    assert_eq!(set.result["bot_id"], dev);
    assert_eq!(set.result["model"]["provider_id"], "codex");
    assert_eq!(set.result["model"]["model"], "gpt-6.1-sol");
    assert_eq!(set.result["model"]["reasoning_effort"], "high");
    assert_eq!(set.result["model"]["service_tier"], "default");
    let retry = tool(&h.app, &token, "set_bot_model", args).await;
    assert_eq!(retry.result, set.result, "a retry changes nothing");
    let saved = profile(dev.clone()).await;
    assert_eq!(saved["own"], true, "{saved}");
    assert_eq!(saved["model"], "gpt-6.1-sol");
    assert_eq!(saved["revision"], 1, "the retry kept the revision");
    let updated = || {
        count(
            &h.app,
            "SELECT count(*) FROM bot_events WHERE bot_id = ?1 AND kind = 'bot.updated'",
            &dev,
        )
    };
    assert_eq!(updated().await, 1, "one change tells clients once");

    let effort = tool(
        &h.app,
        &token,
        "set_bot_model",
        json!({ "target_bot_id": dev, "reasoning_effort": "low" }),
    )
    .await;
    assert!(!effort.error, "{}", effort.body);
    assert_eq!(
        effort.result["model"]["model"], "gpt-6.1-sol",
        "the model is kept"
    );
    assert_eq!(effort.result["model"]["reasoning_effort"], "low");
    assert_eq!(effort.result["model"]["service_tier"], "default");
    assert_eq!(updated().await, 2);

    let own = tool(
        &h.app,
        &token,
        "set_bot_model",
        json!({ "target_bot_id": main, "model": "gpt-6-luna" }),
    )
    .await;
    assert!(!own.error, "{}", own.body);
    assert_eq!(profile(main.clone()).await["model"], "gpt-6-luna");
    finish(&h, turn).await;

    let next = h.send(&dev, "hello").await;
    h.app.wait_turn(&next.to_string(), &["completed"]).await;
    let (provider, used): (String, String) =
        sqlx::query_as("SELECT provider_id, profile FROM turns WHERE id = ?1")
            .bind(next)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(provider, "codex");
    let used: Value = serde_json::from_str(&used).unwrap();
    assert_eq!(used["model"], "gpt-6.1-sol");
    assert_eq!(used["reasoning_effort"], "low");
}
