//! Routine edits and the defaults new routines get.

use super::*;

#[tokio::test]
async fn bot_edits_leave_one_ordered_notice_in_live_and_paged_history() {
    let h = harness(TestApp::builder()).await;
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
    h.app.wait_turn(turn, &["completed"]).await;
    let (next, _) = h.running(&bot, GATE).await;
    let inputs = h.inputs(next).await;
    let history = &inputs[0].history;
    assert!(
        history
            .iter()
            .any(|entry| entry.kind == else_api::db::turns::ItemKind::User)
    );
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
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let sent = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/turns"),
            Some(json!({ "request_id": Uuid::new_v4(), "text": GATE,
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
