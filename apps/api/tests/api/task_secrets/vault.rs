//! The owner's login vault.

use super::*;

async fn put_login(app: &TestApp, name: &str, website: Option<&str>) {
    let mut body = json!({ "env": { "USERNAME": "me@example.com", "PASSWORD": PASSWORD },
                           "description": "X account @example" });
    if let Some(website) = website {
        body["website"] = json!(website);
    }
    let put = app
        .owner("PUT", &format!("/v1/task-secrets/{name}"), Some(body))
        .await;
    assert_eq!(put.status, StatusCode::CREATED, "{:?}", put.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_vault_is_searched_edited_and_revealed_to_the_owner_only() {
    let Setup { h, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Poster").await;
    put_login(app, "x-login", Some("https://www.x.com/home")).await;
    let (_, token) = h.running(&bot, GATE).await;

    let found = tool(
        app,
        &token,
        "computer_secrets",
        json!({ "query": "https://mobile.x.com/i/flow/login" }),
    )
    .await
    .result;
    assert_eq!(found["secrets"][0]["name"], "x-login", "{found}");
    assert_eq!(found["secrets"][0]["website"], "x.com");
    assert!(!found.to_string().contains(PASSWORD));
    let by_words = tool(
        app,
        &token,
        "computer_secrets",
        json!({ "query": "X account" }),
    )
    .await
    .result;
    assert_eq!(by_words["secrets"].as_array().unwrap().len(), 1);
    let none = tool(
        app,
        &token,
        "computer_secrets",
        json!({ "query": "gmail.com" }),
    )
    .await
    .result;
    assert_eq!(none["secrets"], json!([]));
    assert!(none["next"].as_str().unwrap().contains("request_secret"));

    let path = "/v1/task-secrets/x-login";
    let edited = app
        .owner(
            "PATCH",
            path,
            Some(
                json!({ "description": "X, @example", "set": { "PASSWORD": "n3w-Pa55word",
                         "TOTP_SECRET": "JBSWY3DPEHPK3PXP" }, "remove": ["USERNAME"] }),
            ),
        )
        .await;
    assert_eq!(edited.status, StatusCode::OK, "{:?}", edited.body);
    assert_eq!(edited.body["description"], "X, @example");
    assert_eq!(edited.body["website"], "x.com", "an absent website is kept");
    assert_eq!(edited.body["env"], json!(["PASSWORD", "TOTP_SECRET"]));
    assert!(!edited.body.to_string().contains("n3w-Pa55word"));
    let cleared = app
        .owner("PATCH", path, Some(json!({ "website": null })))
        .await;
    assert_eq!(cleared.body["website"], Value::Null);

    let revealed = app.owner("POST", &format!("{path}/reveal"), None).await;
    assert_eq!(revealed.status, StatusCode::OK);
    assert_eq!(
        revealed.body["env"],
        json!({ "PASSWORD": "n3w-Pa55word", "TOTP_SECRET": "JBSWY3DPEHPK3PXP" })
    );
    let as_bot = app
        .call("POST", &format!("{path}/reveal"), None, Some(&token))
        .await;
    assert_eq!(as_bot.status, StatusCode::FORBIDDEN, "bots never reveal");

    for (body, status) in [
        (
            json!({ "remove": ["PASSWORD", "TOTP_SECRET"] }),
            StatusCode::BAD_REQUEST,
        ),
        (json!({ "remove": ["NOPE"] }), StatusCode::BAD_REQUEST),
        (json!({ "website": "not a site" }), StatusCode::BAD_REQUEST),
        (
            json!({ "description": "x".repeat(201) }),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({ "set": { "PATH": "/tmp" } }),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let reply = app.owner("PATCH", path, Some(body.clone())).await;
        assert_eq!(reply.status, status, "{body}: {:?}", reply.body);
    }
    let taken = app
        .owner(
            "POST",
            path,
            Some(json!({ "env": { "PASSWORD": "other-Pa55word" } })),
        )
        .await;
    assert_eq!(
        taken.status,
        StatusCode::CONFLICT,
        "adding never overwrites"
    );
    let added = app
        .owner(
            "POST",
            "/v1/task-secrets/gmail",
            Some(json!({ "env": { "PASSWORD": "gm-Pa55word" }, "website": "gmail.com" })),
        )
        .await;
    assert_eq!(added.status, StatusCode::CREATED, "{:?}", added.body);
    let missing = app
        .owner(
            "PATCH",
            "/v1/task-secrets/missing",
            Some(json!({ "description": "d" })),
        )
        .await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    let still = app.owner("POST", &format!("{path}/reveal"), None).await;
    assert_eq!(
        still.body["env"]["PASSWORD"], "n3w-Pa55word",
        "refused edits change nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vault_login_is_typed_from_stdin_only_into_its_website_and_never_shown() {
    let Setup { h, guest, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Poster").await;
    put_login(app, "x-login", Some("x.com")).await;
    put_login(app, "no-site", None).await;
    let (_, token) = h.running(&bot, GATE).await;
    let typed = |secret: &str, field: Option<&str>| {
        let mut args = json!({ "action": "type", "selector": "#password", "secret": secret,
                               "effect": "local" });
        if let Some(field) = field {
            args["field"] = json!(field);
        }
        args
    };

    let filled = tool(
        app,
        &token,
        "computer_browser",
        typed("x-login", Some("PASSWORD")),
    )
    .await;
    filled.ok();
    let result = filled.result;
    assert_eq!(
        result["added"],
        json!(["Signed in as [secret PASSWORD]"]),
        "{result}"
    );
    assert!(!result.to_string().contains(PASSWORD));
    let exec = guest
        .execs()
        .into_iter()
        .rfind(|e| e.args.iter().any(|a| a == "type-secret"))
        .expect("the guest helper ran");
    assert_eq!(exec.input, PASSWORD.as_bytes(), "the value goes on stdin");
    assert!(
        exec.args.iter().all(|a| !a.contains(PASSWORD)),
        "{:?}",
        exec.args
    );
    assert!(
        exec.args
            .ends_with(&["type-secret".into(), "#password".into(), "x.com".into()])
    );

    let later = tool(app, &token, "computer_browser", json!({ "action": "text" }))
        .await
        .result;
    assert_eq!(
        later["text"], "Signed in as [secret USERNAME] with [secret PASSWORD]",
        "a later read of the same site hides what was filled: {later}"
    );
    let echoed = tool(
        app,
        &token,
        "computer_browser",
        json!({ "action": "click", "selector": "#echoes", "effect": "local" }),
    )
    .await
    .result;
    assert!(
        !echoed.to_string().contains(PASSWORD),
        "an error without a page URL is redacted for what this bot filled: {echoed}"
    );
    let put = app
        .owner(
            "PUT",
            "/v1/task-secrets/pin",
            Some(json!({ "env": { "PIN": "123", "CODE": "4567" }, "website": "bank.example" })),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);

    for (args, code) in [
        (typed("pin", Some("PIN")), "SECRET_TOO_SHORT"),
        (typed("no-site", Some("PASSWORD")), "SECRET_NO_WEBSITE"),
        (typed("x-login", Some("TOTP")), "SECRET_FIELD"),
        (typed("x-login", None), "secret needs field"),
        (typed("missing", Some("PASSWORD")), "SECRET_UNKNOWN"),
    ] {
        let refused = tool(app, &token, "computer_browser", args).await.result;
        assert!(refused.as_str().unwrap().contains(code), "{refused}");
    }

    let listed = app.owner("GET", "/v1/task-secrets", None).await;
    let entry = listed.body["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "x-login")
        .unwrap()
        .clone();
    assert_eq!(entry["last_used"]["use"], "browser", "{entry}");
    assert_eq!(entry["last_used"]["bot_name"], "Poster");
    tool(
        app,
        &token,
        "computer_run",
        json!({ "command": "true", "secrets": ["x-login"], "effect": "local" }),
    )
    .await;
    let listed = app.owner("GET", "/v1/task-secrets", None).await;
    let entry = listed.body["secrets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "x-login")
        .unwrap()
        .clone();
    assert_eq!(entry["last_used"]["use"], "command", "{entry}");
    let mut conn = app.db.pool.acquire().await.unwrap();
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .fetch_all(&mut *conn)
    .await
    .unwrap();
    for table in tables {
        for row in common::rows(&mut conn, &format!("SELECT * FROM \"{table}\"")).await {
            assert!(!row.contains(PASSWORD), "{table} holds the value: {row}");
        }
    }
}
