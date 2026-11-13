use crate::common;

use std::time::Duration;

use axum::http::StatusCode;
use chrono::{DateTime, SubsecRound, Utc};
use common::recorder::{self, GATE, GATE_EVERY_ATTEMPT, Harness, tool};
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use uuid::Uuid;

const SLOW: &str = "[synthetic:slow] set up my morning digest routine, please.";
const YEARLY: &str = "0 9 1 1 *";

async fn harness() -> Harness {
    recorder::harness(4).await
}

impl Harness {
    async fn owner_turn(&self, bot: &str) -> (Uuid, String) {
        self.running(bot, SLOW).await
    }
}

fn digest(prompt: &str) -> Value {
    json!({ "name": "Morning digest", "prompt": prompt, "cron": YEARLY })
}

async fn create(app: &TestApp, token: &str, args: Value) -> Value {
    let called = tool(app, token, "routine_create", args).await;
    assert!(!called.error, "{}", called.body);
    called.result["routine"].clone()
}

fn at(value: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value.as_str().unwrap())
        .unwrap()
        .to_utc()
}

async fn fire(app: &TestApp, now: DateTime<Utc>) -> usize {
    let queued = else_api::db::routines::admit_due(&app.db.pool, app.scheduler.registry(), now, 8)
        .await
        .unwrap();
    app.scheduler.wake();
    queued
}

async fn runs(app: &TestApp, routine: &str) -> Vec<(Option<Uuid>, Option<String>, String)> {
    sqlx::query_as(
        "SELECT turn_id, skip_reason, name FROM routine_runs WHERE routine_id = ?1 ORDER BY due_at",
    )
    .bind(routine.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

async fn owner_routines(app: &TestApp, bot: &str) -> Vec<Value> {
    let reply = app
        .owner("GET", &format!("/v1/bots/{bot}/routines"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body["routines"].as_array().unwrap().clone()
}

async fn set_enabled(app: &TestApp, bot: &str, routine: &str, enabled: bool) -> common::Reply {
    app.owner(
        "PATCH",
        &format!("/v1/bots/{bot}/routines/{routine}"),
        Some(json!({ "enabled": enabled })),
    )
    .await
}

async fn events(app: &TestApp, bot: &str) -> Vec<(String, Value)> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, data FROM bot_events WHERE bot_id = ?1 ORDER BY seq")
            .bind(bot.parse::<Uuid>().unwrap())
            .fetch_all(&app.db.pool)
            .await
            .unwrap();
    rows.into_iter()
        .map(|(kind, data)| (kind, serde_json::from_str(&data).unwrap()))
        .collect()
}

#[tokio::test]
async fn bot_edits_leave_one_ordered_notice_in_live_and_paged_history() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE).await;
    let routine = create(&h.app, &token, digest("Summarize my unread mail.")).await;
    let patch = json!({ "routine_id": routine["id"], "name": "Daily digest", "cron": "0 8 * * *" });
    let edited = tool(&h.app, &token, "routine_update", patch.clone()).await;
    assert!(!edited.error, "{}", edited.body);
    let retry = tool(&h.app, &token, "routine_update", patch).await;
    assert!(!retry.error);
    let rejected = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": routine["id"], "cron": "invalid" }),
    )
    .await;
    assert!(rejected.error);
    assert_eq!(
        set_enabled(&h.app, &bot, routine["id"].as_str().unwrap(), false)
            .await
            .status,
        StatusCode::OK
    );

    let notices: Vec<_> = events(&h.app, &bot)
        .await
        .into_iter()
        .filter(|(kind, data)| kind == "item.upsert" && data["item"]["routine_update"].is_object())
        .map(|(_, data)| data["item"].clone())
        .collect();
    assert_eq!(notices.len(), 1, "only an actual bot edit leaves a notice");
    let notice = &notices[0];
    assert_eq!(notice["kind"], "status");
    assert_eq!(notice["turn_id"], turn.to_string());
    assert_eq!(
        notice["routine_update"],
        json!({ "routine_id": routine["id"], "name": "Daily digest" })
    );
    assert!(notice["created_at"].is_string());
    assert!(notice["bot_task"].is_null());

    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    assert_eq!(snapshot.status, StatusCode::OK);
    let items = snapshot.body["items"].as_array().unwrap();
    assert_eq!(items.last(), Some(notice));
    assert!(items[items.len() - 2]["seq"].as_i64().unwrap() < notice["seq"].as_i64().unwrap());

    let renamed = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": routine["id"], "name": "Weekly digest" }),
    )
    .await;
    assert!(!renamed.error);
    let before = notice["seq"].as_i64().unwrap() + 1;
    let page = h
        .app
        .owner(
            "GET",
            &format!("/v1/bots/{bot}/conversation?limit=1&before_seq={before}"),
            None,
        )
        .await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.body["items"],
        json!([notice]),
        "later renames preserve history"
    );
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
    let (next, _) = h.running(&bot, GATE).await;
    let inputs = h.inputs(next).await;
    let history = &inputs[0].history;
    assert!(history.iter().any(|entry| entry.kind == "user"));
    assert!(
        history
            .iter()
            .all(|entry| !entry.text.starts_with("Updated routine:")),
        "UI notices never enter rebuilt provider history as delegated tasks"
    );
    h.release(next).await;
}

#[tokio::test]
async fn new_routines_default_to_working_hours_in_the_desktop_time_zone() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let sent = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/turns"),
            Some(json!({ "request_id": Uuid::new_v4(), "text": SLOW,
                         "timezone": "America/New_York" })),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    let turn: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let (_, token) = h.started(turn).await;

    let called = tool(
        &h.app,
        &token,
        "routine_create",
        json!({ "name": "Inbox", "prompt": "Triage my inbox.", "cron": "*/30 * * * *" }),
    )
    .await;
    assert!(!called.error, "{}", called.body);
    assert!(
        called.result["next"]
            .as_str()
            .unwrap()
            .contains("Monday to Friday, 08:00 to 18:00"),
        "the bot is told to mention the default hours"
    );
    let routine = &called.result["routine"];
    assert_eq!(routine["timezone"], "America/New_York");
    assert_eq!(
        routine["active_hours"],
        json!({ "days": ["mon", "tue", "wed", "thu", "fri"], "start": "08:00", "end": "18:00" })
    );
    let zone: chrono_tz::Tz = "America/New_York".parse().unwrap();
    let local = at(&routine["next_run_at"]).with_timezone(&zone);
    use chrono::{Datelike, Timelike};
    assert!(local.weekday().num_days_from_monday() < 5, "{local}");
    assert!((8..18).contains(&local.hour()), "{local}");

    let id = routine["id"].clone();

    sqlx::query("UPDATE installation SET owner_timezone = 'Europe/Paris'")
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    let retry = tool(
        &h.app,
        &token,
        "routine_create",
        json!({ "name": "Inbox", "prompt": "Triage my inbox.", "cron": "*/30 * * * *" }),
    )
    .await;
    assert!(!retry.error, "{}", retry.body);
    assert_eq!(
        retry.result["created"], false,
        "a retry survives a new desktop zone"
    );
    assert_eq!(retry.result["routine"]["timezone"], "America/New_York");

    let nested = tool(
        &h.app,
        &token,
        "routine_create",
        json!({ "name": "Nested", "prompt": "x", "cron": "0 9 * * *", "active_hours":
                { "days": ["mon"], "start": "08:00", "end": "18:00", "timezone": "UTC" } }),
    )
    .await;
    assert!(
        nested.error,
        "unknown fields inside active_hours are refused"
    );

    let raw_id = id.as_str().unwrap().to_owned();
    sqlx::query("UPDATE routines SET next_due_at = '2030-01-04T22:59:00.000+00:00' WHERE id = ?1")
        .bind(raw_id.parse::<Uuid>().unwrap())
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(
        fire(&h.app, at(&json!("2030-01-05T17:00:00Z"))).await,
        0,
        "Friday 17:59 caught up on Saturday does not run"
    );
    assert!(runs(&h.app, &raw_id).await.is_empty());
    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(
        at(&listed[0]["next_run_at"]),
        at(&json!("2030-01-07T13:00:00Z")),
        "Monday 08:00 in New York"
    );

    let nights = tool(
        &h.app,
        &token,
        "routine_create",
        json!({ "name": "Backup", "prompt": "Check the backup.", "cron": "0 23 * * *" }),
    )
    .await;
    assert!(nights.error, "a night-only schedule needs explicit hours");
    assert!(
        nights.result.as_str().unwrap().contains("active_hours"),
        "{}",
        nights.body
    );

    let always = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": id, "active_hours": "always" }),
    )
    .await;
    assert!(!always.error, "{}", always.body);
    assert_eq!(always.result["routine"]["active_hours"], "always");
    let again = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": id, "active_hours": "always" }),
    )
    .await;
    assert!(!again.error, "{}", again.body);
    assert_eq!(
        again.result["routine"]["active_hours"], "always",
        "a repeat reads back the hours it kept"
    );
    assert_eq!(
        again.result["routine"]["updated_at"],
        always.result["routine"]["updated_at"]
    );
    let listed = tool(&h.app, &token, "routine_list", json!({})).await;
    assert_eq!(listed.result["routines"][0]["active_hours"], "always");
    assert_eq!(
        owner_routines(&h.app, &main).await[0]["active_hours"],
        Value::Null,
        "the desktop still reads any time as null"
    );
    let weekend = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": id, "active_hours":
                { "days": ["sat", "sun"], "start": "10:00", "end": "12:00" } }),
    )
    .await;
    assert!(!weekend.error, "{}", weekend.body);
    let local = at(&weekend.result["routine"]["next_run_at"]).with_timezone(&zone);
    assert!(local.weekday().num_days_from_monday() >= 5, "{local}");
    assert!((10..12).contains(&local.hour()), "{local}");
}

#[tokio::test]
async fn a_bot_manages_its_own_routines_over_mcp() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (_, token) = h.owner_turn(&main).await;

    let routine = create(&h.app, &token, digest("Summarize my unread mail.")).await;
    let id = routine["id"].as_str().unwrap().to_owned();
    assert_eq!(routine["timezone"], "UTC", "UTC is the explicit default");
    assert_eq!(routine["cron"], YEARLY);
    assert_eq!(routine["enabled"], true);
    assert_eq!(routine["last_run"], Value::Null);
    let next = at(&routine["next_run_at"]);
    assert!(next > Utc::now());
    assert_eq!(next.format("%m-%d %H:%M").to_string(), "01-01 09:00");

    let retry = tool(
        &h.app,
        &token,
        "routine_create",
        digest("Summarize my unread mail."),
    )
    .await;
    assert_eq!(retry.result["created"], false);
    assert_eq!(retry.result["routine"]["id"], id.as_str());
    let changed = tool(&h.app, &token, "routine_create", digest("Something else.")).await;
    assert!(changed.error);
    assert!(
        changed
            .result
            .as_str()
            .unwrap()
            .starts_with("CREATE_CONFLICT")
    );

    for (args, code) in [
        (
            json!({ "name": "Bad", "prompt": "x", "cron": "* * *" }),
            "INVALID_ROUTINE",
        ),
        (
            json!({ "name": "Never", "prompt": "x", "cron": "0 0 30 2 *" }),
            "INVALID_ROUTINE",
        ),
        (
            json!({ "name": "Zone", "prompt": "x", "cron": YEARLY, "timezone": "Mars/Base" }),
            "INVALID_ROUTINE",
        ),
        (
            json!({ "name": "Other", "prompt": "x", "cron": YEARLY, "bot_id": Uuid::new_v4() }),
            "INVALID_ARGUMENTS",
        ),
    ] {
        let called = tool(&h.app, &token, "routine_create", args).await;
        assert!(called.error, "{}", called.body);
        assert!(
            called.result.as_str().unwrap().starts_with(code),
            "{}",
            called.body
        );
    }

    let paris = create(
        &h.app,
        &token,
        json!({ "name": "Paris", "prompt": "Check the agenda.", "cron": "30 7 * * 1-5",
                "timezone": "Europe/Paris", "active_hours": "always" }),
    )
    .await;
    assert_eq!(paris["timezone"], "Europe/Paris");

    let listed = tool(&h.app, &token, "routine_list", json!({})).await;
    let names: Vec<&str> = listed.result["routines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Morning digest", "Paris"]);
    let found = tool(
        &h.app,
        &token,
        "find_tool",
        json!({ "query": "routine_list" }),
    )
    .await;
    assert_eq!(found.result["tools"][0]["name"], "routine_list");
    let used = tool(
        &h.app,
        &token,
        "use_tool",
        json!({ "name": "routine_list", "arguments": {} }),
    )
    .await;
    assert_eq!(
        used.result, listed.result,
        "use_tool runs the tool as called directly"
    );
    let parking = tool(
        &h.app,
        &token,
        "use_tool",
        json!({ "name": "ask_owner", "arguments": {} }),
    )
    .await;
    assert!(
        parking.error,
        "a listed tool is called directly: {}",
        parking.body
    );
    let undeclared = tool(
        &h.app,
        &token,
        "use_tool",
        json!({ "name": "computer_job_cancel",
                "arguments": { "job_id": "j", "effect": "destructive" } }),
    )
    .await;
    assert!(undeclared.error, "{}", undeclared.body);
    assert_ne!(
        undeclared.result["parked"], true,
        "a deferred tool never parks"
    );
    assert_eq!(owner_routines(&h.app, &main).await.len(), 2);

    let updated = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": id, "name": "Digest", "cron": "0 8 * * *", "timezone": "Asia/Tokyo" }),
    )
    .await;
    assert!(!updated.error, "{}", updated.body);
    assert_eq!(updated.result["routine"]["name"], "Digest");
    let next = at(&updated.result["routine"]["next_run_at"]);
    assert_eq!(next.format("%H:%M").to_string(), "23:00", "08:00 in Tokyo");
    let taken = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": paris["id"], "name": "Digest" }),
    )
    .await;
    assert!(taken.result.as_str().unwrap().starts_with("NAME_TAKEN"));

    let other = h.app.create_bot("Work").await;
    let (_, other_token) = h.owner_turn(&other).await;
    let theirs = tool(&h.app, &other_token, "routine_list", json!({})).await;
    assert_eq!(theirs.result["routines"], json!([]));
    for (name, args) in [
        (
            "routine_update",
            json!({ "routine_id": id, "enabled": false }),
        ),
        ("routine_delete", json!({ "routine_id": id })),
    ] {
        let called = tool(&h.app, &other_token, name, args).await;
        assert!(
            called.result.as_str().unwrap().starts_with("NOT_FOUND"),
            "{name}"
        );
    }
    let foreign = set_enabled(&h.app, &other, &id, false).await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND);

    let deleted = tool(
        &h.app,
        &token,
        "routine_delete",
        json!({ "routine_id": id }),
    )
    .await;
    assert_eq!(deleted.result["already_deleted"], false);
    let again = tool(
        &h.app,
        &token,
        "routine_delete",
        json!({ "routine_id": id }),
    )
    .await;
    assert_eq!(again.result["already_deleted"], true);
    let left = owner_routines(&h.app, &main).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0]["name"], "Paris");
    create(
        &h.app,
        &token,
        json!({ "name": "Digest", "prompt": "New.", "cron": YEARLY }),
    )
    .await;
}

#[tokio::test]
async fn routines_per_bot_are_capped() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (_, token) = h.owner_turn(&main).await;
    for n in 0..else_api::app::routines::MAX_PER_BOT {
        create(
            &h.app,
            &token,
            json!({ "name": format!("R{n}"), "prompt": "x", "cron": YEARLY }),
        )
        .await;
    }
    let over = tool(
        &h.app,
        &token,
        "routine_create",
        json!({ "name": "One more", "prompt": "x", "cron": YEARLY }),
    )
    .await;
    assert!(over.result.as_str().unwrap().starts_with("LIMIT_REACHED"));
}

async fn child_of(app: &TestApp, root: Uuid) -> Uuid {
    sqlx::query_scalar(
        "SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1 ORDER BY created_at DESC",
    )
    .bind(root)
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

async fn owner_conversation(app: &TestApp, bot: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM conversations WHERE bot_id = ?1 AND kind = 'owner'")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn forged(app: &TestApp, turn: Uuid, tools: Vec<Tool>) -> String {
    let (bot, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    app.scheduler
        .grants()
        .issue(bot, conversation, turn, tools, DEFAULT_TTL)
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_bot_takes_over_a_routine_without_the_owner_repeating_it() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let args = json!({ "name": "Morning digest", "prompt": "Summarize the owner's unread mail.",
                       "cron": "30 7 * * 1-5", "timezone": "Europe/Paris",
                       "active_hours": "always" });
    let (setup, token) = h.running(&main, GATE).await;
    let old = create(&h.app, &token, args.clone()).await;
    h.release(setup).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;

    let (root, token) = h
        .running(
            &main,
            &format!("{GATE_EVERY_ATTEMPT} make a Mail bot and move my morning digest to it"),
        )
        .await;
    let created = tool(&h.app, &token, "create_bot", json!({ "name": "Mail" })).await;
    assert!(!created.error, "{}", created.body);
    let mail = created.result["bot_id"].as_str().unwrap().to_owned();
    let handoff = "[gate] Save this as your own routine named Morning digest: summarize the \
                   owner's unread mail, at 07:30 on weekdays, Europe/Paris time.";
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": mail, "task": handoff }),
    )
    .await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let child = child_of(&h.app, root).await;
    h.release(root).await;
    h.app.wait_turn(&root.to_string(), &["waiting"]).await;

    let (_, child_token) = h.started(child).await;
    let saved = create(&h.app, &child_token, args.clone()).await;
    let again = tool(&h.app, &child_token, "routine_create", args).await;
    assert_eq!(again.result["created"], false, "{}", again.body);
    assert_eq!(again.result["routine"]["id"], saved["id"]);
    let (owner, conversation, creator): (Uuid, Uuid, Uuid) = sqlx::query_as(
        "SELECT bot_id, conversation_id, created_turn_id FROM routines WHERE id = ?1",
    )
    .bind(saved["id"].as_str().unwrap().parse::<Uuid>().unwrap())
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    assert_eq!(owner.to_string(), mail, "the routine is the new bot's own");
    assert_eq!(conversation, owner_conversation(&h.app, &mail).await);
    assert_eq!(creator, child);
    let edited = tool(
        &h.app,
        &child_token,
        "routine_update",
        json!({ "routine_id": saved["id"], "prompt": "Summarize unread mail and highlight urgent messages." }),
    )
    .await;
    assert!(!edited.error, "{}", edited.body);
    let history = h
        .app
        .owner("GET", &format!("/v1/bots/{mail}/conversation"), None)
        .await;
    let notice = history.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["routine_update"].is_object())
        .unwrap();
    assert_eq!(notice["routine_update"]["routine_id"], saved["id"]);
    assert!(notice["bot_task"].is_null());
    assert!(notice["bot_reply"].is_null());
    let foreign = tool(
        &h.app,
        &child_token,
        "routine_update",
        json!({ "routine_id": old["id"], "enabled": false }),
    )
    .await;
    assert_eq!(foreign.code(), "NOT_FOUND", "{}", foreign.body);
    let delegated = tool(
        &h.app,
        &child_token,
        "delegate",
        json!({ "target_bot_id": main, "task": "loop back" }),
    )
    .await;
    assert!(delegated.error, "{}", delegated.body);
    assert!(
        delegated.result["error"]
            .as_str()
            .is_some_and(|e| e.starts_with("NOT_ALLOWED")),
        "{}",
        delegated.body
    );
    let created = tool(
        &h.app,
        &child_token,
        "create_bot",
        json!({ "name": "Another" }),
    )
    .await;
    assert_eq!(created.status, StatusCode::FORBIDDEN, "{}", created.body);
    assert_eq!(
        else_api::db::delegations::create(&h.app.db.pool, child, main.parse().unwrap(), "x", &[])
            .await,
        Err(else_api::db::delegations::DelegateError::NotAllowed),
        "one hop, even with a forged grant"
    );
    h.release(child).await;
    let (_, continuation_token) = h.started(root).await;

    let attempts = h.inputs(root).await;
    assert_eq!(attempts.len(), 2);
    let results = else_api::providers::prompt::outcomes_block(&attempts[1]);
    assert!(results.contains("bot=\"Mail\""), "{results}");
    assert!(
        results.contains("Europe/Paris"),
        "the new bot's reply: {results}"
    );
    let task = &h.inputs(child).await[0];
    let rules = else_api::providers::prompt::instructions(task, "/p");
    let main_name = &attempts[0].bot_name;
    assert!(
        rules.contains(&format!(
            "delegated to you by the bot {main_name} (bot_id {main})"
        )),
        "{rules}"
    );
    assert!(rules.contains("you may set up, change, or remove your OWN routines"));
    assert!(rules.contains("scheduled routine task in your conversation with the owner"));
    assert_eq!(
        owner_routines(&h.app, &main).await[0]["enabled"],
        true,
        "the old routine is the main bot's to retire"
    );
    let retired = tool(
        &h.app,
        &continuation_token,
        "routine_update",
        json!({ "routine_id": old["id"], "enabled": false }),
    )
    .await;
    assert!(!retired.error, "{}", retired.body);
    assert_eq!(retired.result["routine"]["enabled"], false);
    h.release(root).await;
    h.app.wait_turn(&root.to_string(), &["completed"]).await;
    assert_eq!(owner_routines(&h.app, &main).await[0]["enabled"], false);
    let (status, delivered, result): (String, Option<i32>, Option<String>) = sqlx::query_as(
        "SELECT status, delivered_attempt, result FROM delegations WHERE child_turn_id = ?1",
    )
    .bind(child)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    assert_eq!((status.as_str(), delivered), ("completed", Some(2)));
    assert!(result.unwrap().contains("Morning digest"));

    let listed = owner_routines(&h.app, &mail).await;
    assert_eq!(listed.len(), 1);
    let due = at(&listed[0]["next_run_at"]);
    assert_eq!(fire(&h.app, due).await, 1);
    let run = runs(&h.app, saved["id"].as_str().unwrap()).await[0]
        .0
        .unwrap();
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    let (run_bot, run_conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(run)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(run_bot.to_string(), mail);
    assert_eq!(run_conversation, owner_conversation(&h.app, &mail).await);
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{mail}/conversation"), None)
        .await
        .body;
    assert!(
        snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["turn_id"] == run.to_string().as_str() && i["kind"] == "assistant"),
        "the run reports in the new bot's chat with the owner"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn routine_runs_and_their_delegated_tasks_cannot_manage_routines() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let leaf = h.app.create_bot("School").await;
    let (setup, token) = h.running(&main, GATE).await;
    let routine = create(
        &h.app,
        &token,
        digest("[gate] gather the morning news digest now."),
    )
    .await;
    h.release(setup).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    assert_eq!(fire(&h.app, at(&routine["next_run_at"])).await, 1);
    let run = runs(&h.app, routine["id"].as_str().unwrap()).await[0]
        .0
        .unwrap();
    let (_, run_token) = h.started(run).await;

    let (owner_inputs, run_inputs) = tokio::join!(h.inputs(setup), h.inputs(run));
    let owner_registry = else_api::providers::prompt::registry_text(&owner_inputs[0]);
    let run_registry = else_api::providers::prompt::registry_text(&run_inputs[0]);
    assert_ne!(
        owner_registry, run_registry,
        "routine permissions affect session compatibility"
    );
    let run_rules = else_api::providers::prompt::instructions(&run_inputs[0], "/p");
    assert!(!run_rules.contains("save it as a routine with routine_create"));
    assert!(!run_rules.contains("To hand a routine to another bot"));

    let refused = tool(&h.app, &run_token, "routine_create", digest("again")).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    let called = tool(
        &h.app,
        &forged(&h.app, run, vec![Tool::Routines]).await,
        "routine_delete",
        json!({ "routine_id": routine["id"] }),
    )
    .await;
    assert_eq!(called.code(), "NOT_ALLOWED", "{}", called.body);
    let memory = tool(&h.app, &run_token, "memory_bots", json!({})).await;
    assert_eq!(memory.status, StatusCode::OK);
    assert!(!memory.error);

    let pending = tool(
        &h.app,
        &run_token,
        "delegate",
        json!({ "target_bot_id": leaf, "task": "[gate] fill in the school form, carefully." }),
    )
    .await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let child = child_of(&h.app, run).await;
    h.release(run).await;
    let (_, child_token) = h.started(child).await;
    let refused = tool(&h.app, &child_token, "routine_list", json!({})).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    let bridged = tool(
        &h.app,
        &child_token,
        "use_tool",
        json!({ "name": "routine_list", "arguments": {} }),
    )
    .await;
    assert_eq!(
        bridged.status,
        StatusCode::FORBIDDEN,
        "use_tool keeps the grant check"
    );
    let rules = else_api::providers::prompt::instructions(&h.inputs(child).await[0], "/p");
    assert!(
        rules.contains("the routine tools are refused for you"),
        "{rules}"
    );
    assert!(!rules.contains("save it as a routine with routine_create"));
    let called = tool(
        &h.app,
        &forged(&h.app, child, vec![Tool::Routines]).await,
        "routine_create",
        digest("x"),
    )
    .await;
    assert_eq!(called.code(), "NOT_ALLOWED", "{}", called.body);
    h.release(child).await;
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    assert!(owner_routines(&h.app, &leaf).await.is_empty());
}

#[tokio::test]
async fn a_bot_runs_a_paused_routine_on_demand_without_moving_its_schedule() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.running(&main, GATE).await;
    const TASK: &str = "[gate] update the tickets after the deployment.";
    let routine = create(&h.app, &token, digest(TASK)).await;
    let id = routine["id"].as_str().unwrap().to_owned();
    assert_eq!(
        set_enabled(&h.app, &main, &id, false).await.status,
        StatusCode::OK
    );
    let enabled = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": id, "enabled": true }),
    )
    .await;
    let next_run_at = enabled.result["routine"]["next_run_at"].clone();
    assert!(next_run_at.is_string(), "{}", enabled.body);

    let ran = tool(&h.app, &token, "routine_run", json!({ "routine_id": id })).await;
    assert!(!ran.error, "{}", ran.body);
    assert_eq!(ran.result["queued"], true);
    assert_eq!(ran.result["routine"]["last_run"]["status"], "queued");
    assert_eq!(ran.result["routine"]["next_run_at"], next_run_at);
    let retry = tool(&h.app, &token, "routine_run", json!({ "routine_id": id })).await;
    assert!(!retry.error, "{}", retry.body);
    assert_eq!(
        retry.result["queued"], false,
        "a retry returns the same run"
    );
    let missing = tool(
        &h.app,
        &token,
        "routine_run",
        json!({ "routine_id": Uuid::new_v4() }),
    )
    .await;
    assert_eq!(missing.code(), "NOT_FOUND", "{}", missing.body);
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 1);
    let run = all[0].0.unwrap();

    h.release(setup).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    let (_, run_token) = h.started(run).await;
    let packet = h.packets().await.into_iter().last().unwrap();
    assert!(packet.contains(TASK), "{packet}");
    let refused = tool(
        &h.app,
        &run_token,
        "routine_run",
        json!({ "routine_id": id }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    h.release(run).await;
    h.app.wait_turn(&run.to_string(), &["completed"]).await;

    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(listed[0]["last_run"]["status"], "completed");
    assert_eq!(listed[0]["last_run"]["turn_id"], run.to_string().as_str());
    assert_eq!(listed[0]["next_run_at"], next_run_at);
    assert_eq!(listed[0]["enabled"], true);
    for _ in 0..200 {
        let reviewed: bool =
            sqlx::query_scalar("SELECT reviewed FROM routine_runs WHERE turn_id = ?1")
                .bind(run)
                .fetch_one(&h.app.db.pool)
                .await
                .unwrap();
        if reviewed {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(
        set_enabled(&h.app, &main, &id, false).await.status,
        StatusCode::OK
    );
    let (again, token) = h.running(&main, GATE).await;
    let ran = tool(&h.app, &token, "routine_run", json!({ "routine_id": id })).await;
    assert!(!ran.error, "{}", ran.body);
    assert_eq!(ran.result["queued"], true, "a new request runs it again");
    assert_eq!(ran.result["routine"]["enabled"], false);
    assert!(ran.result["routine"]["next_run_at"].is_null());
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 2);
    assert_eq!(
        ran.result["routine"]["last_run"]["turn_id"],
        all[1].0.unwrap().to_string().as_str()
    );
    h.release(again).await;
    let paused_run = all[1].0.unwrap();
    h.started(paused_run).await;
    h.release(paused_run).await;
    h.app
        .wait_turn(&paused_run.to_string(), &["completed"])
        .await;
    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(listed[0]["last_run"]["status"], "completed");
    assert_eq!(
        listed[0]["enabled"], false,
        "a run on demand never resumes it"
    );
    assert!(listed[0]["next_run_at"].is_null());
}

#[tokio::test]
async fn a_run_is_hidden_and_its_report_is_visible() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.owner_turn(&main).await;
    const TASK: &str = "Collect the overnight build results and list failures.";
    let routine = create(&h.app, &token, digest(TASK)).await;
    let id = routine["id"].as_str().unwrap().to_owned();
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    let selected: Option<String> =
        sqlx::query_scalar("SELECT selected_provider_id FROM bots WHERE id = ?1")
            .bind(main.parse::<Uuid>().unwrap())
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    let owner_entries = owner_memory(&h.app, &main).await;

    let due = at(&routine["next_run_at"]);
    assert_eq!(
        fire(&h.app, due - chrono::Duration::seconds(1)).await,
        0,
        "not due yet"
    );
    assert_eq!(fire(&h.app, due).await, 1);
    assert_eq!(fire(&h.app, due).await, 0, "one occurrence runs once");
    let run = runs(&h.app, &id).await;
    assert_eq!(run.len(), 1);
    let turn = run[0].0.unwrap();
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;

    let packet = h.packets().await.into_iter().last().unwrap();
    assert!(
        packet.contains("[else.bot routine] Scheduled run of the routine \"Morning digest\""),
        "{packet}"
    );
    assert!(packet.contains(TASK));
    assert!(!packet.contains("Owner's latest message"));

    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await
        .body;
    let items = snapshot["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["kind"] != "routine"));
    let report: Vec<&Value> = items
        .iter()
        .filter(|i| i["turn_id"] == turn.to_string().as_str())
        .collect();
    assert_eq!(report.len(), 1);
    assert_eq!(report[0]["kind"], "assistant");
    assert!(report[0]["text"].as_str().unwrap().contains(TASK));
    let visible_turn = snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .unwrap();
    assert_eq!(visible_turn["routine"]["routine_id"], routine["id"]);
    assert_eq!(visible_turn["routine"]["name"], "Morning digest");
    assert_eq!(at(&visible_turn["routine"]["due_at"]), due);
    assert_eq!(snapshot["routines"][0]["last_run"]["status"], "completed");
    assert_eq!(
        snapshot["routines"][0]["last_run"]["turn_id"],
        turn.to_string().as_str()
    );
    let next = at(&snapshot["routines"][0]["next_run_at"]);
    assert!(next > due, "advanced past the run");

    let journal = events(&h.app, &main).await;
    let run_events: Vec<_> = journal
        .iter()
        .filter(|(kind, data)| kind == "turn.updated" && data["turn"]["id"] == turn.to_string())
        .collect();
    assert!(!run_events.is_empty());
    assert!(
        run_events
            .iter()
            .all(|(_, data)| data["turn"]["routine"] == visible_turn["routine"])
    );
    for (kind, data) in &journal {
        if kind == "item.upsert" {
            assert_ne!(data["item"]["kind"], "routine");
            assert!(!data["item"]["text"].as_str().unwrap().starts_with(TASK));
        }
    }
    let last = journal
        .iter()
        .rev()
        .find(|(kind, _)| kind == "bot.updated")
        .unwrap();
    assert_eq!(last.1["routines"][0]["id"], id.as_str());
    assert_eq!(last.1["routines"][0]["last_run"]["status"], "completed");

    let (_, owner_token) = h.owner_turn(&main).await;
    let history = h
        .app
        .call(
            "GET",
            "/tools/v1/history?limit=100",
            None,
            Some(&owner_token),
        )
        .await;
    assert!(
        history.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] != "routine")
    );
    let hidden: Uuid = sqlx::query_scalar("SELECT owner_item_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    let item = h
        .app
        .call(
            "GET",
            &format!("/tools/v1/items/{hidden}"),
            None,
            Some(&owner_token),
        )
        .await;
    assert_eq!(item.status, StatusCode::NOT_FOUND);

    assert_eq!(
        owner_memory(&h.app, &main).await,
        owner_entries + 1,
        "only the new owner message"
    );
    let logged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_entry WHERE body LIKE '%' || ?1 || '%' AND authority = 'owner'",
    )
    .bind(TASK)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    assert_eq!(logged, 0);
    let after: Option<String> =
        sqlx::query_scalar("SELECT selected_provider_id FROM bots WHERE id = ?1")
            .bind(main.parse::<Uuid>().unwrap())
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(after, selected);
}

async fn owner_memory(app: &TestApp, bot: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM memory_entry WHERE bot_id = ?1 AND authority = 'owner'",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn rebuilt_history_labels_a_run_as_a_routine() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.owner_turn(&main).await;
    let routine = create(&h.app, &token, digest("Water the plants reminder.")).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    fire(&h.app, at(&routine["next_run_at"])).await;
    let run = runs(&h.app, routine["id"].as_str().unwrap()).await[0]
        .0
        .unwrap();
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    let sessions: Vec<String> =
        sqlx::query_scalar("SELECT native_session_id FROM provider_sessions")
            .fetch_all(&h.app.db.pool)
            .await
            .unwrap();
    h.recorder.inner.forget_sessions(&sessions).await;
    let sent = h.app.send(&main, Uuid::new_v4(), "Did it run?", None).await;
    h.app
        .wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let packet = h.packets().await.into_iter().last().unwrap();
    assert!(
        packet.contains(
            "Routine or webhook wake (a saved task, not an owner message): Water the plants reminder."
        ),
        "{packet}"
    );
    assert!(!packet.contains("Owner: Water the plants"));
    assert!(packet.ends_with("Owner's latest message:\nDid it run?"));
}

async fn wait_routine(app: &TestApp, bot: &str, done: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..400 {
        let listed = owner_routines(app, bot).await;
        if done(&listed[0]) {
            return listed[0].clone();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "routine never reached the state: {}",
        owner_routines(app, bot).await[0]
    );
}

async fn replies(app: &TestApp, turn: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT text FROM items WHERE turn_id = ?1 AND kind = 'assistant' ORDER BY seq",
    )
    .bind(turn)
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

async fn sidebar(app: &TestApp, bot: &str) -> Value {
    app.owner("GET", &format!("/v1/bots/{bot}"), None)
        .await
        .body["bot"]["sidebar_status"]
        .clone()
}

async fn failing_routine(h: &Harness, bot: &str, prompt: &str) -> (String, DateTime<Utc>) {
    let (setup, token) = h.owner_turn(bot).await;
    let routine = create(
        &h.app,
        &token,
        json!({ "name": "Tickets", "prompt": prompt, "cron": "0 * * * *", "active_hours": "always" }),
    )
    .await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    (
        routine["id"].as_str().unwrap().to_owned(),
        at(&routine["next_run_at"]),
    )
}

async fn set_failed_runs(app: &TestApp, routine: &str, failed_runs: i64) {
    sqlx::query("UPDATE routines SET failed_runs = ?2 WHERE id = ?1")
        .bind(routine.parse::<Uuid>().unwrap())
        .bind(failed_runs)
        .execute(&app.db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_transient_failure_is_retried_once_then_reported_until_dismissed() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:fail] update the tickets").await;
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["failed_runs"] == 1).await;
    assert_eq!(routine["enabled"], true);
    assert_eq!(routine["paused_reason"], Value::Null);
    assert_eq!(
        at(&routine["next_run_at"]),
        due + chrono::Duration::hours(1)
    );
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 2, "one automatic retry");
    let (first, retry) = (all[0].0.unwrap(), all[1].0.unwrap());
    let retry_of: Option<Uuid> =
        sqlx::query_scalar("SELECT retry_of FROM routine_runs WHERE turn_id = ?1")
            .bind(retry)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    let first_run: Uuid = sqlx::query_scalar("SELECT id FROM routine_runs WHERE turn_id = ?1")
        .bind(first)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(retry_of, Some(first_run));
    assert_eq!(routine["last_run"]["turn_id"], retry.to_string());
    assert_eq!(routine["last_run"]["status"], "failed");
    assert_eq!(
        replies(&h.app, first).await,
        ["Partial output before failure. "],
        "the retried failure is not reported"
    );
    let notice = replies(&h.app, retry).await.pop().unwrap();
    assert!(
        notice.starts_with(
            "The routine **Tickets** failed because the AI provider reported an error. I retried \
             it right away and it failed again. I'll try again at the next scheduled run, "
        ),
        "{notice}"
    );
    assert!(notice.ends_with("Error: `provider_failed`: The synthetic provider failed."));

    assert_eq!(sidebar(&h.app, &main).await, json!({ "kind": "failed" }));
    let considered: i64 = sqlx::query_scalar(
        "SELECT max(considered_through_turn) FROM provider_sessions s
         JOIN turns t ON t.conversation_id = s.conversation_id WHERE t.id = ?1",
    )
    .bind(retry)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    let retry_no: i64 = sqlx::query_scalar("SELECT turn_no FROM turns WHERE id = ?1")
        .bind(retry)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert!(considered < retry_no, "warm sessions replay the notice");
    let (partial_seq, partial_bytes): (i64, i64) = sqlx::query_as(
        "SELECT seq, length(CAST(text AS BLOB)) FROM items
         WHERE turn_id = ?1 AND kind = 'assistant' ORDER BY seq LIMIT 1",
    )
    .bind(retry)
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    h.app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/read"),
            Some(json!({ "action": "through",
                         "position": { "item_seq": partial_seq, "text_bytes": partial_bytes } })),
        )
        .await;
    assert_eq!(
        sidebar(&h.app, &main).await,
        json!({ "kind": "failed" }),
        "reading partial output leaves the notice unseen"
    );
    let state = h
        .app
        .owner("GET", &format!("/v1/bots/{main}"), None)
        .await
        .body["bot"]["read_state"]
        .clone();
    assert_eq!(state["unread"], true, "the notice is a new reply");
    let read = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/read"),
            Some(json!({ "action": "through", "position": state["latest"]["position"] })),
        )
        .await;
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(
        sidebar(&h.app, &main).await,
        Value::Null,
        "read failures clear"
    );

    let dismissed = h
        .app
        .owner(
            "PATCH",
            &format!("/v1/bots/{main}/routines/{id}"),
            Some(json!({ "dismiss_failures": true })),
        )
        .await;
    assert_eq!(dismissed.status, StatusCode::OK, "{}", dismissed.body);
    assert_eq!(dismissed.body["routine"]["failed_runs"], 0);
    assert_eq!(dismissed.body["routine"]["enabled"], true);
}

#[tokio::test]
async fn a_failure_covered_by_a_stop_is_reported_but_never_retried() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:fail] update the tickets").await;
    sqlx::query(
        "INSERT INTO request_receipts
             (request_id, operation, bot_id, payload_hash, state, boundary_turn_no)
         SELECT randomblob(16), 'cancel', bot_id, randomblob(32), 'done', last_turn_no + 1
         FROM conversations WHERE bot_id = ?1 AND kind = 'owner'",
    )
    .bind(main.parse::<Uuid>().unwrap())
    .execute(&h.app.db.pool)
    .await
    .unwrap();
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["failed_runs"] == 1).await;
    assert_eq!(routine["enabled"], true);
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 1, "the owner's Stop is not undone by a retry");
    let notice = replies(&h.app, all[0].0.unwrap()).await.pop().unwrap();
    assert!(
        notice.contains("I couldn't retry it right away."),
        "{notice}"
    );
}

#[tokio::test]
async fn a_routine_that_keeps_failing_is_paused_until_turned_back_on() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:fail] update the tickets").await;
    let limit = else_api::app::routine_failures::MAX_FAILED_RUNS;
    set_failed_runs(&h.app, &id, limit - 1).await;
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["paused_reason"] == "repeated").await;
    assert_eq!(routine["enabled"], false);
    assert_eq!(routine["next_run_at"], Value::Null);
    assert_eq!(routine["failed_runs"], limit);
    assert_eq!(runs(&h.app, &id).await.len(), 2, "retried before pausing");
    let retry = runs(&h.app, &id).await[1].0.unwrap();
    let notice = replies(&h.app, retry).await.pop().unwrap();
    assert!(
        notice.starts_with(&format!(
            "I paused the routine **Tickets** after {limit} failed runs in a row. The last one \
             failed because the AI provider reported an error."
        )),
        "{notice}"
    );
    let far = Utc::now() + chrono::Duration::days(3650);
    assert_eq!(fire(&h.app, far).await, 0, "paused routines never run");

    let resumed = set_enabled(&h.app, &main, &id, true).await;
    assert_eq!(resumed.status, StatusCode::OK, "{}", resumed.body);
    assert_eq!(resumed.body["routine"]["failed_runs"], 0);
    assert_eq!(resumed.body["routine"]["paused_reason"], Value::Null);
    assert!(at(&resumed.body["routine"]["next_run_at"]) > Utc::now());
}

#[tokio::test]
async fn a_failure_retrying_cannot_fix_pauses_at_once_and_says_why() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "[synthetic:policy] check the servers").await;
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["paused_reason"] == "needs_action").await;
    assert_eq!(routine["enabled"], false);
    assert_eq!(routine["failed_runs"], 1);
    assert_eq!(routine["last_run"]["status"], "failed");
    assert_eq!(routine["last_run"]["error"]["code"], "policy_violation");
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 1, "never retried");
    let notice = replies(&h.app, all[0].0.unwrap()).await;
    assert_eq!(notice.len(), 1);
    assert!(
        notice[0].starts_with(
            "I paused the routine **Tickets** because it tried to use a tool it is not allowed \
             to use. Retrying won't fix this"
        ),
        "{}",
        notice[0]
    );
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await
        .body;
    assert!(
        snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "assistant" && i["text"] == notice[0].as_str()),
        "the notice is in the transcript"
    );
}

#[tokio::test]
async fn a_successful_run_forgets_earlier_failures() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (id, due) = failing_routine(&h, &main, "Collect the build results.").await;
    set_failed_runs(&h.app, &id, 2).await;
    fire(&h.app, due).await;
    let routine = wait_routine(&h.app, &main, |r| r["failed_runs"] == 0).await;
    assert_eq!(routine["last_run"]["status"], "completed");
    assert_eq!(routine["enabled"], true);
    assert_eq!(runs(&h.app, &id).await.len(), 1);
}

#[tokio::test]
async fn overlapping_occurrences_are_skipped_and_missed_ones_coalesce() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.owner_turn(&main).await;
    let routine = create(
        &h.app,
        &token,
        json!({ "name": "Slow", "prompt": "[synthetic:slow] a long routine report for overlap checks.",
                "cron": "0 * * * *", "active_hours": "always" }),
    )
    .await;
    let id = routine["id"].as_str().unwrap().to_owned();
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    let due = at(&json!("2030-01-01T00:00:00Z"));
    sqlx::query("UPDATE routines SET next_due_at = '2030-01-01T00:00:00.000+00:00' WHERE id = ?1")
        .bind(id.parse::<Uuid>().unwrap())
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    fire(&h.app, due).await;
    let first = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(&first.to_string(), &["running"]).await;

    assert_eq!(fire(&h.app, due + chrono::Duration::hours(1)).await, 0);
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[1], (None, Some("overlap".into()), "Slow".into()));
    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(listed[0]["last_run"]["status"], "skipped");
    assert_eq!(listed[0]["last_run"]["skip_reason"], "overlap");
    h.app.wait_turn(&first.to_string(), &["completed"]).await;

    let (_, _) = h.owner_turn(&main).await;
    for _ in 0..else_api::app::messages::MAX_QUEUED_TURNS {
        let sent = h.app.send(&main, Uuid::new_v4(), "queued", None).await;
        assert_eq!(sent.status, StatusCode::ACCEPTED);
    }
    assert_eq!(fire(&h.app, due + chrono::Duration::hours(2)).await, 1);
    assert_eq!(runs(&h.app, &id).await[2].1, None);
    let stop = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success());
    for _ in 0..200 {
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE status NOT IN ('completed', 'interrupted', 'failed', 'uncertain'))
             OR EXISTS (SELECT 1 FROM bots WHERE cancel_pending)
             OR EXISTS (SELECT 1 FROM routine_runs WHERE turn_id IS NOT NULL AND NOT reviewed)",
        )
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
        if !busy {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let late = due + chrono::Duration::days(10) + chrono::Duration::minutes(7);
    assert_eq!(fire(&h.app, late).await, 1);
    assert_eq!(fire(&h.app, late).await, 0);
    assert_eq!(runs(&h.app, &id).await.len(), 4);
    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(
        at(&listed[0]["next_run_at"]),
        late - chrono::Duration::minutes(7) + chrono::Duration::hours(1)
    );
}

#[tokio::test]
async fn an_owner_message_starts_while_routines_fill_the_other_slots() {
    let h = recorder::harness(2).await;
    let main = h.app.main_bot().await;
    let due = at(&json!("2030-01-01T00:00:00Z"));
    for name in ["Scout", "Auto"] {
        let bot = h.app.create_bot(name).await;
        let (setup, token) = h.owner_turn(&bot).await;
        let routine = create(
            &h.app,
            &token,
            json!({ "name": "Hourly", "prompt": "[synthetic:hang] an hourly sweep that keeps running.",
                    "cron": "0 * * * *", "active_hours": "always" }),
        )
        .await;
        h.app.wait_turn(&setup.to_string(), &["completed"]).await;
        sqlx::query(
            "UPDATE routines SET next_due_at = '2030-01-01T00:00:00.000+00:00' WHERE id = ?1",
        )
        .bind(routine["id"].as_str().unwrap().parse::<Uuid>().unwrap())
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    }
    assert_eq!(fire(&h.app, due).await, 2);
    let first = loop {
        let running: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM turns WHERE status = 'running' AND id IN (SELECT turn_id FROM routine_runs)",
        )
        .fetch_optional(&h.app.db.pool)
        .await
        .unwrap();
        if let Some(turn) = running {
            break turn;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };

    let owner = h.send(&main, "[synthetic] are you there?").await;
    assert_eq!(
        h.app.wait_turn(&owner.to_string(), &["completed"]).await,
        "completed",
        "the owner message ran while routines held the other slot"
    );
    assert_eq!(
        h.app.wait_turn(&first.to_string(), &["running"]).await,
        "running"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pausing_stops_future_runs_and_resuming_counts_from_now() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.owner_turn(&main).await;
    let routine = create(
        &h.app,
        &token,
        digest("[synthetic:slow] a long routine report for pause checks."),
    )
    .await;
    let id = routine["id"].as_str().unwrap().to_owned();
    let original_next = routine["next_run_at"].clone();
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;

    fire(&h.app, at(&original_next)).await;
    let run = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(&run.to_string(), &["running"]).await;
    let paused = set_enabled(&h.app, &main, &id, false).await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);
    assert_eq!(paused.body["routine"]["enabled"], false);
    assert_eq!(paused.body["routine"]["next_run_at"], Value::Null);
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    let far = Utc::now() + chrono::Duration::days(3650);
    assert_eq!(fire(&h.app, far).await, 0, "paused routines never run");

    let before = events(&h.app, &main).await.len();
    set_enabled(&h.app, &main, &id, false).await;
    assert_eq!(events(&h.app, &main).await.len(), before);
    let resumed = set_enabled(&h.app, &main, &id, true).await;
    let next = resumed.body["routine"]["next_run_at"].clone();
    assert!(at(&next) > Utc::now());
    let journal = events(&h.app, &main).await;
    assert_eq!(journal.len(), before + 1);
    assert_eq!(journal[before].0, "bot.updated");
    assert_eq!(journal[before].1["routines"][0]["enabled"], true);
    let again = set_enabled(&h.app, &main, &id, true).await;
    assert_eq!(again.body["routine"]["next_run_at"], next);
    assert_eq!(events(&h.app, &main).await.len(), before + 1);

    let (_, token) = h.owner_turn(&main).await;
    tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": id, "name": "Renamed" }),
    )
    .await;
    assert_eq!(runs(&h.app, &id).await[0].2, "Morning digest");
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await
        .body;
    let admitted = snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == run.to_string())
        .unwrap();
    assert_eq!(admitted["routine"]["name"], "Morning digest");
    let bad = set_enabled(&h.app, &main, &Uuid::new_v4().to_string(), true).await;
    assert_eq!(bad.status, StatusCode::NOT_FOUND);
}

async fn owner_delete(app: &TestApp, bot: &str, routine: &str) -> common::Reply {
    app.owner(
        "DELETE",
        &format!("/v1/bots/{bot}/routines/{routine}"),
        None,
    )
    .await
}

#[tokio::test]
async fn the_owner_deletes_a_routine_and_its_admitted_run_finishes() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let other = h.app.create_bot("Work").await;
    let (setup, token) = h.running(&main, GATE).await;
    let routine = create(
        &h.app,
        &token,
        digest(&format!("{GATE} a routine report for delete checks.")),
    )
    .await;
    let id = routine["id"].as_str().unwrap().to_owned();
    let path = format!("/v1/bots/{main}/routines/{id}");
    let anonymous = h.app.call("DELETE", &path, None, None).await;
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    let granted = h.app.call("DELETE", &path, None, Some(&token)).await;
    assert_eq!(granted.status, StatusCode::FORBIDDEN);
    let wrong_bot = owner_delete(&h.app, &other, &id).await;
    assert_eq!(
        wrong_bot.status,
        StatusCode::NOT_FOUND,
        "{}",
        wrong_bot.body
    );
    let missing = owner_delete(&h.app, &main, &Uuid::new_v4().to_string()).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let unknown_bot = owner_delete(&h.app, &Uuid::new_v4().to_string(), &id).await;
    assert_eq!(unknown_bot.status, StatusCode::NOT_FOUND);
    assert_eq!(owner_routines(&h.app, &main).await.len(), 1);

    h.release(setup).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    fire(&h.app, at(&routine["next_run_at"])).await;
    let run = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(&run.to_string(), &["running"]).await;
    let before = events(&h.app, &main).await.len();
    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(listed[0]["last_run"]["status"], "running");
    let deleted = owner_delete(&h.app, &main, &id).await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(
        deleted.body,
        json!({ "routine_id": id, "deleted": true, "already_deleted": false })
    );
    let journal = events(&h.app, &main).await;
    assert_eq!(journal.len(), before + 1);
    assert_eq!(journal[before].0, "bot.updated");
    assert_eq!(journal[before].1["routines"], json!([]));
    assert!(owner_routines(&h.app, &main).await.is_empty());

    let again = owner_delete(&h.app, &main, &id).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["already_deleted"], true);
    assert_eq!(events(&h.app, &main).await.len(), before + 1);
    let toggled = set_enabled(&h.app, &main, &id, true).await;
    assert_eq!(toggled.status, StatusCode::NOT_FOUND);

    h.release(run).await;
    h.app.wait_turn(&run.to_string(), &["completed"]).await;
    let far = Utc::now() + chrono::Duration::days(3650);
    assert_eq!(fire(&h.app, far).await, 0, "deleted routines never run");
    assert_eq!(runs(&h.app, &id).await.len(), 1);

    let gone = h
        .app
        .owner("DELETE", &format!("/v1/bots/{other}"), None)
        .await;
    assert!(gone.status.is_success());
    let deleted_bot = owner_delete(&h.app, &other, &id).await;
    assert_eq!(deleted_bot.status, StatusCode::GONE);
}

#[tokio::test]
async fn stop_deletion_and_maintenance_hold_runs() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.owner_turn(&main).await;
    let mut args = digest("[synthetic:slow] a long routine report for stop checks.");
    args["active_hours"] = json!("always");
    let routine = create(&h.app, &token, args).await;
    let id = routine["id"].as_str().unwrap().to_owned();
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;

    fire(&h.app, at(&routine["next_run_at"])).await;
    let run = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(&run.to_string(), &["running"]).await;
    let stop = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    h.app.wait_turn(&run.to_string(), &["interrupted"]).await;
    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(listed[0]["last_run"]["status"], "interrupted");
    assert_eq!(listed[0]["enabled"], true);

    let closed = h
        .app
        .scheduler
        .gate()
        .close(
            Duration::from_secs(10),
            else_api::maintenance::Pause::backup(None),
        )
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE routines SET next_due_at = '2026-01-01T00:00:00.000+00:00' WHERE id = ?1")
        .bind(id.parse::<Uuid>().unwrap())
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    h.app.scheduler.wake();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        runs(&h.app, &id).await.len(),
        1,
        "nothing admitted during maintenance"
    );
    drop(closed);
    for _ in 0..200 {
        if runs(&h.app, &id).await.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let after = runs(&h.app, &id).await;
    assert_eq!(after.len(), 2, "admitted once the gate opened");
    let second = after
        .iter()
        .find_map(|r| r.0.filter(|t| *t != run))
        .unwrap();
    h.app.wait_turn(&second.to_string(), &["completed"]).await;

    let other = h.app.create_bot("Work").await;
    let (other_setup, other_token) = h.owner_turn(&other).await;
    let theirs = create(&h.app, &other_token, digest("Weekly report.")).await;
    h.app
        .wait_turn(&other_setup.to_string(), &["completed"])
        .await;
    let gone = h
        .app
        .owner("DELETE", &format!("/v1/bots/{other}"), None)
        .await;
    assert!(gone.status.is_success());
    assert_eq!(fire(&h.app, at(&theirs["next_run_at"])).await, 0);
    assert!(
        runs(&h.app, theirs["id"].as_str().unwrap())
            .await
            .is_empty()
    );
    let listed = h
        .app
        .owner("GET", &format!("/v1/bots/{other}/routines"), None)
        .await;
    assert_eq!(listed.status, StatusCode::GONE);
}

#[tokio::test]
async fn a_restart_never_repeats_an_admitted_run() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.owner_turn(&main).await;
    let routine = create(
        &h.app,
        &token,
        digest("[synthetic:slow] a long routine report for restart checks."),
    )
    .await;
    let id = routine["id"].as_str().unwrap().to_owned();
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    let due = at(&routine["next_run_at"]);
    fire(&h.app, due).await;
    let run = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(&run.to_string(), &["running"]).await;
    let Harness { app, recorder } = h;
    let app = app.restart().await;
    let status = app
        .wait_turn(&run.to_string(), &["uncertain", "completed", "interrupted"])
        .await;
    assert_ne!(status, "queued");
    assert_eq!(
        else_api::db::routines::admit_due(&app.db.pool, app.scheduler.registry(), due, 8)
            .await
            .unwrap(),
        0
    );
    assert_eq!(runs(&app, &id).await.len(), 1);
    drop(recorder);
}

#[tokio::test]
async fn the_routines_migration_preserves_existing_data() {
    let db = TestDb::empty().await;
    let dir = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for name in [
        "0001_application.sql",
        "0002_secret_requests.sql",
        "0003_remove_bot_roles.sql",
    ] {
        std::fs::copy(source.join(name), dir.path().join(name)).unwrap();
    }
    sqlx::migrate::Migrator::new(dir.path())
        .await
        .unwrap()
        .run(&db.pool)
        .await
        .unwrap();
    let main = Uuid::new_v4();
    for sql in [
        "INSERT INTO bots (id, name) VALUES (?1, 'Assistant')",
        "INSERT INTO conversations (id, bot_id) VALUES (randomblob(16), ?1)",
        "INSERT INTO installation (default_bot_id) VALUES (?1)",
    ] {
        sqlx::query(sql).bind(main).execute(&db.pool).await.unwrap();
    }
    let root = Uuid::new_v4();
    let owner_item = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status)
         SELECT ?1, ?2, c.id, ?1, ?3, 1, ?4, 'synthetic', 'waiting'
         FROM conversations c WHERE c.bot_id = ?2 AND c.kind = 'owner'",
    )
    .bind(root)
    .bind(main)
    .bind(vec![1u8; 32])
    .bind(owner_item)
    .execute(&db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         SELECT ?1, conversation_id, 1, id, 'user', 'plan the week' FROM turns WHERE id = ?2",
    )
    .bind(owner_item)
    .bind(root)
    .execute(&db.pool)
    .await
    .unwrap();
    let leaf = Uuid::new_v4();
    let task_conversation = Uuid::new_v4();
    let child = Uuid::new_v4();
    let child_item = Uuid::new_v4();
    let hash = vec![7u8; 32];
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Leaf')")
        .bind(leaf)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id, kind) VALUES (?1, ?2, 'task')")
        .bind(task_conversation)
        .bind(leaf)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status)
         VALUES (?1, ?2, ?3, ?1, ?4, 1, ?5, 'synthetic', 'running')",
    )
    .bind(child)
    .bind(leaf)
    .bind(task_conversation)
    .bind(&hash)
    .bind(child_item)
    .execute(&db.pool)
    .await
    .unwrap();
    for (seq, kind, text) in [(1, "user", "task text"), (2, "assistant", "child result")] {
        sqlx::query(
            "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(if seq == 1 { child_item } else { Uuid::new_v4() })
        .bind(task_conversation)
        .bind(seq)
        .bind(child)
        .bind(kind)
        .bind(text)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO delegations (id, root_turn_id, parent_attempt, parent_bot_id, target_bot_id,
             task, task_hash, conversation_id, child_turn_id, status)
         VALUES (randomblob(16), ?1, 1, ?2, ?3, 'task text', ?4, ?5, ?6, 'running')",
    )
    .bind(root)
    .bind(main)
    .bind(leaf)
    .bind(&hash)
    .bind(task_conversation)
    .bind(child)
    .execute(&db.pool)
    .await
    .unwrap();
    let mut conn = db.pool.acquire().await.unwrap();
    let saved_items =
        "SELECT id, conversation_id, seq, turn_id, kind, text, provider_id, created_at FROM items";
    let before = common::rows(&mut conn, saved_items).await;
    drop(conn);

    else_api::db::migrate(&db.pool).await.unwrap();

    let mut conn = db.pool.acquire().await.unwrap();
    assert_eq!(common::rows(&mut conn, saved_items).await, before);
    drop(conn);
    let avatars: Vec<(Uuid, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, avatar_shape, avatar_color FROM bots ORDER BY id = ?1 DESC")
            .bind(main)
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        avatars,
        [
            (main, Some("star".into()), Some("blue".into())),
            (leaf, None, None)
        ],
        "the main bot becomes the blue star; others keep a derived avatar"
    );
    let index: Option<String> =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'items_turn'")
            .fetch_optional(&db.pool)
            .await
            .unwrap();
    assert!(index.is_some());
    let violations: Vec<(String,)> =
        sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?1")
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
    let (status, result): (String, Option<String>) =
        sqlx::query_as("SELECT status, result FROM delegations WHERE child_turn_id = ?1")
            .bind(child)
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        (status.as_str(), result.as_deref()),
        ("completed", Some("child result"))
    );
    let root_status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
        .bind(root)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(root_status, "queued");
    sqlx::query("INSERT INTO items (id, conversation_id, seq, turn_id, kind, text) VALUES (randomblob(16), ?1, 3, ?2, 'routine', 'p')")
        .bind(task_conversation)
        .bind(child)
        .execute(&db.pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_failures_migration_keeps_a_failed_last_run_visible_and_never_reviews_history() {
    let db = TestDb::empty().await;
    let dir = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let migrate = |dir: &std::path::Path| {
        let dir = dir.to_owned();
        let pool = db.pool.clone();
        async move {
            sqlx::migrate::Migrator::new(dir.as_path())
                .await
                .unwrap()
                .run(&pool)
                .await
                .unwrap();
        }
    };
    let mut names: Vec<_> = std::fs::read_dir(&source)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".sql") && name.as_str() < "0031")
        .collect();
    names.sort();
    for name in &names {
        std::fs::copy(source.join(name), dir.path().join(name)).unwrap();
    }
    migrate(dir.path()).await;
    let bot = Uuid::new_v4();
    for sql in [
        "INSERT INTO bots (id, name) VALUES (?1, 'Assistant')",
        "INSERT INTO conversations (id, bot_id) VALUES (randomblob(16), ?1)",
    ] {
        sqlx::query(sql).bind(bot).execute(&db.pool).await.unwrap();
    }
    for (turn_no, status, routine) in [
        (1, "completed", "Fine"),
        (2, "failed", "Broken"),
        (3, "queued", "Pending"),
    ] {
        let turn = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
                 owner_item_id, provider_id, status)
             SELECT ?1, ?2, c.id, ?1, ?3, ?4, randomblob(16), 'synthetic', ?5
             FROM conversations c WHERE c.bot_id = ?2",
        )
        .bind(turn)
        .bind(bot)
        .bind(vec![1u8; 32])
        .bind(turn_no)
        .bind(status)
        .execute(&db.pool)
        .await
        .unwrap();
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
                 created_turn_id, created_name, request_hash)
             SELECT ?1, ?2, c.id, ?3, 'do it', '0 * * * *', 'UTC', ?4, ?3, ?5
             FROM conversations c WHERE c.bot_id = ?2",
        )
        .bind(id)
        .bind(bot)
        .bind(routine)
        .bind(turn)
        .bind(vec![2u8; 32])
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id)
             VALUES (randomblob(16), ?1, '2026-11-08T03:00:00.000+00:00', ?2, ?3)",
        )
        .bind(id)
        .bind(routine)
        .bind(turn)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    std::fs::copy(
        source.join("0031_routine_failures.sql"),
        dir.path().join("0031_routine_failures.sql"),
    )
    .unwrap();
    migrate(dir.path()).await;
    let failed: Vec<(String, i64)> =
        sqlx::query_as("SELECT name, failed_runs FROM routines ORDER BY name")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        failed,
        [
            ("Broken".into(), 1),
            ("Fine".into(), 0),
            ("Pending".into(), 0)
        ]
    );
    let unreviewed: Vec<String> =
        sqlx::query_scalar("SELECT name FROM routine_runs WHERE NOT reviewed")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    assert_eq!(
        unreviewed,
        ["Pending"],
        "an unfinished run is reviewed when it ends"
    );
}

#[tokio::test]
async fn a_reminder_runs_once_then_leaves_the_list() {
    let h = harness().await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.owner_turn(&main).await;
    let args = json!({ "name": "Scout check", "prompt": "Check how many DMs Scout sent.",
                       "in_minutes": 360 });
    let before = Utc::now();
    let called = tool(&h.app, &token, "reminder_create", args.clone()).await;
    assert!(!called.error, "{}", called.body);
    let reminder = called.result["reminder"].clone();
    let id = reminder["id"].as_str().unwrap().to_owned();
    let run_at = at(&reminder["run_at"]);
    let six_hours = chrono::Duration::minutes(360);
    assert!(run_at >= before.trunc_subsecs(0) + six_hours && run_at <= Utc::now() + six_hours);
    assert_eq!(at(&reminder["next_run_at"]), run_at);
    assert_eq!(reminder["cron"], "once");
    assert!(
        reminder["active_hours"].is_null(),
        "a reminder runs at any hour"
    );

    let again = tool(&h.app, &token, "reminder_create", args).await;
    assert_eq!(
        again.result["reminder"]["id"],
        id.as_str(),
        "a retry returns it"
    );
    assert_eq!(again.result["created"], false);
    assert_eq!(at(&again.result["reminder"]["run_at"]), run_at);

    let paris = chrono_tz::Europe::Paris;
    let local = (Utc::now() + chrono::Duration::days(2)).with_timezone(&paris);
    let nine = local.format("%Y-%m-%dT09:00").to_string();
    let dated = tool(
        &h.app,
        &token,
        "reminder_create",
        json!({ "name": "Paris check", "prompt": "Check the launch.", "at": nine,
                "timezone": "Europe/Paris" }),
    )
    .await;
    assert!(!dated.error, "{}", dated.body);
    let dated_at = at(&dated.result["reminder"]["run_at"]).with_timezone(&paris);
    assert_eq!(dated_at.format("%Y-%m-%dT%H:%M").to_string(), nine);

    for (bad, code) in [
        (
            json!({ "in_minutes": 5, "at": "2030-01-01T09:00" }),
            "INVALID_ROUTINE",
        ),
        (json!({}), "INVALID_ROUTINE"),
        (json!({ "at": "2020-01-01T09:00" }), "INVALID_ROUTINE"),
        (json!({ "in_minutes": 0 }), "INVALID_ROUTINE"),
    ] {
        let mut args = json!({ "name": "Bad", "prompt": "Nothing." });
        args.as_object_mut()
            .unwrap()
            .extend(bad.as_object().unwrap().clone());
        let refused = tool(&h.app, &token, "reminder_create", args).await;
        assert!(refused.error, "{}", refused.body);
        assert!(refused.body.to_string().contains(code), "{}", refused.body);
    }
    let moved = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": id, "cron": "0 9 * * *" }),
    )
    .await;
    assert!(moved.error, "a reminder's time is fixed: {}", moved.body);
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;

    assert_eq!(
        set_enabled(&h.app, &main, &id, false).await.status,
        StatusCode::OK
    );
    let resumed = set_enabled(&h.app, &main, &id, true).await;
    assert_eq!(resumed.status, StatusCode::OK, "{}", resumed.body);
    assert_eq!(at(&resumed.body["routine"]["next_run_at"]), run_at);

    assert_eq!(fire(&h.app, run_at - chrono::Duration::seconds(1)).await, 0);
    assert_eq!(fire(&h.app, run_at).await, 1);
    assert_eq!(fire(&h.app, run_at + six_hours).await, 0, "it runs once");
    let run = runs(&h.app, &id).await;
    assert_eq!(run.len(), 1);
    h.app
        .wait_turn(&run[0].0.unwrap().to_string(), &["completed"])
        .await;
    for _ in 0..400 {
        let listed = owner_routines(&h.app, &main).await;
        if listed.iter().all(|r| r["id"] != id.as_str()) {
            assert_eq!(listed.len(), 1, "the other reminder stays: {listed:?}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("the reminder stayed listed after its run completed");
}
