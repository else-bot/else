//! Host task grants and launch secrets.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn task_grants_are_scoped_to_their_turn() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.main_bot().await;
    let other = app.create_bot("Other").await;
    let foreign = app
        .send(&other, Uuid::new_v4(), "foreign secret", Some("synthetic"))
        .await;
    app.wait_turn(foreign.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let foreign_item = foreign.body["item"]["id"].as_str().unwrap().to_owned();

    let (turn, token) = h.running(&bot, GATE).await;
    let history = app
        .call("GET", "/tools/v1/history", None, Some(&token))
        .await;
    assert_eq!(history.status, StatusCode::OK);
    let texts: Vec<_> = history.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["text"].clone())
        .collect();
    assert_eq!(texts, vec![json!(GATE)], "only its own conversation");
    let forged = app
        .call(
            "GET",
            &format!("/tools/v1/history?bot_id={other}"),
            None,
            Some(&token),
        )
        .await;
    assert_eq!(forged.status, StatusCode::BAD_REQUEST);
    let item = app
        .call(
            "GET",
            &format!("/tools/v1/items/{foreign_item}"),
            None,
            Some(&token),
        )
        .await;
    assert_eq!(
        item.status,
        StatusCode::NOT_FOUND,
        "foreign objects look missing"
    );
    let owner = app.call("GET", "/v1/bots", None, Some(&token)).await;
    assert_eq!(
        (owner.status, owner.body["error"]["code"].clone()),
        (StatusCode::FORBIDDEN, json!("task_grant_forbidden"))
    );
    let create = app
        .call(
            "POST",
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "x" })),
            Some(&token),
        )
        .await;
    assert_eq!(create.status, StatusCode::FORBIDDEN);
    let settings = app
        .call(
            "PATCH",
            "/v1/settings/providers/codex",
            Some(json!({ "reasoning_effort": "high" })),
            Some(&token),
        )
        .await;
    assert_eq!(
        settings.status,
        StatusCode::FORBIDDEN,
        "task grants cannot update settings"
    );
    let with_owner = app
        .call("GET", "/tools/v1/history", None, Some(common::TOKEN))
        .await;
    assert_eq!(with_owner.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, Some("tg_forged"))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    let narrow = h.grant_with(turn, vec![]).await;
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, Some(&narrow))
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    let stop_request = Uuid::new_v4();
    app.owner(
        "POST",
        &format!("/v1/bots/{bot}/cancel"),
        Some(json!({ "request_id": stop_request })),
    )
    .await;
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, Some(&token))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    app.wait_turn(turn, &["interrupted"]).await;
    let conversation = h.inputs(turn).await[0].conversation_id;
    let stale = app
        .state
        .grants
        .issue(
            bot.parse().unwrap(),
            conversation,
            turn,
            vec![Tool::ReadHistory],
            DEFAULT_TTL,
        )
        .await;
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, Some(&stale))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    let (_, newer_grant) = h.running(&bot, GATE).await;
    let retry = app
        .owner(
            "POST",
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": stop_request })),
        )
        .await;
    assert_eq!(retry.body["state"], "done");
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, Some(&newer_grant))
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_a_bot_revokes_its_grants() {
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let bot = app.create_bot("Temp").await;
    let (_, token) = h.running(&bot, GATE).await;
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, Some(&token))
            .await
            .status,
        StatusCode::OK
    );
    app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(
        app.call("GET", "/tools/v1/history", None, Some(&token))
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn launch_secrets_stay_off_host_command_lines() {
    let fx = fixture();
    let mut spec = shell(&fx, Uuid::new_v4(), "echo \"$GRANT_CANARY\"; read release");
    spec.env
        .push(("GRANT_CANARY".into(), "tg_canary_secret".into()));
    let mut command = fx.host.command(&spec);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    let mut process = Supervised::spawn(command).unwrap();
    let mut stdout = tokio::io::BufReader::new(process.child.stdout.take().unwrap());
    let mut seen = String::new();
    common::until(
        "the provider to read the secret",
        tokio::io::AsyncBufReadExt::read_line(&mut stdout, &mut seen),
    )
    .await
    .unwrap();
    assert!(!fx.host.live_processes(spec.turn_id).is_empty());
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        if let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) {
            let text = String::from_utf8_lossy(&cmdline);
            assert!(!text.contains("tg_canary_secret"), "{text}");
        }
    }
    assert_eq!(
        seen.trim(),
        "tg_canary_secret",
        "the value reached the provider"
    );
}
