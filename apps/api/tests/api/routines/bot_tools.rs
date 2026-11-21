//! Routines a bot manages over MCP, and who may manage them.

use super::*;

#[tokio::test]
async fn a_bot_manages_its_own_routines_over_mcp() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (_, token) = h.running(&main, GATE).await;

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
    let (_, other_token) = h.running(&other, GATE).await;
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
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (_, token) = h.running(&main, GATE).await;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_new_bot_takes_over_a_routine_without_the_owner_repeating_it() {
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let args = json!({ "name": "Morning digest", "prompt": "Summarize the owner's unread mail.",
                       "cron": "30 7 * * 1-5", "timezone": "Europe/Paris",
                       "active_hours": "always" });
    let (setup, token) = h.running(&main, GATE).await;
    let old = create(&h.app, &token, args.clone()).await;
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;

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
    h.app.wait_turn(root, &["waiting"]).await;

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
    assert_eq!(
        delegated.code(),
        "UPSTREAM_TARGET",
        "a task never hands work back up its chain: {}",
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
        else_api::db::delegations::create(
            &h.app.db.pool,
            child,
            main.parse().unwrap(),
            "x",
            None,
            &[],
            None
        )
        .await,
        Err(else_api::db::delegations::DelegateError::UpstreamTarget),
        "never back up the chain, even with a forged grant"
    );
    h.release(child).await;
    let continuation_token = h.continued(root, &token).await;

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
    h.app.wait_turn(root, &["completed"]).await;
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
    h.app.wait_turn(run, &["completed"]).await;
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
    let h = harness(TestApp::builder()).await;
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
    h.app.wait_turn(setup, &["completed"]).await;
    assert_eq!(fire(&h.app, at(&routine["next_run_at"])).await, 1);
    let run = runs(&h.app, routine["id"].as_str().unwrap()).await[0]
        .0
        .unwrap();
    let (_, run_token) = h.started(run).await;

    let (owner_inputs, run_inputs) = tokio::join!(h.inputs(setup), h.inputs(run));
    assert_ne!(
        else_api::providers::prompt::fingerprint_tag(&owner_inputs[0]),
        else_api::providers::prompt::fingerprint_tag(&run_inputs[0]),
        "routine permissions affect session compatibility"
    );
    let run_rules = else_api::providers::prompt::instructions(&run_inputs[0], "/p");
    assert!(!run_rules.contains("save it as a routine with routine_create"));
    assert!(!run_rules.contains("To hand a routine to another bot"));

    let refused = tool(&h.app, &run_token, "routine_create", digest("again")).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    let called = tool(
        &h.app,
        &h.grant_with(run, vec![Tool::Routines]).await,
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
        &h.grant_with(child, vec![Tool::Routines]).await,
        "routine_create",
        digest("x"),
    )
    .await;
    assert_eq!(called.code(), "NOT_ALLOWED", "{}", called.body);
    h.release(child).await;
    h.app.wait_turn(run, &["completed"]).await;
    assert!(owner_routines(&h.app, &leaf).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_answer_in_a_routine_run_lets_the_delegated_bot_change_its_routine() {
    let h = harness(TestApp::builder()).await;
    let manager = h.app.main_bot().await;
    let scout = h.app.create_bot("Scout").await;
    let (setup, token) = h.running(&scout, GATE_EVERY_ATTEMPT).await;
    let asked = tool(
        &h.app,
        &token,
        "ask_owner",
        json!({ "question": "Name the routine Leads?", "choices": ["Yes", "No"] }),
    )
    .await;
    let foreign = asked.result["question_id"].as_str().unwrap().to_owned();
    h.release(setup).await;
    h.app.wait_turn(setup, &["waiting"]).await;
    let chosen = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{scout}/questions/{foreign}/answer"),
            Some(json!({ "choice": "a" })),
        )
        .await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    let token = h.continued(setup, &token).await;
    let leads = create(
        &h.app,
        &token,
        json!({ "name": "Leads", "prompt": "Find leads; DM at most 5 per day.", "cron": "0 9 1 6 *" }),
    )
    .await;
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;
    let (setup, token) = h.running(&manager, GATE).await;
    let audit = create(
        &h.app,
        &token,
        digest(&format!("{GATE_EVERY_ATTEMPT} audit Scout's lead runs.")),
    )
    .await;
    h.release(setup).await;
    h.app.wait_turn(setup, &["completed"]).await;
    assert_eq!(fire(&h.app, at(&audit["next_run_at"])).await, 1);
    let run = runs(&h.app, audit["id"].as_str().unwrap()).await[0]
        .0
        .unwrap();

    let (_, run_token) = h.started(run).await;
    let asked = tool(
        &h.app,
        &run_token,
        "ask_owner",
        json!({ "question": "Raise Scout's DM cap?", "choices": ["Keep 5", "Raise to 20"] }),
    )
    .await;
    let question = asked.result["question_id"].as_str().unwrap().to_owned();
    h.release(run).await;
    h.app.wait_turn(run, &["waiting"]).await;
    let chosen = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{manager}/questions/{question}/answer"),
            Some(json!({ "choice": "b" })),
        )
        .await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    let run_token = h.continued(run, &run_token).await;

    let task = "[gate] Raise your Leads routine's DM cap to 20 per day.";
    for ineligible in [Uuid::new_v4().to_string(), foreign] {
        let refused = tool(
            &h.app,
            &run_token,
            "delegate",
            json!({ "target_bot_id": scout, "task": task, "owner_answer": ineligible }),
        )
        .await;
        assert_eq!(refused.code(), "ANSWER_UNAVAILABLE", "{}", refused.body);
    }
    let pending = tool(
        &h.app,
        &run_token,
        "delegate",
        json!({ "target_bot_id": scout, "task": task, "owner_answer": question }),
    )
    .await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let child = child_of(&h.app, run).await;
    h.release(run).await;

    let (_, child_token) = h.started(child).await;
    let rules = else_api::providers::prompt::instructions(&h.inputs(child).await[0], "/p");
    assert!(
        rules.contains("[else.bot verified owner answer]"),
        "{rules}"
    );
    assert!(rules.contains("Card: Raise Scout's DM cap?"), "{rules}");
    assert!(rules.contains("The owner chose:\n- Raise to 20"), "{rules}");
    assert!(!rules.contains("- Keep 5"), "{rules}");
    assert!(rules.contains("you may set up, change, or remove your OWN routines"));
    let edited = tool(
        &h.app,
        &child_token,
        "routine_update",
        json!({ "routine_id": leads["id"], "prompt": "Find leads; DM at most 20 per day." }),
    )
    .await;
    assert!(!edited.error, "{}", edited.body);
    let mut conn = h.app.db.pool.acquire().await.unwrap();
    assert_eq!(
        else_api::db::delegations::origin(&mut conn, child)
            .await
            .unwrap(),
        else_api::app::providers::Origin::RoutineRun,
        "the answer unlocks routines, not the owner-request permissions"
    );
    drop(conn);
    for (name, args) in [
        ("rename_bot", json!({ "bot_id": manager, "name": "Boss" })),
        ("mcp_install", json!({ "id": "github" })),
    ] {
        let refused = tool(&h.app, &child_token, name, args).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{name}: {}",
            refused.body
        );
    }
    h.release(child).await;
    h.app.wait_turn(child, &["completed"]).await;
    assert_eq!(
        owner_routines(&h.app, &scout).await[0]["prompt"],
        "Find leads; DM at most 20 per day."
    );
}

#[tokio::test]
async fn a_bot_runs_a_paused_routine_on_demand_without_moving_its_schedule() {
    let h = harness(TestApp::builder()).await;
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
    h.app.wait_turn(setup, &["completed"]).await;
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
    h.app.wait_turn(run, &["completed"]).await;

    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(listed[0]["last_run"]["status"], "completed");
    assert_eq!(listed[0]["last_run"]["turn_id"], run.to_string().as_str());
    assert_eq!(listed[0]["next_run_at"], next_run_at);
    assert_eq!(listed[0]["enabled"], true);
    common::until(
        "the completed routine run to be reviewed",
        common::poll(|| async {
            let reviewed: bool =
                sqlx::query_scalar("SELECT reviewed FROM routine_runs WHERE turn_id = ?1")
                    .bind(run)
                    .fetch_one(&h.app.db.pool)
                    .await
                    .unwrap();
            reviewed.then_some(())
        }),
    )
    .await;

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
    h.app.wait_turn(paused_run, &["completed"]).await;
    let listed = owner_routines(&h.app, &main).await;
    assert_eq!(listed[0]["last_run"]["status"], "completed");
    assert_eq!(
        listed[0]["enabled"], false,
        "a run on demand never resumes it"
    );
    assert!(listed[0]["next_run_at"].is_null());
}
