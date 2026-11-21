//! Turns a mail event wakes.

use super::*;

#[tokio::test]
async fn mail_event_origin_removes_computer_authority() {
    let transport = Arc::new(Transport {
        sends: AtomicUsize::new(0),
        uncertain: false,
        recovers: None,
    });
    let h = recorder::harness(common::TestApp::builder().mail(transport.clone())).await;
    let bot: Uuid = h.app.main_bot().await.parse().unwrap();
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, bot, &permissions())
        .await
        .unwrap();
    let turn = mail_event(
        &h,
        &account,
        &format!("INBOX {GATE}"),
        "Summarize it for me.",
    )
    .await;
    let notice: String =
        sqlx::query_scalar("SELECT text FROM items WHERE turn_id = ?1 AND kind = 'connector'")
            .bind(turn)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    assert!(
        notice.contains("instructions for this trigger: Summarize it for me."),
        "{notice}"
    );
    let (_, token) = h.started(turn).await;
    let grant = h.app.state.grants.resolve(&token).await.unwrap();
    assert!(grant.allows(Tool::Mail));
    assert!(
        grant.allows(Tool::DraftEmail),
        "the owner reviews its drafts"
    );
    for forbidden in [
        Tool::Computer,
        Tool::CreateBot,
        Tool::RequestSecret,
        Tool::AskOwner,
        Tool::Routines,
        Tool::Mcp,
        Tool::ManageMcp,
    ] {
        assert!(!grant.allows(forbidden));
    }
    assert_eq!(grant.memory_scope, Some(vec![bot]));
    let send = json!({ "account_id": account.id, "action_id": Uuid::new_v4(),
                       "command": { "operation": "send", "draft": {
                           "to": ["owner@example.com"], "subject": "New mail", "body": "Summary" } } });
    let sent = tool(&h.app, &token, "mail_action", send).await;
    sent.ok();
    assert_eq!(sent.result["status"], "succeeded", "{}", sent.body);
    assert_eq!(
        transport.sends.load(Ordering::SeqCst),
        1,
        "a standing send permission still sends"
    );
    let search = json!({ "account_id": account.id,
                         "command": { "operation": "search", "folder": "INBOX", "limit": 200 } });
    let wide = tool(&h.app, &token, "mail_action", search).await;
    assert!(
        wide.error
            && wide
                .body
                .to_string()
                .contains("invalid_mail_command: limit must be 1 to 100"),
        "{}",
        wide.body
    );
    for name in [
        "ask_owner",
        "confirm_draft_approval",
        "mail_trigger_set",
        "mail_trigger_delete",
        "mcp_list",
        "mcp_install",
        "mcp_request",
    ] {
        let refused = tool(&h.app, &token, name, json!({})).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{name}: {}",
            refused.body
        );
    }
    let inputs = h.inputs(turn).await;
    assert_eq!(
        inputs[0].delegation.origin,
        else_api::app::providers::Origin::ConnectorEvent
    );
    let snapshot = else_api::db::conversation::snapshot(&h.app.db.pool, bot, 100, None)
        .await
        .unwrap()
        .unwrap();
    assert!(
        snapshot
            .items
            .iter()
            .all(|item| item.kind != else_api::db::turns::ItemKind::Connector)
    );
    let mut conn = h.app.db.pool.acquire().await.unwrap();
    let source: Uuid = sqlx::query_scalar("SELECT owner_item_id FROM turns WHERE id=?1")
        .bind(turn)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    else_api::db::events::item_upsert(&mut conn, bot, source)
        .await
        .unwrap();
    let visible: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bot_events WHERE kind='item.upsert' AND json_extract(data,'$.item.kind')='connector')").fetch_one(&mut *conn).await.unwrap();
    assert!(!visible);
    drop(conn);
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
}

/// An inbox bot passes a notice on as its trigger says (issue #518). The
/// task runs on email content, so it sees only the task: never the other
/// bot's memory or mail, which the email could then have read back.
#[tokio::test]
async fn a_mail_event_hands_a_notice_to_another_bot_without_its_memory_or_mail() {
    let h = recorder::harness(common::TestApp::builder()).await;
    let inbox: Uuid = h.app.main_bot().await.parse().unwrap();
    let marketing = h.app.create_bot("Marketing Manager").await;
    let marketing_id: Uuid = marketing.parse().unwrap();
    let finance = h.app.create_bot("Finance").await;
    let mut tx = h.app.db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    else_api::db::memory::append(
        &mut tx,
        marketing_id,
        &else_api::db::memory::NewEntry {
            request_key: "budget",
            kind: else_api::db::memory::Kind::OwnerMessage,
            body: "The ad budget for Q4 is 40k.",
            source: json!({}),
            supersedes_seq: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    common::compact(&h.app.db.pool).await.unwrap();
    let account = account(&h.app).await;
    store::set_permissions(&h.app.db.pool, account.id, inbox, &permissions())
        .await
        .unwrap();
    let turn = mail_event(
        &h,
        &account,
        &format!("INBOX {GATE_EVERY_ATTEMPT}"),
        "Pass X Ads emails to the Marketing Manager bot.",
    )
    .await;
    let (_, token) = h.started(turn).await;
    let registry = &h.inputs(turn).await[0].delegation.registry;
    assert!(
        registry.iter().any(|(id, _)| *id == marketing_id),
        "{registry:?}"
    );
    let with_files = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": marketing, "task": "Read the notes.",
                "title": "X Ads notes", "files": ["notes.md"] }),
    )
    .await;
    assert!(with_files.error, "{}", with_files.body);
    let council = tool(
        &h.app,
        &token,
        "council",
        json!({ "target_bot_ids": [marketing, finance], "task": "Review this ad.",
                "title": "X Ads review" }),
    )
    .await;
    assert!(
        council.error && council.body.to_string().contains("COUNCIL_REFUSED"),
        "{}",
        council.body
    );
    let pending = tool(
        &h.app,
        &token,
        "delegate",
        json!({ "target_bot_id": marketing,
                "task": format!("{GATE} X Ads: your October invoice is ready."),
                "title": "X Ads invoice notice" }),
    )
    .await;
    assert_eq!(pending.result["status"], "pending", "{}", pending.body);
    let child: Uuid =
        sqlx::query_scalar("SELECT child_turn_id FROM delegations WHERE root_turn_id = ?1")
            .bind(turn)
            .fetch_one(&h.app.db.pool)
            .await
            .unwrap();
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
    let (_, child_token) = h.started(child).await;
    let grant = h.app.state.grants.resolve(&child_token).await.unwrap();
    assert!(grant.allows(Tool::ReadHistory));
    for forbidden in [
        Tool::ReadMemory,
        Tool::Mail,
        Tool::DraftEmail,
        Tool::Delegate,
        Tool::Computer,
        Tool::Mcp,
    ] {
        assert!(!grant.allows(forbidden), "{forbidden:?}");
    }
    let input = &h.inputs(child).await[0];
    assert_eq!(
        input.delegation.origin,
        else_api::app::providers::Origin::ConnectorEvent
    );
    assert!(input.memory.is_none(), "{:?}", input.memory);
    let onward = tool(
        &h.app,
        &child_token,
        "delegate",
        json!({ "target_bot_id": finance, "task": "Pay the X Ads invoice.", "title": "Pay" }),
    )
    .await;
    assert!(
        onward.error && onward.body.to_string().contains("handed on from email"),
        "{}",
        onward.body
    );
    h.release(child).await;
    h.app.wait_turn(child, &["completed"]).await;
    h.continued(turn, &token).await;
    let inputs = h.inputs(turn).await;
    let outcomes = &inputs.last().unwrap().delegation.outcomes;
    assert!(
        outcomes.iter().any(|o| o.target_bot_id == marketing_id
            && o.status == else_api::db::delegations::Status::Completed
            && o.result.is_some()),
        "{outcomes:?}"
    );
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
}

#[tokio::test]
async fn a_mail_event_drafts_a_reply_the_owner_approves_in_a_card() {
    let transport = Arc::new(Transport {
        sends: AtomicUsize::new(0),
        uncertain: false,
        recovers: None,
    });
    let h = recorder::harness(common::TestApp::builder().mail(transport.clone())).await;
    let bot = h.app.main_bot().await;
    let account = account(&h.app).await;
    store::set_permissions(
        &h.app.db.pool,
        account.id,
        bot.parse().unwrap(),
        &permissions(),
    )
    .await
    .unwrap();
    let folder = format!("INBOX {GATE_EVERY_ATTEMPT}");
    let turn = mail_event(
        &h,
        &account,
        &folder,
        "Draft replies to support mail for my review.",
    )
    .await;
    let (_, token) = h.started(turn).await;

    let message = json!({ "folder": folder, "uid_validity": 1, "uid": 9 });
    for command in [
        json!({ "operation": "draft", "draft": { "to": ["customer@example.com"],
                "subject": "Re: Support", "body": "Thanks.", "reply_to": message } }),
        json!({ "operation": "reply", "message": message, "body": "Thanks." }),
        json!({ "operation": "reply", "message": message, "body": "Thanks.", "send": false }),
        json!({ "operation": "forward", "message": message, "to": ["team@example.com"],
                "body": "FYI" }),
    ] {
        let saved = tool(
            &h.app,
            &token,
            "mail_action",
            json!({ "account_id": account.id, "action_id": Uuid::new_v4(), "command": command }),
        )
        .await;
        assert!(
            saved.error && saved.body.to_string().contains("draft_email"),
            "a mailbox draft is refused toward the card: {}",
            saved.body
        );
    }
    assert_eq!(
        transport.sends.load(Ordering::SeqCst),
        0,
        "no mailbox draft was saved"
    );

    let reply = json!({ "account_id": account.id,
                        "reply_to": { "folder": folder, "uid_validity": 1, "uid": 9 },
                        "to": ["customer@example.com"], "subject": "Re: Support",
                        "body": "Thanks, we are looking into it." });
    let drafted = tool(&h.app, &token, "draft_email", reply).await;
    drafted.ok();
    assert_eq!(drafted.result["status"], "pending", "{}", drafted.body);
    let id = drafted.result["draft"]["draft_id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;

    let snapshot = h
        .app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let card = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .expect("the mail turn is in the owner's chat")["outgoing_drafts"][0]
        .clone();
    assert_eq!(card["id"], id, "{card}");
    assert_eq!(card["status"], "pending");
    assert_eq!(card["content"]["from"], "bot@example.com");
    assert_eq!(
        transport.sends.load(Ordering::SeqCst),
        0,
        "nothing sent before approval"
    );

    let approved = h
        .app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/outgoing-drafts/{id}/approve"),
            Some(json!({ "version": 1 })),
        )
        .await;
    assert_eq!(approved.status, StatusCode::OK, "{}", approved.body);
    let token = h.continued(turn, &token).await;
    for (name, args) in [
        ("ask_owner", json!({})),
        ("confirm_draft_approval", json!({ "draft_id": id })),
        ("draft_message", json!({ "platform": "x", "body": "Hi" })),
    ] {
        let refused = tool(&h.app, &token, name, args).await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{name}: {}",
            refused.body
        );
    }
    let sent = tool(&h.app, &token, "send_draft", json!({ "draft_id": id })).await;
    assert_eq!(sent.result["status"], "sent", "{}", sent.body);
    assert_eq!(transport.sends.load(Ordering::SeqCst), 1);
    h.release(turn).await;
    h.app.wait_turn(turn, &["completed"]).await;
}
