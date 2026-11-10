mod common;

use std::time::Duration;

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use common::TestApp;
use common::recorder::{self, GATE, GATE_EVERY_ATTEMPT, Harness, tool};
use serde_json::{Value, json};
use uuid::Uuid;

const TASK: &str = "Read the watcher job's output and tell the owner the BTC price.";

async fn harness() -> Harness {
    recorder::harness(4).await
}

fn watcher(extra: Value) -> Value {
    let mut args = json!({ "name": "BTC above 100k", "prompt": TASK });
    args.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    args
}

async fn trigger(app: &TestApp, id: &str, token: &str) -> common::Reply {
    app.call("POST", &format!("/webhooks/v1/{id}"), None, Some(token))
        .await
}

fn at(value: &Value) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value.as_str().unwrap())
        .unwrap()
        .to_utc()
}

async fn wakes(app: &TestApp, id: &str) -> Vec<(Uuid, DateTime<Utc>)> {
    sqlx::query_as(
        "SELECT turn_id, woke_at FROM webhook_runs WHERE webhook_id = ?1 ORDER BY woke_at",
    )
    .bind(id.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap()
}

async fn wait_for_wakes(app: &TestApp, id: &str, count: usize) -> Vec<(Uuid, DateTime<Utc>)> {
    for _ in 0..400 {
        let found = wakes(app, id).await;
        if found.len() >= count {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("webhook {id} never reached {count} wakes");
}

async fn admit(app: &TestApp, now: DateTime<Utc>) -> usize {
    else_api::db::webhooks::admit_requested(&app.db.pool, app.scheduler.registry(), now, 8)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_trigger_wakes_the_bot_with_its_prompt_and_later_events_wait_and_coalesce() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (setup, token) = h.running(&bot, GATE).await;
    let created = tool(&h.app, &token, "webhook_create", watcher(json!({}))).await;
    assert!(!created.error, "{}", created.body);
    let webhook = created.result["webhook"].clone();
    let id = webhook["id"].as_str().unwrap().to_owned();
    let secret = created.result["token"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("whk_"));
    assert_eq!(webhook["min_interval_minutes"], 15);
    assert_eq!(webhook["daily_limit"], 24);
    assert!(
        !webhook.to_string().contains(&secret),
        "the view never holds the token"
    );

    let retried = tool(&h.app, &token, "webhook_create", watcher(json!({}))).await;
    assert_eq!(retried.result["webhook"]["id"], id.as_str());
    assert!(retried.result["token"].is_null(), "a token is shown once");
    h.release(setup).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;

    let wrong = trigger(&h.app, &id, "whk_wrong").await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    let unknown = trigger(&h.app, &Uuid::new_v4().to_string(), &secret).await;
    assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
    assert!(wakes(&h.app, &id).await.is_empty());

    let first = trigger(&h.app, &id, &secret).await;
    assert_eq!(first.status, StatusCode::ACCEPTED, "{}", first.body);
    assert_eq!(first.body["status"], "requested");
    let (turn, _) = wait_for_wakes(&h.app, &id, 1).await[0];
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
    let finished: DateTime<Utc> = sqlx::query_scalar("SELECT finished_at FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    let packet = h.packets().await.into_iter().last().unwrap();
    assert!(
        packet.starts_with("[else.bot webhook] The webhook \"BTC above 100k\""),
        "{packet}"
    );
    assert!(packet.contains(TASK));
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body;
    assert!(
        snapshot["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["kind"] != "routine"),
        "the saved prompt stays hidden like a routine's"
    );

    let held = trigger(&h.app, &id, &secret).await;
    assert_eq!(held.body["status"], "requested");
    let cooldown = finished + chrono::Duration::minutes(15);
    assert_eq!(at(&held.body["wakes_at"]), cooldown);
    let again = trigger(&h.app, &id, &secret).await;
    assert_eq!(again.body["status"], "coalesced");
    assert_eq!(at(&again.body["wakes_at"]), cooldown);
    assert_eq!(
        admit(&h.app, Utc::now()).await,
        0,
        "the pace holds the wake"
    );
    assert_eq!(
        admit(&h.app, cooldown - chrono::Duration::seconds(1)).await,
        0
    );
    assert_eq!(admit(&h.app, cooldown).await, 1);
    assert_eq!(admit(&h.app, cooldown).await, 0, "two events, one wake");
    assert_eq!(wakes(&h.app, &id).await.len(), 2);
}

#[tokio::test]
async fn a_wake_cannot_manage_webhooks() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (setup, token) = h.running(&bot, GATE).await;
    let created = tool(
        &h.app,
        &token,
        "webhook_create",
        watcher(json!({ "prompt": format!("{GATE} {TASK}") })),
    )
    .await;
    let id = created.result["webhook"]["id"].as_str().unwrap().to_owned();
    let secret = created.result["token"].as_str().unwrap().to_owned();
    h.release(setup).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;

    trigger(&h.app, &id, &secret).await;
    let (turn, _) = wait_for_wakes(&h.app, &id, 1).await[0];
    let (_, wake_token) = h.started(turn).await;
    let listed = tool(&h.app, &wake_token, "webhook_list", json!({})).await;
    assert!(
        listed.status == StatusCode::FORBIDDEN || listed.error,
        "{}",
        listed.body
    );
    let made = tool(
        &h.app,
        &wake_token,
        "webhook_create",
        watcher(json!({ "name": "Loop" })),
    )
    .await;
    assert!(made.status == StatusCode::FORBIDDEN || made.error);
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["completed"]).await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM webhooks")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn a_faster_pace_waits_for_the_owner_to_approve_its_cost() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (turn, token) = h.running(&bot, GATE_EVERY_ATTEMPT).await;
    let fast = watcher(json!({ "min_interval_minutes": 1, "daily_limit": 100 }));
    let fast = &fast;
    let pending = tool(&h.app, &token, "webhook_create", fast.clone()).await;
    assert_eq!(pending.result["parked"], true, "{}", pending.body);
    let none: i64 = sqlx::query_scalar("SELECT count(*) FROM webhooks")
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
    assert_eq!(none, 0, "nothing is saved before approval");
    let (question, kind, title, context): (Uuid, String, String, String) =
        sqlx::query_as("SELECT id, kind, title, context FROM owner_questions WHERE turn_id = ?1")
            .bind(turn)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert_eq!(kind, "external");
    assert!(title.contains("every 1 min, up to 100 a day"), "{title}");
    assert!(
        context.contains("credits") && context.contains("about 100 turns a day"),
        "{context}"
    );
    h.release(turn).await;
    h.app.wait_turn(&turn.to_string(), &["waiting"]).await;

    let answered = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/questions/{question}/answer"),
            Some(json!({ "choice": "approve" })),
        )
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let (_, token) = h.started(turn).await;
    let created = tool(&h.app, &token, "webhook_create", fast.clone()).await;
    assert!(!created.error, "{}", created.body);
    assert_eq!(created.result["webhook"]["min_interval_minutes"], 1);
    assert_eq!(created.result["webhook"]["daily_limit"], 100);
    let retried = tool(&h.app, &token, "webhook_create", fast.clone()).await;
    assert!(!retried.error, "{}", retried.body);
    assert_eq!(
        retried.result["webhook"]["id"],
        created.result["webhook"]["id"]
    );
    assert!(
        retried.result["parked"].is_null(),
        "a retry returns the saved webhook without a second card: {}",
        retried.body
    );

    let id = created.result["webhook"]["id"].clone();
    let slower = tool(
        &h.app,
        &token,
        "webhook_update",
        json!({ "webhook_id": id, "min_interval_minutes": 30 }),
    )
    .await;
    assert!(
        !slower.error,
        "slowing down needs no approval: {}",
        slower.body
    );
    assert_eq!(slower.result["webhook"]["min_interval_minutes"], 30);
    h.release(turn).await;
}

#[tokio::test]
async fn a_linked_job_fires_its_webhook_only_when_it_exits_zero() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (setup, token) = h.running(&bot, GATE).await;
    let created = tool(&h.app, &token, "webhook_create", watcher(json!({}))).await;
    let id: Uuid = created.result["webhook"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let bot_id: Uuid = bot.parse().unwrap();
    sqlx::query(
        "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib, network)
         VALUES (?1, 'test', 'image', 1, 512, 'public')",
    )
    .bind(bot_id)
    .execute(&h.app.db.pool)
    .await
    .unwrap();
    let job = |status: &'static str| {
        let pool = h.app.db.pool.clone();
        async move {
            let job = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO guest_jobs (id, bot_id, command, status, deadline, webhook_id)
                 VALUES (?1, ?2, 'watch', 'running', '2100-01-01T00:00:00.000+00:00', ?3)",
            )
            .bind(job)
            .bind(bot_id)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("UPDATE guest_jobs SET status = ?2 WHERE id = ?1")
                .bind(job)
                .bind(status)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    let requested = || async {
        sqlx::query_scalar::<_, bool>(
            "SELECT requested_at IS NOT NULL OR EXISTS (SELECT 1 FROM webhook_runs WHERE webhook_id = ?1)
             FROM webhooks WHERE id = ?1",
        )
        .bind(id)
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap()
    };
    job("failed").await;
    job("lost").await;
    assert!(
        !requested().await,
        "a failed or lost watcher does not wake the bot"
    );
    job("completed").await;
    assert!(requested().await);
    h.release(setup).await;
    wait_for_wakes(&h.app, &id.to_string(), 1).await;
}

#[tokio::test]
async fn deleting_a_webhook_revokes_its_token_and_drops_a_waiting_wake() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (setup, token) = h.running(&bot, GATE).await;
    let created = tool(&h.app, &token, "webhook_create", watcher(json!({}))).await;
    let id = created.result["webhook"]["id"].as_str().unwrap().to_owned();
    let secret = created.result["token"].as_str().unwrap().to_owned();
    let rotated = tool(
        &h.app,
        &token,
        "webhook_update",
        json!({ "webhook_id": id, "rotate_token": true }),
    )
    .await;
    let fresh = rotated.result["token"].as_str().unwrap().to_owned();
    assert_ne!(fresh, secret);
    assert_eq!(
        trigger(&h.app, &id, &secret).await.status,
        StatusCode::UNAUTHORIZED,
        "a rotated token stops working"
    );
    let disabled = tool(
        &h.app,
        &token,
        "webhook_update",
        json!({ "webhook_id": id, "enabled": false }),
    )
    .await;
    assert_eq!(disabled.result["webhook"]["enabled"], false);
    assert_eq!(
        trigger(&h.app, &id, &fresh).await.status,
        StatusCode::CONFLICT
    );
    tool(
        &h.app,
        &token,
        "webhook_update",
        json!({ "webhook_id": id, "enabled": true }),
    )
    .await;
    let waiting = || async {
        sqlx::query(
            "UPDATE webhooks SET requested_at = '2030-01-01T00:00:00.000+00:00',
                 not_before = '2100-01-01T00:00:00.000+00:00' WHERE id = ?1",
        )
        .bind(id.parse::<Uuid>().unwrap())
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    };
    let requested = || async {
        sqlx::query_scalar::<_, bool>("SELECT requested_at IS NOT NULL FROM webhooks WHERE id = ?1")
            .bind(id.parse::<Uuid>().unwrap())
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap()
    };
    waiting().await;
    tool(
        &h.app,
        &token,
        "webhook_update",
        json!({ "webhook_id": id, "enabled": false }),
    )
    .await;
    assert!(!requested().await, "disabling drops a waiting wake");
    tool(
        &h.app,
        &token,
        "webhook_update",
        json!({ "webhook_id": id, "enabled": true }),
    )
    .await;
    waiting().await;
    let deleted = tool(
        &h.app,
        &token,
        "webhook_delete",
        json!({ "webhook_id": id }),
    )
    .await;
    assert_eq!(deleted.result["deleted"], true);
    assert!(!requested().await, "deleting drops a waiting wake");
    assert_eq!(
        trigger(&h.app, &id, &fresh).await.status,
        StatusCode::UNAUTHORIZED
    );
    h.release(setup).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    assert!(wakes(&h.app, &id).await.is_empty());
}

#[tokio::test]
async fn an_update_never_restores_a_faster_pace_the_owner_did_not_approve_for_it() {
    use else_api::db::routines::By;
    use else_api::db::webhooks::{self, Patch, WebhookError};
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (setup, token) = h.running(&bot, GATE).await;
    let created = tool(&h.app, &token, "webhook_create", watcher(json!({}))).await;
    let id: Uuid = created.result["webhook"]["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    h.release(setup).await;
    let by = By::Owner {
        bot_id: bot.parse().unwrap(),
    };
    let stale = webhooks::update(
        &h.app.db.pool,
        by,
        id,
        Patch {
            min_interval_minutes: Some(1),
            daily_limit: Some(100),
            ..Patch::default()
        },
    )
    .await;
    assert!(matches!(stale, Err(WebhookError::Changed)), "{stale:?}");
    let approved = else_api::app::webhooks::Pace::new(1, 100).unwrap();
    let other = webhooks::update(
        &h.app.db.pool,
        by,
        id,
        Patch {
            min_interval_minutes: Some(2),
            approved: Some(approved),
            ..Patch::default()
        },
    )
    .await;
    assert!(
        matches!(other, Err(WebhookError::Changed)),
        "an approval covers only the pace it named: {other:?}"
    );
    let renamed = webhooks::update(
        &h.app.db.pool,
        by,
        id,
        Patch {
            name: Some(else_api::app::routines::RoutineName::parse("Renamed").unwrap()),
            ..Patch::default()
        },
    )
    .await
    .unwrap()
    .0;
    assert_eq!(
        renamed.pace,
        else_api::app::webhooks::Pace::DEFAULT,
        "omitted fields keep the stored pace"
    );
}

#[tokio::test]
async fn the_owner_sees_pauses_and_deletes_webhooks_in_the_sidebar() {
    let h = harness().await;
    let bot = h.app.main_bot().await;
    let (setup, token) = h.running(&bot, GATE).await;
    let created = tool(&h.app, &token, "webhook_create", watcher(json!({}))).await;
    let id = created.result["webhook"]["id"].as_str().unwrap().to_owned();
    let secret = created.result["token"].as_str().unwrap().to_owned();
    h.release(setup).await;
    h.app.wait_turn(&setup.to_string(), &["completed"]).await;
    let webhooks = || async {
        h.app
            .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
            .await
            .body["webhooks"]
            .clone()
    };
    let listed = webhooks().await;
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["prompt"], TASK);
    assert_eq!(listed[0]["enabled"], true);
    assert!(
        !listed.to_string().contains(&secret),
        "the snapshot never carries a token"
    );
    let path = format!("/v1/bots/{bot}/webhooks/{id}");
    let paused = h
        .app
        .owner("PATCH", &path, Some(json!({ "enabled": false })))
        .await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);
    assert_eq!(webhooks().await[0]["enabled"], false);
    assert_eq!(
        trigger(&h.app, &id, &secret).await.status,
        StatusCode::CONFLICT
    );
    let deleted = h.app.owner("DELETE", &path, None).await;
    assert_eq!(deleted.status, StatusCode::OK, "{}", deleted.body);
    assert_eq!(webhooks().await, json!([]));
    assert_eq!(
        trigger(&h.app, &id, &secret).await.status,
        StatusCode::UNAUTHORIZED
    );
    let other = h.app.create_bot("Other").await;
    let foreign = h
        .app
        .owner("DELETE", &format!("/v1/bots/{other}/webhooks/{id}"), None)
        .await;
    assert_eq!(foreign.status, StatusCode::NOT_FOUND, "{}", foreign.body);
}
