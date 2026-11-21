//! Choice and open questions a turn asks the owner.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_choice_parks_the_turn_and_the_answer_continues_it() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let asked = tool(app, &token, "ask_owner", choice_args()).await;
    let result = asked.result;
    assert_eq!(result["status"], "pending", "{}", asked.body);
    assert_eq!(result["parked"], true);
    let question: Uuid = result["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(result["choices"][1]["id"], "b");
    let more = tool(app, &token, "memory_bots", json!({})).await;
    assert_eq!(more.status, StatusCode::CONFLICT, "{}", more.body);
    assert_eq!(more.body["error"]["code"], "attempt_parked");

    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let shown = shown_turn(app, &main, turn).await;
    let card = &shown["questions"][0];
    assert_eq!(card["id"], question.to_string());
    assert_eq!(card["kind"], "choice");
    assert_eq!(card["status"], "pending");
    assert_eq!(card["title"], "Which template for the newsletter?");
    assert_eq!(card["context"], "Draft: Hello everyone, this month...");
    assert_eq!(card["choices"][0]["label"], "Plain text");
    assert_eq!(card["answer"], Value::Null);
    assert_eq!(journaled(app, &main, question).await["status"], "pending");
    assert_eq!(
        sidebar_status(app, &main).await,
        json!({ "kind": "waiting_for_answer", "preview": "Which template for the newsletter?" })
    );

    let bad = answer(app, &main, question, "c").await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
    assert_eq!(bad.body["error"]["code"], "invalid_choice");
    let chosen = answer(app, &main, question, "b").await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    assert_eq!(chosen.body["question"]["status"], "answered");
    assert_eq!(chosen.body["question"]["answer"], "b");
    assert_eq!(chosen.body["requeued"], true);
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(sidebar_status(app, &main).await, Value::Null);
    let shown = shown_turn(app, &main, turn).await;
    assert_eq!(shown["questions"][0]["status"], "answered");
    assert_eq!(journaled(app, &main, question).await["answer"], "b");
    let (_, _, consumed) = question_row(app, question).await;
    assert_eq!(consumed, None, "a choice is not an action approval");

    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("[else.bot owner answers]"), "{packet}");
    assert!(packet.contains("The owner chose: HTML digest"), "{packet}");
    assert!(packet.contains("kind=\"choice\""), "{packet}");

    let again = answer(app, &main, question, "b").await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["requeued"], false);
    let stale = answer(app, &main, question, "a").await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "question_settled");
    assert_eq!(question_row(app, question).await.1, Some("b".into()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_turns_unread_when_its_turn_parks_on_a_question() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_AFTER_OPENING).await;
    let read_state = || async {
        app.owner("GET", &format!("/v1/bots/{main}"), None)
            .await
            .body["bot"]["read_state"]
            .clone()
    };
    tokio::time::timeout(common::SETTLE, async {
        loop {
            let snapshot = app
                .owner("GET", &format!("/v1/bots/{main}/conversation"), None)
                .await;
            let items = snapshot.body["items"].as_array().unwrap().clone();
            if items.iter().any(|i| i["kind"] == "assistant") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the reply started");
    let working = read_state().await;
    assert_eq!(working["latest"], Value::Null, "{working}");
    assert_eq!(working["unread"], false);

    let asked = tool(app, &token, "ask_owner", choice_args()).await;
    assert_eq!(asked.result["parked"], true, "{}", asked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let parked = read_state().await;
    assert_eq!(parked["latest"]["turn_id"], turn.to_string(), "{parked}");
    assert_eq!(parked["unread"], true);
    assert_eq!(parked["unread_replies"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn five_options_accept_exactly_one_answer_by_default_or_explicitly() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    for multiple in [None, Some(false)] {
        let (turn, token) = h.running(&main, GATE).await;
        let mut args = json!({
            "question": "What should I prepare for the changelog?",
            "choices": ["Skip posting", "Prepare a Discord + Telegram draft",
                "Prepare a Discord + Telegram + X draft", "Edit the draft first", "Something else"],
        });
        if let Some(multiple) = multiple {
            args["multiple"] = json!(multiple);
        }
        let pending = tool(app, &token, "ask_owner", args).await.result;
        let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
        assert_eq!(pending["multiple"], false);
        h.release(turn).await;
        app.wait_turn(turn, &["waiting"]).await;
        let shown = shown_turn(app, &main, turn).await;
        let card = &shown["questions"][0];
        assert_eq!(card["multiple"], false);
        assert_eq!(card["choices"].as_array().unwrap().len(), 5);
        assert_eq!(
            card["choices"][3],
            json!({ "id": "d", "label": "Edit the draft first" })
        );
        assert_eq!(
            card["choices"][4],
            json!({ "id": "e", "label": "Something else" })
        );

        for invalid in [json!(["d"]), json!(["a", "d"])] {
            let refused = select(app, &main, question, invalid).await;
            assert_eq!(refused.status, StatusCode::BAD_REQUEST);
            assert_eq!(refused.body["error"]["code"], "invalid_choice");
        }
        assert_eq!(status(app, turn).await, "waiting");
        let chosen = answer(app, &main, question, "d").await;
        assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
        assert_eq!(chosen.body["question"]["answer"], "d");
        assert_eq!(chosen.body["requeued"], true);
        app.wait_turn(turn, &["completed"]).await;
        let shown = shown_turn(app, &main, turn).await;
        assert_eq!(shown["questions"][0]["status"], "answered");
        assert_eq!(shown["questions"][0]["answer"], "d");
        assert_eq!(journaled(app, &main, question).await["answer"], "d");
        let packet = h.packets().await.last().unwrap().clone();
        assert!(
            packet.contains("The owner chose: Edit the draft first"),
            "{packet}"
        );
        assert_eq!(
            answer(app, &main, question, "a").await.status,
            StatusCode::CONFLICT
        );
        assert_eq!(question_row(app, question).await.1, Some("d".into()));
    }
}

fn multiple_args() -> Value {
    json!({
        "question": "Which inbox tasks should I take care of?",
        "choices": ["Archive cold recruiter emails", "Unsubscribe from newsletters",
                    "Label receipts and invoices", "Draft replies to starred emails",
                    "Summarize unread threads"],
        "multiple": true,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn multiple_choices_wait_for_done_and_deliver_only_selected_actions() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "ask_owner", multiple_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(pending["multiple"], true);
    assert_eq!(pending["choices"].as_array().unwrap().len(), 5);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let shown = shown_turn(app, &main, turn).await;
    assert_eq!(shown["questions"][0]["multiple"], true);
    assert_eq!(shown["questions"][0]["answer"], Value::Null);
    assert_eq!(journaled(app, &main, question).await["multiple"], true);

    for invalid in [
        json!([]),
        json!(["a", "a"]),
        json!(["f"]),
        json!(["a", "b", "c", "d", "e", "f"]),
    ] {
        let refused = select(app, &main, question, invalid).await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{}", refused.body);
    }
    assert_eq!(
        answer(app, &main, question, "a").await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(status(app, turn).await, "waiting");
    let chosen = select(app, &main, question, json!(["e", "c"])).await;
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    assert_eq!(chosen.body["question"]["answer"], json!(["c", "e"]));
    assert_eq!(chosen.body["requeued"], true);
    app.wait_turn(turn, &["completed"]).await;
    let card = &shown_turn(app, &main, turn).await["questions"][0];
    assert_eq!(card["status"], "answered");
    assert_eq!(card["answer"], json!(["c", "e"]));
    assert_eq!(
        journaled(app, &main, question).await["answer"],
        json!(["c", "e"])
    );
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains("Label receipts and invoices"), "{packet}");
    assert!(packet.contains("Summarize unread threads"), "{packet}");
    assert!(
        !packet.contains("Archive cold recruiter emails"),
        "{packet}"
    );
    assert!(!packet.contains("Unsubscribe from newsletters"), "{packet}");
    assert!(
        !packet.contains("Draft replies to starred emails"),
        "{packet}"
    );
    assert_eq!(permissions(app, &main).await, 0);
    let again = select(app, &main, question, json!(["e", "c"])).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body["requeued"], false);
    let stale = select(app, &main, question, json!(["a"])).await;
    assert_eq!(stale.status, StatusCode::CONFLICT);
    assert_eq!(stale.body["error"]["code"], "question_settled");
}

#[tokio::test(flavor = "multi_thread")]
async fn chat_closes_a_multiple_choice_card_without_selecting_actions() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "ask_owner", multiple_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let sent = app
        .send(
            &main,
            Uuid::new_v4(),
            "Only summarize unread threads.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED);
    let stale = select(app, &main, question, json!(["a", "b"])).await;
    assert_eq!(stale.status, StatusCode::CONFLICT, "{}", stale.body);
    assert_eq!(stale.body["error"]["code"], "question_settled");
    let shown = shown_turn(app, &main, turn).await;
    assert_eq!(shown["questions"][0]["status"], "skipped");
    assert_eq!(shown["questions"][0]["answer"], Value::Null);
    assert_eq!(permissions(app, &main).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn asking_the_same_question_again_returns_it_and_other_shapes_conflict() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    for bad in [
        json!({ "question": "q", "choices": ["only one"] }),
        json!({ "question": "q", "choices": ["a", "b"], "extra": 1 }),
    ] {
        let refused = tool(app, &token, "ask_owner", bad).await;
        assert!(
            refused
                .result
                .as_str()
                .unwrap()
                .starts_with("INVALID_QUESTION"),
            "{}",
            refused.body
        );
    }
    let first = tool(app, &token, "ask_owner", choice_args()).await.result;
    assert_eq!(first["status"], "pending", "{first}");
    let parked = tool(app, &token, "ask_owner", choice_args()).await;
    assert_eq!(parked.status, StatusCode::CONFLICT, "{}", parked.body);
    use else_api::app::questions::{Spec, Status};
    use else_api::db::owner_questions::{RequestError, create};

    let again = create(&app.db.pool, turn, &Spec::choice(&choice_args()).unwrap())
        .await
        .unwrap();
    assert_eq!(again.id.to_string(), first["question_id"]);
    assert_eq!(again.status, Status::Pending);
    let mut multiple = choice_args();
    multiple["multiple"] = json!(true);
    let mut other = choice_args();
    other["choices"] = json!(["Plain text", "Both"]);
    for changed in [multiple, other] {
        let spec = Spec::choice(&changed).unwrap();
        assert_eq!(
            create(&app.db.pool, turn, &spec).await.unwrap_err(),
            RequestError::Conflict
        );
    }
    let cards: i64 = sqlx::query_scalar("SELECT count(*) FROM owner_questions")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(cards, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_follow_up_skips_the_card_and_grants_nothing() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "computer_run", delete_args())
        .await
        .result;
    assert_eq!(pending["status"], "pending", "{pending}");
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    let follow_up = "Actually, keep the old site for now and just archive it.";
    let sent = app
        .send(&main, Uuid::new_v4(), follow_up, Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], turn.to_string());
    let (row_status, row_answer, consumed) = question_row(app, question).await;
    assert_eq!(
        (row_status.as_str(), row_answer, consumed),
        ("skipped", None, None)
    );
    assert_eq!(permissions(app, &main).await, 0);
    assert_eq!(journaled(app, &main, question).await["status"], "skipped");

    let late = answer(app, &main, question, "approve").await;
    assert_eq!(late.status, StatusCode::CONFLICT, "{}", late.body);
    assert_eq!(
        late.body["error"]["message"],
        "This question was already skipped."
    );
    assert_eq!(question_row(app, question).await.0, "skipped");
    app.wait_turn(turn, &["completed"]).await;
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    assert_eq!(attempts(app, turn).await, 2, "the reply continued with it");
    let packet = h.packets().await.last().unwrap().clone();
    assert!(packet.contains(follow_up));
    assert!(
        packet.contains(&format!(
            "id=\"{question}\" kind=\"destructive\" status=\"skipped\""
        )) && packet.contains("Not approved"),
        "the reply learns its action was not approved: {packet}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_follow_up_while_the_asking_attempt_still_runs_skips_the_card_too() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(app, &token, "ask_owner", choice_args()).await.result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(status(app, turn).await, "running");
    let sent = app
        .send(&main, Uuid::new_v4(), "Use plain text.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED, "{}", sent.body);
    assert_eq!(sent.body["turn"]["steered_into"], turn.to_string());
    assert_eq!(question_row(app, question).await.0, "skipped");
    h.release(turn).await;
    app.wait_turn(turn, &["completed"]).await;
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    assert_eq!(attempts(app, turn).await, 2, "the reply continued with it");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_open_question_uses_a_card_and_chat_supersedes_it_without_approval() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    let pending = tool(
        app,
        &token,
        "ask_owner",
        json!({ "question": "Who should receive the draft?" }),
    )
    .await
    .result;
    let question: Uuid = pending["question_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(pending["status"], "pending");
    let shown = shown_turn(app, &main, turn).await;
    assert_eq!(
        shown["questions"][0]["title"],
        "Who should receive the draft?"
    );
    assert_eq!(shown["questions"][0]["choices"], json!([]));
    let sent = app
        .send(
            &main,
            Uuid::new_v4(),
            "Prepare it for Alice, but do not send it.",
            Some("synthetic"),
        )
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED);
    assert_eq!(question_row(app, question).await.0, "skipped");
    assert_eq!(permissions(app, &main).await, 0);
    assert_eq!(
        answer(app, &main, question, "approve").await.status,
        StatusCode::BAD_REQUEST
    );
}
