//! When runs start, skip, and hold.

use super::*;

#[tokio::test]
async fn overlapping_occurrences_are_skipped_and_missed_ones_coalesce() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.running(&main, GATE).await;
    let routine = create(
        &h.app,
        &token,
        json!({ "name": "Slow", "prompt": format!("{GATE} a long routine report for overlap checks."),
                "cron": "0 * * * *", "active_hours": "always" }),
    )
    .await;
    let id = routine["id"].as_str().unwrap().to_owned();
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;
    let due = at(&json!("2030-01-01T00:00:00Z"));
    sqlx::query("UPDATE routines SET next_due_at = '2030-01-01T00:00:00.000+00:00' WHERE id = ?1")
        .bind(id.parse::<Uuid>().unwrap())
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    fire(&h.app, due).await;
    let first = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(first, &["running"]).await;

    assert_eq!(fire(&h.app, due + chrono::Duration::hours(1)).await, 0);
    let all = runs(&h.app, &id).await;
    assert_eq!(all.len(), 2);
    assert_eq!(all[1], (None, Some("overlap".into()), "Slow".into()));
    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(listed[0]["last_run"]["status"], "skipped");
    assert_eq!(listed[0]["last_run"]["skip_reason"], "overlap");
    h.release(first).await;
    h.app.wait_turn(first, &["completed"]).await;

    h.running(&main, GATE).await;
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
    common::until("turns, cancellation, and routine review to settle", common::poll(|| async {
        let busy: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM turns WHERE status NOT IN ('completed', 'interrupted', 'failed', 'uncertain'))
             OR EXISTS (SELECT 1 FROM bots WHERE cancel_pending)
             OR EXISTS (SELECT 1 FROM routine_runs WHERE turn_id IS NOT NULL AND NOT reviewed)",
        )
        .fetch_one(&h.app.db.pool)
        .await
        .unwrap();
        (!busy).then_some(())
    })).await;

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
    let h = harness(TestApp::builder().max_active(2)).await;
    let main = h.app.main_bot().await;
    let due = at(&json!("2030-01-01T00:00:00Z"));
    for name in ["Scout", "Auto"] {
        let bot = h.app.create_bot(name).await;
        let (setup, token) = h.running(&bot, GATE).await;
        let routine = create(
            &h.app,
            &token,
            json!({ "name": "Hourly", "prompt": "[synthetic:hang] an hourly sweep that keeps running.",
                    "cron": "0 * * * *", "active_hours": "always" }),
        )
        .await;
        h.release(setup).await;
        h.app.wait_turn(setup, &["completed"]).await;
        sqlx::query(
            "UPDATE routines SET next_due_at = '2030-01-01T00:00:00.000+00:00' WHERE id = ?1",
        )
        .bind(routine["id"].as_str().unwrap().parse::<Uuid>().unwrap())
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    }
    assert_eq!(fire(&h.app, due).await, 2);
    let first: Uuid = common::until("a routine turn to run", common::poll(|| async {
        sqlx::query_scalar(
            "SELECT id FROM turns WHERE status = 'running' AND id IN (SELECT turn_id FROM routine_runs)",
        )
        .fetch_optional(&h.app.db.pool)
        .await
        .unwrap()
    }))
    .await;

    let owner = h.send(&main, "[synthetic] are you there?").await;
    assert_eq!(
        h.app.wait_turn(owner, &["completed"]).await,
        "completed",
        "the owner message ran while routines held the other slot"
    );
    assert_eq!(h.app.wait_turn(first, &["running"]).await, "running");
}

#[tokio::test(flavor = "multi_thread")]
async fn pausing_stops_future_runs_and_resuming_counts_from_now() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.running(&main, GATE).await;
    let routine = create(
        &h.app,
        &token,
        digest(&format!("{GATE} a long routine report for pause checks.")),
    )
    .await;
    let id = routine["id"].as_str().unwrap().to_owned();
    let original_next = routine["next_run_at"].clone();
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;

    fire(&h.app, at(&original_next)).await;
    let run = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(run, &["running"]).await;
    let paused = set_enabled(&h.app, &main, &id, false).await;
    assert_eq!(paused.status, StatusCode::OK, "{}", paused.body);
    assert_eq!(paused.body["routine"]["enabled"], false);
    assert_eq!(paused.body["routine"]["next_run_at"], Value::Null);
    assert_eq!(paused.body.get("queued"), None);
    h.release(run).await;
    h.app.wait_turn(run, &["completed"]).await;
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

    let (_, token) = h.running(&main, GATE).await;
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
    let h = harness(TestApp::builder()).await;
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
    h.app.wait_turn(setup, &["completed"]).await;
    fire(&h.app, at(&routine["next_run_at"])).await;
    let run = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(run, &["running"]).await;
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
    h.app.wait_turn(run, &["completed"]).await;
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
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.running(&main, GATE).await;
    let mut args = digest(&format!("{GATE} a long routine report for stop checks."));
    args["active_hours"] = json!("always");
    let routine = create(&h.app, &token, args).await;
    let id = routine["id"].as_str().unwrap().to_owned();
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;

    fire(&h.app, at(&routine["next_run_at"])).await;
    let run = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(run, &["running"]).await;
    let stop = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(stop.status.is_success(), "{}", stop.body);
    h.app.wait_turn(run, &["interrupted"]).await;
    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(listed[0]["last_run"]["status"], "interrupted");
    assert_eq!(listed[0]["enabled"], true);

    let closed = h
        .app
        .state
        .gate
        .close(common::SETTLE, else_api::maintenance::Pause::backup(None))
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE routines SET next_due_at = '2026-01-01T00:00:00.000+00:00' WHERE id = ?1")
        .bind(id.parse::<Uuid>().unwrap())
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    h.app.scheduler.wake();
    common::never_polled(
        "the scheduler admits a run during maintenance",
        Duration::from_millis(1500),
        || async {
            let count = runs(&h.app, &id).await.len();
            (count != 1).then_some(count)
        },
    )
    .await;
    drop(closed);
    common::until(
        "the second run after maintenance",
        common::poll(|| async { (runs(&h.app, &id).await.len() == 2).then_some(()) }),
    )
    .await;
    let after = runs(&h.app, &id).await;
    assert_eq!(after.len(), 2, "admitted once the gate opened");
    let second = after
        .iter()
        .find_map(|r| r.0.filter(|t| *t != run))
        .unwrap();
    h.release(second).await;
    h.app.wait_turn(second, &["completed"]).await;

    let other = h.app.create_bot("Work").await;
    let (other_setup, other_token) = h.running(&other, GATE).await;
    let theirs = create(&h.app, &other_token, digest("Weekly report.")).await;
    h.release(other_setup).await;
    h.app.wait_turn(other_setup, &["completed"]).await;
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
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.running(&main, GATE).await;
    let routine = create(
        &h.app,
        &token,
        digest(&format!("{GATE} a long routine report for restart checks.")),
    )
    .await;
    let id = routine["id"].as_str().unwrap().to_owned();
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;
    let due = at(&routine["next_run_at"]);
    fire(&h.app, due).await;
    let run = runs(&h.app, &id).await[0].0.unwrap();
    h.app.wait_turn(run, &["running"]).await;
    let Harness { app, recorder } = h;
    let app = app.restart().await;
    let status = app
        .wait_turn(run, &["uncertain", "completed", "interrupted"])
        .await;
    assert_ne!(status, "queued");
    assert_eq!(
        else_api::db::routines::admit_due(&app.db.pool, &app.state.registry, None, due, 8)
            .await
            .unwrap()
            .queued,
        0
    );
    assert_eq!(runs(&app, &id).await.len(), 1);
    drop(recorder);
}

#[tokio::test]
async fn a_reminder_runs_once_then_leaves_the_list() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (setup, token) = h.running(&main, GATE).await;
    let args = json!({ "name": "Scout check", "prompt": "Check how many DMs Scout sent.",
                       "in_minutes": 360 });
    let before = Utc::now();
    let called = tool(&h.app, &token, "reminder_create", args.clone()).await;
    called.ok();
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
    dated.ok();
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
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;

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
    let turn = run[0].0.unwrap().to_string();
    h.app.wait_turn(&turn, &["completed"]).await;
    let listed = common::until(
        "the completed reminder to disappear from the list",
        common::poll(|| async {
            let listed = owner_routines(&h.app, &main).await;
            listed
                .iter()
                .all(|r| r["id"] != id.as_str())
                .then_some(listed)
        }),
    )
    .await;
    assert_eq!(listed.len(), 1, "the other reminder stays: {listed:?}");
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await
        .body;
    let ran = snapshot["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.as_str())
        .unwrap();
    assert_eq!(ran["routine"]["name"], "Scout check");
    assert_eq!(
        ran["routine"]["reminder"], true,
        "its run still shows as a reminder's"
    );
}
