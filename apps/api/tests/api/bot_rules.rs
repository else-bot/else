use crate::common;

use axum::http::StatusCode;
use common::TestApp;
use common::recorder::{GATE, GATE_EVERY_ATTEMPT, harness, tool};
use else_api::grants::Tool;
use serde_json::{Value, json};
use uuid::Uuid;

const OLD_RULE: &str = "When: a new issue lands in Linear project ELSE\n\
    Do: start a T3 Code thread in `else` that describes the issue";
const NEW_RULE: &str = "When: a new issue lands in Linear project ELSE\n\
    Do: start a T3 Code thread in `else` with exactly `fix issue {ISSUE_ID}`\n\
    Never: add words, a title, or context to that message.";

async fn rules(app: &TestApp, bot: &str) -> Value {
    let reply = app
        .owner("GET", &format!("/v1/bots/{bot}/rules"), None)
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    reply.body
}

async fn cards(app: &TestApp, turn: Uuid) -> Vec<Value> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT rules_update FROM items WHERE turn_id = ?1 AND rules_update IS NOT NULL
         ORDER BY seq",
    )
    .bind(turn)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    rows.iter()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

fn update(section: &str, content: &str, words: &str) -> Value {
    json!({ "section": section, "content": content, "owner_words": words })
}

#[tokio::test]
async fn a_bot_saves_a_rule_from_the_owner_and_reads_it_verbatim_next_turn() {
    let h = harness(common::TestApp::builder().max_active(2)).await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;

    let added = tool(
        &h.app,
        &token,
        "rules_update",
        update(
            "New Linear issue",
            OLD_RULE,
            "When a new issue comes in, start a thread.",
        ),
    )
    .await;
    assert!(!added.error, "{}", added.body);
    assert_eq!(added.result["version"], 1);
    assert_eq!(added.result["change"], "added");

    let retry = tool(
        &h.app,
        &token,
        "rules_update",
        update("new linear issue", OLD_RULE, "again"),
    )
    .await;
    assert_eq!(retry.result, json!({ "version": 1, "changed": false }));

    let words = "Just send fix issue ELSE-142, nothing else.";
    let changed = tool(
        &h.app,
        &token,
        "rules_update",
        update("New Linear issue", NEW_RULE, words),
    )
    .await;
    assert!(!changed.error, "{}", changed.body);
    assert_eq!(changed.result["version"], 2);
    assert_eq!(changed.result["change"], "changed");

    let shown = cards(&h.app, turn).await;
    assert_eq!(
        shown.len(),
        2,
        "one card per saved version, none for the retry"
    );
    assert_eq!(shown[1]["owner_words"], words);
    assert_eq!(shown[1]["section"], "New Linear issue");
    let diff = shown[1]["diff"].as_array().unwrap();
    assert!(diff.contains(&json!({ "op": "remove",
        "text": "Do: start a T3 Code thread in `else` that describes the issue" })));
    assert!(diff.contains(&json!({ "op": "add",
        "text": "Do: start a T3 Code thread in `else` with exactly `fix issue {ISSUE_ID}`" })));
    assert!(diff.contains(&json!({ "op": "same",
        "text": "When: a new issue lands in Linear project ELSE" })));

    let file = rules(&h.app, &main).await;
    assert_eq!(file["version"], 2);
    assert_eq!(file["sections"][0]["title"], "New Linear issue");
    assert_eq!(file["sections"][0]["body"], NEW_RULE);
    assert_eq!(file["history"].as_array().unwrap().len(), 2);
    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
        .await;
    assert_eq!(snapshot.body["rules_version"], 2, "{}", snapshot.body);
    let event: String = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE kind = 'bot.updated' ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&h.app.db.pool)
    .await
    .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&event).unwrap()["rules_version"],
        2
    );
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;

    let (next, _) = h.running(&main, GATE).await;
    let input = &h.inputs(next).await[0];
    assert!(
        input.rules.as_deref().unwrap().contains(NEW_RULE),
        "{:?}",
        input.rules
    );
    let packets = h.packets().await;
    let packet = packets.last().unwrap();
    assert!(packet.contains("[else.bot rules]"), "{packet}");
    assert!(packet.contains("`fix issue {ISSUE_ID}`"), "{packet}");
    h.release(next).await;
}

#[tokio::test]
async fn a_saved_rule_points_the_bot_at_its_enabled_routines() {
    let h = harness(common::TestApp::builder().max_active(2)).await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let without = tool(
        &h.app,
        &token,
        "rules_update",
        update("Tone", "Do: write plainly.", "Write plainly."),
    )
    .await;
    assert!(
        without.result.get("next").is_none(),
        "no routines, no next: {}",
        without.body
    );

    let mut ids = Vec::new();
    for name in ["Growth digest", "Old digest"] {
        let created = tool(
            &h.app,
            &token,
            "routine_create",
            json!({ "name": name, "prompt": "Post detailed sections.", "cron": "0 9 * * *" }),
        )
        .await;
        assert!(!created.error, "{}", created.body);
        ids.push(created.result["routine"]["id"].as_str().unwrap().to_owned());
    }
    let paused = tool(
        &h.app,
        &token,
        "routine_update",
        json!({ "routine_id": ids[1], "enabled": false }),
    )
    .await;
    assert!(!paused.error, "{}", paused.body);

    let saved = tool(
        &h.app,
        &token,
        "rules_update",
        update(
            "Digests",
            "Do: keep chat text brief; put details in a report.",
            "I'm not able to read it.",
        ),
    )
    .await;
    assert!(!saved.error, "{}", saved.body);
    let next = saved.result["next"].as_str().unwrap();
    assert!(
        next.contains(&format!("\"Growth digest\" ({})", ids[0])),
        "{next}"
    );
    assert!(next.contains("routine_update"), "{next}");
    assert!(
        !next.contains("Old digest"),
        "paused routines are left out: {next}"
    );

    let removed = tool(
        &h.app,
        &token,
        "rules_update",
        update("Digests", "", "Forget the digest rule."),
    )
    .await;
    assert_eq!(removed.result["change"], "removed", "{}", removed.body);
    assert!(
        removed.result.get("next").is_none(),
        "a removed rule contradicts nothing: {}",
        removed.body
    );
    h.release(turn).await;
}

#[tokio::test]
async fn the_owner_restores_an_earlier_version_or_the_empty_file() {
    let h = harness(common::TestApp::builder().max_active(2)).await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    for (content, words) in [(OLD_RULE, "first"), (NEW_RULE, "second")] {
        let saved = tool(
            &h.app,
            &token,
            "rules_update",
            update("New Linear issue", content, words),
        )
        .await;
        assert!(!saved.error, "{}", saved.body);
    }
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;

    let path = format!("/v1/bots/{main}/rules/restore");
    let restored = h
        .app
        .owner("POST", &path, Some(json!({ "version": 1 })))
        .await;
    assert_eq!(restored.status, StatusCode::OK, "{}", restored.body);
    assert_eq!(restored.body["version"], 3);
    let file = rules(&h.app, &main).await;
    assert_eq!(file["sections"][0]["body"], OLD_RULE);
    assert_eq!(file["history"][0]["change"], "restored");
    assert_eq!(file["history"][0]["restored_from"], 1);
    assert_eq!(file["history"][0]["author"], "owner");

    let again = h
        .app
        .owner("POST", &path, Some(json!({ "version": 1 })))
        .await;
    assert_eq!(
        again.body["version"], 3,
        "restoring the current text saves nothing"
    );

    let cleared = h
        .app
        .owner("POST", &path, Some(json!({ "version": 0 })))
        .await;
    assert_eq!(cleared.body["version"], 4);
    assert_eq!(rules(&h.app, &main).await["sections"], json!([]));

    let missing = h
        .app
        .owner("POST", &path, Some(json!({ "version": 99 })))
        .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);

    let stale = h
        .app
        .owner(
            "POST",
            &path,
            Some(json!({ "version": 1, "expected_version": 3 })),
        )
        .await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(rules(&h.app, &main).await["version"], 4);
    let current = h
        .app
        .owner(
            "POST",
            &path,
            Some(json!({ "version": 1, "expected_version": 4 })),
        )
        .await;
    assert_eq!(current.body["version"], 5);
}

async fn rule_rows(app: &TestApp, bot: &str) -> (Vec<Value>, Vec<Value>) {
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE bot_id = ?1 AND kind = 'item.upsert' ORDER BY seq",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    let live = events
        .iter()
        .map(|data| serde_json::from_str::<Value>(data).unwrap()["item"].clone())
        .filter(|item| item["rules_update"].is_object())
        .collect();
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    assert_eq!(snapshot.status, StatusCode::OK, "{}", snapshot.body);
    let shown = snapshot.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["rules_update"].is_object())
        .cloned()
        .collect();
    (live, shown)
}

fn undone(row: &Value) -> Option<&Value> {
    row["rules_update"].get("undone_at")
}

#[tokio::test]
async fn undoing_a_rule_change_marks_its_row_undone_live_and_after_reload() {
    let h = harness(common::TestApp::builder().max_active(2)).await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    for (section, content) in [
        ("New Linear issue", OLD_RULE),
        ("New Linear issue", NEW_RULE),
        ("Tone", "Short."),
    ] {
        let saved = tool(
            &h.app,
            &token,
            "rules_update",
            update(section, content, "remember this"),
        )
        .await;
        assert!(!saved.error, "{}", saved.body);
    }
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
    let path = format!("/v1/bots/{main}/rules/restore");
    let restore = |version: i64, expected: i64| {
        h.app.owner(
            "POST",
            &path,
            Some(json!({ "version": version, "expected_version": expected })),
        )
    };

    let stale = restore(1, 2).await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    let skipped = restore(1, 3).await;
    assert_eq!(skipped.status, StatusCode::OK, "{}", skipped.body);
    let (live, shown) = rule_rows(&h.app, &main).await;
    assert_eq!(
        live.len(),
        3,
        "neither a stale undo nor a skip back undoes a row"
    );
    assert!(shown.iter().all(|row| undone(row).is_none()), "{shown:?}");

    let (turn, token) = h.running(&main, GATE).await;
    let saved = tool(
        &h.app,
        &token,
        "rules_update",
        update(
            "Weekends",
            "Never: message me on weekends.",
            "not on weekends",
        ),
    )
    .await;
    assert_eq!(saved.result["version"], 5, "{}", saved.body);
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
    let undo = restore(4, 5).await;
    assert_eq!(undo.status, StatusCode::OK, "{}", undo.body);

    let file = rules(&h.app, &main).await;
    let versions: Vec<(i64, bool)> = file["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| (v["version"].as_i64().unwrap(), v["undone_at"].is_string()))
        .collect();
    assert_eq!(
        versions,
        [
            (6, false),
            (5, true),
            (4, false),
            (3, false),
            (2, false),
            (1, false)
        ]
    );
    let undone_at = file["history"][1]["undone_at"].clone();
    assert_eq!(undone_at, file["history"][0]["created_at"]);

    let (live, shown) = rule_rows(&h.app, &main).await;
    let last = live.last().unwrap();
    assert_eq!(last["rules_update"]["version"], 5);
    assert_eq!(undone(last), Some(&undone_at));
    assert_eq!(shown.len(), 4);
    assert_eq!(&shown[3], last, "the reload shows what the live event did");
    assert!(shown[..3].iter().all(|row| undone(row).is_none()));

    sqlx::query("UPDATE items SET rules_update = json_remove(rules_update, '$.undone_at')")
        .execute(&h.app.db.pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!(
        "../../migrations/0081_undone_rule_changes.sql"
    ))
    .execute(&h.app.db.pool)
    .await
    .unwrap();
    let (_, backfilled) = rule_rows(&h.app, &main).await;
    assert_eq!(
        backfilled, shown,
        "an undo saved before the stamp is backfilled"
    );
}

#[tokio::test]
async fn a_cleared_file_still_tells_the_next_turn_that_no_rules_apply() {
    let h = harness(common::TestApp::builder().max_active(2)).await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let saved = tool(
        &h.app,
        &token,
        "rules_update",
        update("New Linear issue", NEW_RULE, "send it"),
    )
    .await;
    assert!(!saved.error, "{}", saved.body);
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
    let cleared = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/rules/restore"),
            Some(json!({ "version": 0 })),
        )
        .await;
    assert_eq!(cleared.status, StatusCode::OK, "{}", cleared.body);

    let (next, _) = h.running(&main, GATE).await;
    let packets = h.packets().await;
    let packet = packets.last().unwrap();
    assert!(packet.contains("You have no saved rules now"), "{packet}");
    assert!(!packet.contains("fix issue {ISSUE_ID}`\nNever"), "{packet}");
    h.release(next).await;
}

#[tokio::test]
async fn delegated_tasks_and_routine_runs_cannot_edit_rules() {
    let h = harness(common::TestApp::builder().max_active(2)).await;
    let main = h.app.main_bot().await;
    let worker = h.app.create_bot("Work").await;
    let (root, token) = h.running(&main, GATE).await;
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": worker, "task": format!("{GATE} Save a rule.") }),
    )
    .await;
    assert!(!pending.error, "{}", pending.body);
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    let (_, child_token) = h.started(child).await;
    let args = update("Tone", "Short.", "Be short.");
    let denied = tool(&h.app, &child_token, "rules_update", args.clone()).await;
    assert_eq!(denied.status, StatusCode::FORBIDDEN);

    let forged = h.grant_with(child, vec![Tool::Rules]).await;
    let refused = tool(&h.app, &forged, "rules_update", args).await;
    assert_eq!(refused.code(), "NOT_ALLOWED");
    assert_eq!(rules(&h.app, &worker).await["version"], 0);
    h.release(child).await;
    h.release(root).await;
}

#[tokio::test]
async fn an_owner_answer_lets_the_delegated_bot_save_its_rule_in_the_owners_words() {
    let h = harness(common::TestApp::builder().max_active(2)).await;
    let manager = h.app.main_bot().await;
    let scout = h.app.create_bot("Scout").await;
    let (root, token) = h.running(&manager, GATE_EVERY_ATTEMPT).await;
    let asked = tool(
        &h.app,
        &token,
        "ask_owner",
        json!({ "question": "Scout's DM cap?", "choices": ["Keep 1 per run", "3 per run, 20 per 24h"] }),
    )
    .await;
    let question = asked.result["question_id"].as_str().unwrap().to_owned();
    h.release(root).await;
    h.app.wait_turn(root, &["waiting"]).await;
    let chosen = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{manager}/questions/{question}/answer"),
            Some(json!({ "choice": "b" })),
        )
        .await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    let token = h.continued(root, &token).await;
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({
            "target_bot_id": scout,
            "task": format!("{GATE} Replace your DM cap rule with 3 per run, 20 per 24h."),
            "owner_answer": question,
        }),
    )
    .await;
    assert!(!pending.error, "{}", pending.body);
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(root)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    h.release(root).await;

    let (_, child_token) = h.started(child).await;
    let instructions = else_api::providers::prompt::instructions(&h.inputs(child).await[0], "/p");
    assert!(
        instructions.contains("your own routines and rules"),
        "{instructions}"
    );
    let saved = tool(
        &h.app,
        &child_token,
        "rules_update",
        update(
            "DM cap",
            "Do: send at most 3 DMs per run and 20 per rolling 24h.",
            "Marketing Manager said the owner wants 3 per run.",
        ),
    )
    .await;
    assert!(!saved.error, "{}", saved.body);
    assert_eq!(saved.result["version"], 1, "{}", saved.body);
    assert_eq!(cards(&h.app, child).await.len(), 1);
    let chat = h
        .app
        .owner("GET", &format!("/v1/bots/{scout}/conversation"), None)
        .await;
    let card = chat.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["rules_update"].is_object())
        .unwrap_or_else(|| panic!("no rules card in Scout's chat: {}", chat.body));
    assert_eq!(card["rules_update"]["section"], "DM cap");
    h.release(child).await;
    h.app.wait_turn(child, &["completed"]).await;

    let saved = rules(&h.app, &scout).await;
    assert_eq!(saved["sections"][0]["title"], "DM cap");
    assert_eq!(
        saved["history"][0]["owner_words"], "3 per run, 20 per 24h",
        "the owner's verified pick, not the words the task relayed"
    );
    assert_eq!(rules(&h.app, &manager).await["version"], 0);
}

#[tokio::test]
async fn invalid_rules_are_refused_without_a_version() {
    let h = harness(common::TestApp::builder().max_active(2)).await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    for args in [
        json!({ "section": "Tone", "content": "Short." }),
        update("", "Short.", "x"),
        update("# Tone", "Short.", "x"),
        update("Tone", "Short.\n## Other\nx", "x"),
        update("Tone", "Short.", "   "),
        update("Tone", &"a".repeat(4_001), "x"),
        update("Tone", &"a".repeat(801), "x"),
        update("Missing", "", "remove it"),
    ] {
        let called = tool(&h.app, &token, "rules_update", args.clone()).await;
        assert!(called.error, "{args}: {}", called.body);
    }
    assert_eq!(rules(&h.app, &main).await["version"], 0);
    assert!(cards(&h.app, turn).await.is_empty());
    h.release(turn).await;
}
