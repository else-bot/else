//! Approve-always grants, and the drafts a delegated task's origin allows.

use super::*;

async fn granted(app: &TestApp, bot: &str) -> Value {
    app.owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["draft_grants"]
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn approving_always_approves_later_drafts_of_that_kind_until_stopped() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let reply = draft(
        app,
        &token,
        json!({ "platform": "x", "account": "@Proliquid_xyz", "body": "Agreed.",
                "destination": "https://x.com/someone/status/1" }),
    )
    .await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(shown(app, &main, turn).await[0]["kind"], Value::Null);
    let refused = owner_action(
        app,
        &main,
        reply,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    assert_eq!(refused.body["error"]["code"], "no_kind");
    assert_eq!(
        shown(app, &main, turn).await[0]["status"],
        "pending",
        "an X reply cannot be approved always, and the refusal approves nothing"
    );
    let discarded = owner_action(app, &main, reply, "discard", json!({})).await;
    assert_eq!(discarded.body["requeued"], true, "{}", discarded.body);

    let token = h.continued(turn, &token).await;
    let post = |body: &str| json!({ "platform": "x", "account": "@Proliquid_xyz", "body": body });
    let first = draft(app, &token, post("A whale flipped long.")).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let card = shown(app, &main, turn).await[1].clone();
    assert_eq!(
        card["kind"],
        json!({ "platform": "x", "account_id": null, "account": "@proliquid_xyz",
                "destination": null })
    );
    assert_eq!(granted(app, &main).await, json!([]));
    let approved = owner_action(
        app,
        &main,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(approved.body["draft"]["status"], "approved");
    assert_eq!(approved.body["draft"]["approval_source"], "button");
    let expected_grants = json!([{
        "platform": "x", "account_id": null, "account": "@proliquid_xyz", "destination": null
    }]);
    assert_eq!(granted(app, &main).await, expected_grants);
    let update: String = sqlx::query_scalar(
        "SELECT data FROM bot_events WHERE bot_id = ?1 AND kind = 'bot.updated' ORDER BY seq DESC LIMIT 1",
    )
    .bind(main.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    let update: Value = serde_json::from_str(&update).unwrap();
    assert_eq!(update["draft_grants"], expected_grants);
    let replay = owner_action(
        app,
        &main,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(replay.status, StatusCode::OK, "{}", replay.body);
    assert_eq!(replay.body["requeued"], false);

    let token = h.continued(turn, &token).await;
    let drafted = tool(
        app,
        &token,
        "draft_message",
        json!({ "platform": "x", "account": "@PROLIQUID_XYZ", "body": "A whale flipped short." }),
    )
    .await;
    let result = &drafted.result;
    assert_eq!(result["status"], "approved", "{}", drafted.body);
    assert!(result.get("parked").is_none(), "nothing waits on the owner");
    assert!(
        result["next"]
            .as_str()
            .unwrap()
            .contains("approves X post like this always"),
        "{result}"
    );
    let second: Uuid = result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let card = journaled(app, &main, second).await;
    assert_eq!(card["approval_source"], "standing");
    assert_eq!(card["can_approve"], false);
    let claimed = tool(
        app,
        &token,
        "start_draft_send",
        json!({ "draft_id": second }),
    )
    .await;
    assert_eq!(claimed.result["status"], "sending", "{}", claimed.body);
    let sent = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": second, "status": "sent", "detail": "post id 7" }),
    )
    .await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    let third = tool(app, &token, "draft_message", post("A third whale.")).await;
    assert_eq!(
        third.result["status"], "approved",
        "an approved-always draft parks nothing, so the request can send more: {}",
        third.body
    );
    let reply = tool(
        app,
        &token,
        "draft_message",
        json!({ "platform": "x", "account": "@proliquid_xyz", "reply_to": "1890",
                "body": "Thanks!" }),
    )
    .await;
    assert_eq!(
        reply.result["status"], "pending",
        "an X reply named only by reply_to is not a post: {}",
        reply.body
    );
    let reply: Uuid = reply.result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let discarded = owner_action(app, &main, reply, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);

    let stop = format!("/v1/bots/{main}/outgoing-drafts/{second}/always");
    let stopped = app.owner("DELETE", &stop, None).await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    assert_eq!(granted(app, &main).await, json!([]));
    assert_eq!(
        stopped.body["draft"]["status"], "sent",
        "a sent post stays sent"
    );
    let again = app.owner("DELETE", &stop, None).await;
    assert_eq!(
        again.status,
        StatusCode::OK,
        "stopping twice changes nothing"
    );

    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    let (next, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    draft(app, &token, post("Another whale.")).await;
    h.release(next).await;
    app.wait_turn(next, &["waiting"]).await;
    assert_eq!(
        shown(app, &main, next).await[0]["status"],
        "pending",
        "after stopping, the kind waits for approval again"
    );
}

async fn root_source(app: &TestApp, root: Uuid, kind: &str) {
    sqlx::query(
        "UPDATE items SET kind = ?2 WHERE id = (SELECT owner_item_id FROM turns WHERE id = ?1)",
    )
    .bind(root)
    .bind(kind)
    .execute(&app.db.pool)
    .await
    .unwrap();
}

/// Makes `root` a scheduled run of a saved routine. A 'routine' item alone
/// is how a webhook wake starts.
async fn scheduled_run(app: &TestApp, root: Uuid) {
    root_source(app, root, "routine").await;
    let routine = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO routines (id, bot_id, conversation_id, name, prompt, cron, timezone,
             created_turn_id, created_name, request_hash)
         SELECT ?1, bot_id, conversation_id, 'School blog', 'Forward new posts.', '0 8 * * 1-5',
             'UTC', id, 'School blog', zeroblob(32) FROM turns WHERE id = ?2",
    )
    .bind(routine)
    .bind(root)
    .execute(&app.db.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO routine_runs (id, routine_id, due_at, name, turn_id)
         VALUES (randomblob(16), ?1, '2030-01-01T08:00:00Z', 'School blog', ?2)",
    )
    .bind(routine)
    .bind(root)
    .execute(&app.db.pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_a_routine_run_delegated_drafts_for_the_owners_approval() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, root, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    scheduled_run(app, root).await;

    let id = draft(app, &token, ship_post()).await;
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    let card = shown(app, &leaf, child).await[0].clone();
    assert_eq!(
        card["status"], "pending",
        "the owner still approves the post"
    );
    assert_eq!(card["id"], id.to_string(), "{card}");

    let approved = owner_action(app, &leaf, id, "approve", json!({ "version": 1 })).await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    h.continued(child, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("send exactly this X post now"), "{packet}");
    h.release(child).await;
    app.wait_turn(child, &["completed"]).await;
    let delegation: String =
        sqlx::query_scalar("SELECT status FROM delegations WHERE child_turn_id = ?1")
            .bind(child)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(delegation, "completed", "the routine run gets the result");
}

/// Drafts `family_message` from a task delegated by a scheduled routine run
/// or a webhook wake, clicks Approve always on its card, and returns the
/// next draft's result in the same task.
async fn delegated_draft_after_approving_always(scheduled: bool) -> Value {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, root, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    if scheduled {
        scheduled_run(app, root).await;
    } else {
        root_source(app, root, "routine").await;
    }
    let first = draft(app, &token, family_message()).await;
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;
    let approved = owner_action(
        app,
        &leaf,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(child, &token).await;
    let mut next = family_message();
    next["body"] = json!("Pick-up moved to 16:30.");
    let drafted = tool(app, &token, "draft_message", next).await;
    drafted.result
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_a_routine_run_delegated_sends_with_its_bots_approve_always() {
    let result = delegated_draft_after_approving_always(true).await;
    assert_eq!(result["status"], "approved", "{result}");
    assert_eq!(
        result["draft"]["content"]["body"], "Pick-up moved to 16:30.",
        "it sends exactly what the task proposed: {result}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_a_webhook_wake_delegated_still_asks_for_each_message() {
    let result = delegated_draft_after_approving_always(false).await;
    assert_eq!(result["status"], "pending", "{result}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_whatsapp_draft_is_approved_always_for_its_chat_only() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let to = |chat: &str, body: &str| {
        json!({ "platform": "whatsapp", "account": "+33612345678", "destination": chat,
                "body": body })
    };
    let family = "120363041234567890@g.us";
    let unsigned = draft(
        app,
        &token,
        json!({ "platform": "whatsapp", "destination": family, "body": "Bonjour" }),
    )
    .await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    assert_eq!(shown(app, &main, turn).await[0]["kind"], Value::Null);
    let refused = owner_action(
        app,
        &main,
        unsigned,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(
        refused.body["error"]["code"], "no_kind",
        "without its sending account, consent could outlive a switch of account: {}",
        refused.body
    );
    let discarded = owner_action(app, &main, unsigned, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);
    let token = h.continued(turn, &token).await;
    let first = draft(app, &token, to(family, "Nouvel article sur le blog.")).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let kind = json!({ "platform": "whatsapp", "account_id": null, "account": "+33612345678",
                       "destination": family });
    assert_eq!(
        shown(app, &main, turn).await[1]["kind"],
        kind,
        "a WhatsApp card offers Approve always for its one chat"
    );
    let approved = owner_action(
        app,
        &main,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(granted(app, &main).await, json!([kind]));

    let token = h.continued(turn, &token).await;
    let same = tool(
        app,
        &token,
        "draft_message",
        to(family, "Sortie scolaire vendredi."),
    )
    .await;
    assert_eq!(same.result["status"], "approved", "{}", same.body);
    let other = tool(
        app,
        &token,
        "draft_message",
        to("33698765432@s.whatsapp.net", "Bonjour"),
    )
    .await;
    assert_eq!(
        other.result["status"], "pending",
        "another chat still waits for the owner: {}",
        other.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_reply_saying_always_approves_a_delegated_chat_always() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (leaf, _, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    let school = "120363431565310389@g.us";
    let to = |chat: &str, body: &str| {
        json!({ "platform": "whatsapp", "account": "+33612345678", "destination": chat,
                "body": body })
    };
    let first = draft(app, &token, to(school, "Rue aux enfants vendredi.")).await;
    h.release(child).await;
    app.wait_turn(child, &["waiting"]).await;

    let reply = child;
    let (_, token) = joined(
        &h,
        &leaf,
        child,
        &token,
        "Always approve posting messages",
        Some(json!({ "id": first, "version": 1 })),
    )
    .await;
    let unquoted = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "approve everything always" }),
    )
    .await;
    assert!(
        unquoted.error && unquoted.code() == "NOT_QUOTED",
        "only the owner's own words can approve always: {}",
        unquoted.body
    );
    assert_eq!(granted(app, &leaf).await, json!([]));
    assert_eq!(shown(app, &leaf, child).await[0]["status"], "pending");

    let confirmed = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "Always approve posting messages" }),
    )
    .await;
    let result = confirmed.result;
    assert_eq!(result["status"], "approved", "{}", confirmed.body);
    let kind = json!({ "platform": "whatsapp", "account_id": null, "account": "+33612345678",
                       "destination": school });
    assert_eq!(result["always"], kind, "{result}");
    assert_eq!(granted(app, &leaf).await, json!([kind]));
    let card = shown(app, &leaf, reply).await[0].clone();
    assert_eq!(card["approval_source"], "message");
    let replay = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "Always approve posting messages" }),
    )
    .await;
    assert_eq!(replay.result["status"], "approved", "{}", replay.body);

    let same = tool(
        app,
        &token,
        "draft_message",
        to(school, "Invitation de la CAF."),
    )
    .await;
    assert_eq!(
        same.result["status"], "approved",
        "the next post to that chat needs no card: {}",
        same.body
    );
    let same: Uuid = same.result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let other = tool(
        app,
        &token,
        "draft_message",
        to("33698765432@s.whatsapp.net", "Bonjour"),
    )
    .await;
    assert_eq!(
        other.result["status"], "pending",
        "another chat still waits for the owner: {}",
        other.body
    );
    let other: Uuid = other.result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    h.release(reply).await;
    app.wait_turn(reply, &["waiting"]).await;
    let discarded = owner_action(app, &leaf, other, "discard", json!({})).await;
    assert_eq!(discarded.status, StatusCode::OK, "{}", discarded.body);
    let token = h.continued(reply, &token).await;

    let stop = format!("/v1/bots/{leaf}/outgoing-drafts/{same}/always");
    let stopped = app.owner("DELETE", &stop, None).await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    assert_eq!(granted(app, &leaf).await, json!([]));
    let after = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "Always approve posting messages" }),
    )
    .await;
    assert!(
        after.error && after.code() == "NOT_ALWAYS",
        "a retry never brings back what the owner stopped: {}",
        after.body
    );
    let claimed = tool(
        app,
        &token,
        "start_draft_send",
        json!({ "draft_id": first }),
    )
    .await;
    assert_eq!(claimed.result["status"], "sending", "{}", claimed.body);
    let failed = tool(
        app,
        &token,
        "report_draft_send",
        json!({ "draft_id": first, "status": "failed", "detail": "offline" }),
    )
    .await;
    assert_eq!(failed.result["status"], "failed", "{}", failed.body);
    let retried = tool(
        app,
        &token,
        "confirm_draft_approval",
        json!({ "draft_id": first, "always": "Always approve posting messages" }),
    )
    .await;
    assert!(
        retried.error && retried.code() == "NOT_ALWAYS",
        "nor does a retry after its send failed: {}",
        retried.body
    );
    assert_eq!(granted(app, &leaf).await, json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_rooted_in_neither_the_owner_nor_a_routine_cannot_draft() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let (_, root, _, child) = delegated_task(&h).await;
    let (_, token) = h.started(child).await;
    root_source(app, root, "status").await;

    let refused = tool(app, &token, "draft_message", ship_post()).await;
    assert!(
        refused.error && refused.code() == "NOT_ALLOWED",
        "{}",
        refused.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn approving_a_task_always_approves_its_later_messages_to_anyone() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    let dm = |to: &str, task: &str| {
        json!({ "platform": "x", "account": "@Proliquid_xyz", "destination": to,
                "task": task, "body": format!("Hey {to}, want VIP access?") })
    };
    let unowned = tool(
        app,
        &token,
        "draft_message",
        json!({ "platform": "x", "destination": "@ada", "task": "Initial lead DMs",
                "body": "Hi" }),
    )
    .await;
    assert!(
        unowned.error
            && unowned
                .result
                .as_str()
                .unwrap()
                .contains("a task needs the account it sends from"),
        "{}",
        unowned.body
    );
    let first = draft(app, &token, dm("@ada", "Initial  lead DMs")).await;
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let kind = json!({ "platform": "x", "account_id": null, "account": "@proliquid_xyz",
                       "destination": null, "task": "initial lead dms" });
    assert_eq!(
        shown(app, &main, turn).await[0]["kind"],
        kind,
        "a DM filed under a task can be approved always"
    );
    let approved = owner_action(
        app,
        &main,
        first,
        "approve",
        json!({ "version": 1, "always": true }),
    )
    .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    assert_eq!(granted(app, &main).await, json!([kind]));

    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("[else.bot always-approved message tasks]")
            && packet.contains("\"task\":\"initial lead dms\""),
        "the next attempt learns the saved task to reuse: {packet}"
    );
    let next = tool(app, &token, "draft_message", dm("@bob", "initial lead dms")).await;
    assert_eq!(
        next.result["status"], "approved",
        "the same task from the same account reaches a new lead without a card: {}",
        next.body
    );
    let other = tool(app, &token, "draft_message", dm("@cy", "Follow-ups")).await;
    assert_eq!(
        other.result["status"], "pending",
        "another task still waits for the owner: {}",
        other.body
    );
}
