//! Inbox sign-in cards a bot raises in chat.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_bot_asks_the_owner_to_sign_in_to_an_inbox_from_chat() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let (turn, token) = h
        .running(
            &bot,
            &format!("{GATE_EVERY_ATTEMPT} Handle my Gmail inbox owner@gmail.com"),
        )
        .await;
    let unsupported = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"yahoo","address":"owner@yahoo.com"}),
    )
    .await;
    assert!(unsupported.error, "{}", unsupported.body);
    assert!(!app.state.grants.is_parked(turn).await);

    let parked = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"gmail","address":" Owner@Gmail.com ","purpose":"Triage your inbox."}),
    )
    .await;
    assert_eq!(
        parked.result["status"], "pending_owner_auth",
        "{}",
        parked.body
    );
    assert_eq!(parked.result["parked"], true);
    assert!(app.state.grants.is_parked(turn).await);
    let question: Uuid = parked.result["question_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    let card = snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["questions"].as_array().unwrap())
        .find(|q| q["id"] == question.to_string())
        .unwrap()
        .clone();
    assert_eq!(
        card["action"]["email_connection"],
        json!({"provider":"gmail","address":"owner@gmail.com"})
    );
    assert!(
        card["context"]
            .as_str()
            .unwrap()
            .contains("New mail there will wake this bot."),
        "the card says the inbox wakes the bot: {card}"
    );
    assert!(
        card["context"]
            .as_str()
            .unwrap()
            .contains("Triage your inbox.")
    );

    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    let answer_path = format!("/v1/bots/{bot}/questions/{question}/answer");
    let approve = || app.owner("POST", &answer_path, Some(json!({"choice":"approve"})));
    let early = approve().await;
    assert_eq!(early.status, StatusCode::CONFLICT, "{}", early.body);
    assert_eq!(early.body["error"]["code"], "inbox_not_connected");
    let unrelated = account(app).await;
    let still_early = approve().await;
    assert_eq!(
        still_early.status,
        StatusCode::CONFLICT,
        "an IMAP inbox is not this sign-in"
    );
    let other_mailbox = store::create(
        &app.db.pool,
        &app.state.secret_key,
        &Provider::Gmail.config("other@gmail.com".into()).unwrap(),
        &Credential::OAuth2 {
            client_id: "desktop".into(),
            client_secret: None,
            refresh_token: "private-refresh".into(),
            access_token: None,
            expires_at: None,
        },
    )
    .await
    .unwrap();
    let answered = approve().await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let bot_id: Uuid = bot.parse().unwrap();
    let woken = |account| store::permissions(&app.db.pool, account, bot_id);
    assert!(
        woken(other_mailbox.id).await.unwrap().unwrap().wake,
        "the signed-in inbox wakes the bot that asked for it"
    );
    assert!(!woken(unrelated.id).await.unwrap().unwrap().wake);
    let triggers = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await
        .body["mail_triggers"]
        .clone();
    assert_eq!(
        triggers
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                (
                    t["address"].as_str().unwrap(),
                    t["wake"].as_bool().unwrap(),
                    t["instructions"].as_str().unwrap(),
                )
            })
            .collect::<Vec<_>>(),
        [("other@gmail.com", true, store::DEFAULT_WAKE_INSTRUCTIONS)],
        "the sign-in card turns on a trigger with default instructions"
    );
    let token = h.continued(turn, &token).await;
    let packet = h.packets().await.into_iter().last().unwrap();
    assert!(packet.contains("<email_connection"), "{packet}");
    assert!(packet.contains("Call mail_accounts"), "{packet}");
    let resumed = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"gmail","address":"owner@gmail.com"}),
    )
    .await;
    assert_eq!(resumed.result["status"], "connected", "{}", resumed.body);

    let known = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"microsoft","address":"BOT@example.com"}),
    )
    .await;
    assert_eq!(known.result["status"], "connected", "{}", known.body);
    assert_eq!(known.result["account_id"], unrelated.id.to_string());
    let chosen = tool(
        app,
        &token,
        "mail_connect",
        json!({"provider":"gmail","address":"other@gmail.com"}),
    )
    .await;
    assert_eq!(chosen.result["account_id"], other_mailbox.id.to_string());
    h.release(turn).await;
}

/// Parks a turn on an inbox card for owner@gmail.com and returns the bot and
/// the card's answer path once the turn waits.
async fn inbox_card(h: &recorder::Harness) -> (String, String) {
    let bot = h.app.main_bot().await;
    let (turn, token) = h
        .running(&bot, &format!("{GATE} Watch owner@gmail.com"))
        .await;
    let parked = tool(
        &h.app,
        &token,
        "mail_connect",
        json!({"provider":"gmail","address":"owner@gmail.com"}),
    )
    .await;
    let question = parked.result["question_id"].as_str().unwrap().to_owned();
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
    let path = format!("/v1/bots/{bot}/questions/{question}/answer");
    (bot, path)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inbox_card_wakes_no_bot_when_its_inbox_is_ambiguous() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let (bot, path) = inbox_card(&h).await;
    let mut ids = vec![];
    for address in ["first@gmail.com", "second@gmail.com"] {
        let (config, credential) = gmail(address);
        let key = &h.app.state.secret_key;
        ids.push(
            store::create(&h.app.db.pool, key, &config, &credential)
                .await
                .unwrap()
                .id,
        );
    }
    let answered = h
        .app
        .owner("POST", &path, Some(json!({"choice":"approve"})))
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    for id in ids {
        let p = store::permissions(&h.app.db.pool, id, bot.parse().unwrap())
            .await
            .unwrap();
        assert!(
            !p.unwrap().wake,
            "neither inbox is surely the one signed in"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_inbox_card_never_restores_read_the_owner_withdrew() {
    let h =
        recorder::harness(common::TestApp::builder().mail(Arc::new(QuietInbox::default()))).await;
    let (bot, path) = inbox_card(&h).await;
    let (config, credential) = gmail("owner@gmail.com");
    let key = &h.app.state.secret_key;
    let id = store::create(&h.app.db.pool, key, &config, &credential)
        .await
        .unwrap()
        .id;
    let bot_id: Uuid = bot.parse().unwrap();
    store::set_permissions(&h.app.db.pool, id, bot_id, &Permissions::default())
        .await
        .unwrap();
    let answered = h
        .app
        .owner("POST", &path, Some(json!({"choice":"approve"})))
        .await;
    assert_eq!(answered.status, StatusCode::OK, "{}", answered.body);
    let p = store::permissions(&h.app.db.pool, id, bot_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!p.read && !p.wake, "{p:?}");
}
