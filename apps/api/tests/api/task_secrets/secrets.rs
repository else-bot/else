//! Task secrets: storage, delivery to the guest, and upgrades.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn secrets_are_write_only_encrypted_and_shared_by_every_bot() {
    let Setup { h, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Mailer").await;
    let other = app.create_bot("Other").await;
    let path = "/v1/task-secrets/smtp";
    let body = json!({ "env": { "SMTP_USER": "bot", "SMTP_PASSWORD": PASSWORD } });
    let created = app.owner("PUT", path, Some(body.clone())).await;
    assert_eq!(created.status, StatusCode::CREATED, "{:?}", created.body);
    assert_eq!(
        app.owner("PUT", path, Some(body)).await.status,
        StatusCode::OK
    );
    let listed = app.owner("GET", "/v1/task-secrets", None).await;
    assert_eq!(listed.body["secrets"][0]["name"], "smtp");
    assert_eq!(
        listed.body["secrets"][0]["env"],
        json!(["SMTP_PASSWORD", "SMTP_USER"])
    );
    assert!(!listed.body.to_string().contains(PASSWORD));
    let stored: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM task_secrets")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&stored).contains(PASSWORD));
    assert!(stored.starts_with(b"age-encryption.org/v1"));
    let (_, other_token) = h.running(&other, GATE).await;
    assert_eq!(
        tool(app, &other_token, "computer_secrets", json!({}))
            .await
            .result["secrets"],
        json!([{ "name": "smtp", "description": "", "website": null,
                 "env": ["SMTP_PASSWORD", "SMTP_USER"], "last_used_at": null }]),
        "another bot sees the same secret"
    );
    for env in [
        json!({ "PATH": "/tmp" }),
        json!({ "LD_PRELOAD": "x" }),
        json!({}),
    ] {
        let refused = app
            .owner("PUT", "/v1/task-secrets/bad", Some(json!({ "env": env })))
            .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST, "{env}");
    }
    let (_, token) = h.running(&bot, GATE).await;
    let as_task = app
        .call("GET", "/v1/task-secrets", None, Some(&token))
        .await;
    assert_eq!(as_task.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.owner("DELETE", path, None).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.owner("DELETE", path, None).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn secrets_reach_the_guest_only_on_stdin_and_are_redacted_from_output() {
    let Setup { h, guest, .. } = setup().await;
    let app = &h.app;
    let bot = app.create_bot("Mailer").await;
    let put = app
        .owner(
            "PUT",
            "/v1/task-secrets/smtp",
            Some(json!({ "env": { "SMTP_PASSWORD": PASSWORD } })),
        )
        .await;
    assert_eq!(put.status, StatusCode::CREATED);
    let (_, token) = h.running(&bot, GATE).await;

    let listed = tool(app, &token, "computer_secrets", json!({}))
        .await
        .result;
    assert_eq!(listed["secrets"][0]["env"], json!(["SMTP_PASSWORD"]));
    let unknown = tool(
        app,
        &token,
        "computer_run",
        json!({ "command": "true", "secrets": ["missing"], "effect": "local" }),
    )
    .await
    .result;
    assert!(
        unknown.as_str().unwrap().starts_with("SECRET_UNKNOWN"),
        "{unknown}"
    );

    let ran = tool(
        app,
        &token,
        "computer_run",
        json!({ "command": "./send.sh", "secrets": ["smtp"], "effect": "local",
                "purpose": "Send the report" }),
    )
    .await
    .result;
    assert_eq!(
        ran["stdout"], "SMTP_PASSWORD=[secret SMTP_PASSWORD]",
        "{ran}"
    );
    let execs = guest.execs();
    let Exec {
        program,
        args,
        input,
        ..
    } = execs
        .iter()
        .find(|Exec { args, .. }| args.iter().any(|a| a == "./send.sh"))
        .unwrap();
    assert_eq!(program, "/usr/sbin/runuser");
    assert!(
        args.iter().all(|a| !a.contains(PASSWORD)),
        "never an argument"
    );
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(PASSWORD);
    assert_eq!(
        String::from_utf8_lossy(input),
        format!("SMTP_PASSWORD {encoded}\n")
    );

    let job = tool(
        app,
        &token,
        "computer_job_start",
        json!({ "command": "./send.sh", "secrets": ["smtp"], "effect": "local",
                "purpose": "Send the report" }),
    )
    .await
    .result;
    let id: Uuid = job["job_id"].as_str().unwrap().parse().unwrap();
    let row: Value = sqlx::query_scalar("SELECT json_object('command', command, 'output_tail', output_tail, 'error', error, 'secrets', json(secrets)) FROM guest_jobs WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(row["secrets"], json!(["smtp"]));
    assert!(!row.to_string().contains(PASSWORD), "names only");
    let launch = guest
        .execs()
        .into_iter()
        .find(|Exec { args, .. }| args.iter().any(|a| a.contains("echo launched")))
        .unwrap();
    assert!(launch.args.iter().all(|a| !a.contains(PASSWORD)));
    assert!(String::from_utf8_lossy(&launch.input).contains(&encoded));
    let failed = tool(
        app,
        &token,
        "computer_job_start",
        json!({ "command": "LAUNCH_ERROR", "secrets": ["smtp"], "effect": "local",
                "purpose": "Send the report" }),
    )
    .await
    .result;
    let id: Uuid = failed["job_id"].as_str().unwrap().parse().unwrap();
    let error: String = sqlx::query_scalar("SELECT error FROM guest_jobs WHERE id = ?1")
        .bind(id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert!(error.contains("[secret SMTP_PASSWORD]"), "{error}");
    assert!(!error.contains(PASSWORD), "launch errors are masked");
    let logged: Vec<String> = sqlx::query_scalar("SELECT body FROM memory_entry WHERE bot_id = ?1")
        .bind(bot.parse::<Uuid>().unwrap())
        .fetch_all(&app.db.pool)
        .await
        .unwrap();
    assert!(logged.iter().any(|l| l.contains("\"secrets\":[\"smtp\"]")));
    assert!(logged.iter().all(|l| !l.contains(PASSWORD)));
}

#[tokio::test]
async fn upgrading_keeps_every_bots_secrets_under_distinct_shared_names() {
    let db = common::TestDb::empty().await;
    let old = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(&source).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_str().unwrap() < "0026" {
            std::fs::copy(entry.path(), old.path().join(entry.file_name())).unwrap();
        }
    }
    sqlx::migrate::Migrator::new(old.path())
        .await
        .unwrap()
        .run(&db.pool)
        .await
        .unwrap();
    let older = Uuid::parse_str("0badc0de-0000-4000-8000-000000000000").unwrap();
    let alike = Uuid::parse_str("0badc0de-1111-4000-8000-000000000000").unwrap();
    let newer = Uuid::new_v4();
    let long = "a".repeat(40);
    let (jan, feb, mar) = (
        "2026-01-01T00:00:00.000+00:00",
        "2026-02-01T00:00:00.000+00:00",
        "2026-03-01T00:00:00.000+00:00",
    );
    for (bot, bot_name, secret, updated) in [
        (older, "Older", "kaneo", jan),
        (alike, "Alike", "kaneo", feb),
        (newer, "Newer", "kaneo", mar),
        (newer, "Newer", "kaneo-0badc0de", mar),
        (older, "Older", long.as_str(), jan),
        (newer, "Newer", long.as_str(), mar),
        (older, "Older", "smtp", jan),
    ] {
        sqlx::query("INSERT OR IGNORE INTO bots (id, name) VALUES (?1, ?2)")
            .bind(bot)
            .bind(bot_name)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO task_secrets (bot_id, name, env_names, salt, ciphertext, updated_at)
             VALUES (?1, ?2, '[\"KANEO_API_KEY\"]', zeroblob(32), ?3, ?4)",
        )
        .bind(bot)
        .bind(secret)
        .bind(format!("{bot_name}:{secret}").into_bytes())
        .bind(updated)
        .execute(&db.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO bot_computers (bot_id, runtime_name, image, cpus, memory_mib)
         VALUES (?1, 'older', 'image', 1, 512)",
    )
    .bind(older)
    .execute(&db.pool)
    .await
    .unwrap();
    let job = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO guest_jobs (id, bot_id, command, deadline, status, secrets)
         VALUES (?1, ?2, './sync.sh', '2030-01-01T00:00:00Z', 'running', '[\"kaneo\",\"smtp\"]')",
    )
    .bind(job)
    .bind(older)
    .execute(&db.pool)
    .await
    .unwrap();

    else_api::db::migrate(&db.pool).await.unwrap();
    else_api::db::migrate(&db.pool).await.unwrap();

    let rows: Vec<(String, Vec<u8>)> =
        sqlx::query_as("SELECT name, ciphertext FROM task_secrets ORDER BY name")
            .fetch_all(&db.pool)
            .await
            .unwrap();
    let rows: Vec<(String, String)> = rows
        .into_iter()
        .map(|(n, c)| (n, String::from_utf8(c).unwrap()))
        .collect();
    let short = "a".repeat(31);
    let expected: Vec<(String, String)> = [
        (format!("{short}-0badc0de"), format!("Older:{long}")),
        (long.clone(), format!("Newer:{long}")),
        ("kaneo".into(), "Newer:kaneo".into()),
        ("kaneo-0badc0de".into(), "Newer:kaneo-0badc0de".into()),
        ("kaneo-0badc0de-2".into(), "Older:kaneo".into()),
        ("kaneo-0badc0de-3".into(), "Alike:kaneo".into()),
        ("smtp".into(), "Older:smtp".into()),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        rows, expected,
        "the newest copy keeps the name and no stored secret is lost"
    );
    let secrets: String = sqlx::query_scalar("SELECT secrets FROM guest_jobs WHERE id = ?1")
        .bind(job)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        secrets, r#"["kaneo-0badc0de-2","smtp"]"#,
        "a running job still names its own bot's copy"
    );
}
