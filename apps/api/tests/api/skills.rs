use crate::common;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::Path;
use axum::http::StatusCode;
use axum::routing::get;
use common::{TestApp, TestDb};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::providers::synthetic::Synthetic;
use serde_json::{Value, json};
use uuid::Uuid;

const CANARY: &str = "CANARY-5k1ll";
const SLOW: &str = "[synthetic:slow] install the rea-growth skill for me.";
const COMMIT: &str = "a41c9e2f00000000000000000000000000000001";
const URL: &str = "https://github.com/acme/treg/tree/main/skills/rea-growth";
const SKILL_MD: &str = "---\nname: rea-growth\ndescription: Reverse engineer how a company grows\n  on each channel.\n---\n# REA growth\nRun scripts/teardown.py with TREG_TOKEN.\n";
const SCRIPT: &str = "#!/usr/bin/env python3\nimport os\nprint(os.environ['TREG_TOKEN'][:2])\n";

fn repo_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("skills/rea-growth/SKILL.md", SKILL_MD),
        ("skills/rea-growth/scripts/teardown.py", SCRIPT),
        ("skills/rea-growth/references/channels.md", "# Channels\n"),
        (
            "skills/other/SKILL.md",
            "---\nname: other\ndescription: d\n---\n",
        ),
        ("README.md", "# treg\n"),
        (
            "skills/big/SKILL.md",
            "---\nname: big\ndescription: A long reference.\n---\nRead big.md.\n",
        ),
        ("skills/big/big.md", BIG),
        (
            "skills/long/SKILL.md",
            "---\nname: long\ndescription: d\n---\n",
        ),
        (LONG_PATH, "x"),
    ]
}

/// About 85 KiB, longer than one skill_read page.
const BIG: &str = concat!(
    include_str!("../../src/skills.rs"),
    include_str!("../../src/skills.rs"),
    include_str!("../../src/skills.rs"),
    include_str!("../../src/skills.rs"),
    include_str!("../../src/skills.rs"),
);
const LONG_PATH: &str = concat!(
    "skills/long/",
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/",
    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc.md",
);

/// A local stand-in for GitHub's REST API and raw file host.
async fn fake_github() -> else_api::skills::Sources {
    let app = axum::Router::new()
        .route(
            "/repos/{owner}/{repo}/commits/{reference}",
            get(|Path((owner, repo, reference)): Path<(String, String, String)>| async move {
                if owner == "acme" && matches!(repo.as_str(), "treg" | "huge") && reference == "main" {
                    (StatusCode::OK, COMMIT.to_owned())
                } else {
                    (StatusCode::NOT_FOUND, String::new())
                }
            }),
        )
        .route(
            "/repos/{owner}/{repo}/git/trees/{sha}",
            get(|Path((_, repo, sha)): Path<(String, String, String)>| async move {
                assert_eq!(sha, COMMIT, "the tree is read at the pinned commit");
                let truncated = repo == "huge";
                let tree: Vec<Value> = repo_files()
                    .into_iter()
                    .map(|(path, body)| json!({ "path": path, "mode": "100644", "type": "blob", "size": body.len() }))
                    .chain([json!({ "path": "skills/rea-growth/scripts", "mode": "040000", "type": "tree" })])
                    .collect();
                axum::Json(json!({ "sha": sha, "tree": tree, "truncated": truncated }))
            }),
        )
        .route(
            "/raw/{owner}/{repo}/{sha}/{*path}",
            get(|Path((_, _, sha, path)): Path<(String, String, String, String)>| async move {
                assert_eq!(sha, COMMIT, "files are read at the pinned commit");
                match repo_files().into_iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => (StatusCode::OK, body.to_owned()),
                    None => (StatusCode::NOT_FOUND, String::new()),
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    else_api::skills::Sources {
        api: format!("http://{addr}/").parse().unwrap(),
        raw: format!("http://{addr}/raw/").parse().unwrap(),
    }
}

struct Harness {
    app: TestApp,
    synthetic: Arc<Synthetic>,
}

async fn harness() -> Harness {
    let db = Arc::new(TestDb::migrated().await);
    else_api::db::installation::setup(&db.pool).await.unwrap();
    let synthetic = Arc::new(Synthetic::new(Duration::from_millis(5)));
    let registry = else_api::providers::Registry::new(vec![synthetic.clone()]);
    Harness {
        app: TestApp::with_skill_sources(db, registry, fake_github().await),
        synthetic,
    }
}

fn tools() -> Vec<Tool> {
    vec![
        Tool::ReadHistory,
        Tool::ReadMemory,
        Tool::Mcp,
        Tool::ManageMcp,
    ]
}

async fn running(app: &TestApp, bot: &str) -> (Uuid, String) {
    let sent = app.send(bot, Uuid::new_v4(), SLOW, Some("synthetic")).await;
    let turn = sent.body["turn"]["id"].as_str().unwrap().to_owned();
    app.wait_turn(&turn, &["running"]).await;
    let turn_id: Uuid = turn.parse().unwrap();
    (turn_id, grant(app, turn_id).await)
}

async fn grant(app: &TestApp, turn_id: Uuid) -> String {
    let (owner, conversation): (Uuid, Uuid) =
        sqlx::query_as("SELECT bot_id, conversation_id FROM turns WHERE id = ?1")
            .bind(turn_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    app.scheduler
        .grants()
        .issue(owner, conversation, turn_id, tools(), DEFAULT_TTL)
        .await
}

async fn tool(app: &TestApp, token: &str, name: &str, args: Value) -> Value {
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": name, "arguments": args } })),
            Some(token),
        )
        .await;
    let result = &reply.body["result"];
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    let mut value = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
    if result["isError"] == true {
        value = json!({ "error": text });
    }
    value
}

async fn install(app: &TestApp, token: &str) -> Value {
    tool(
        app,
        token,
        "skill_install",
        json!({ "url": URL, "reason": "Run the Lovable teardown.", "env": ["TREG_TOKEN"] }),
    )
    .await
}

async fn wait(app: &TestApp, turn: Uuid, status: &str) {
    app.wait_turn_for(&turn.to_string(), &[status], Duration::from_secs(30))
        .await;
}

async fn shown_card(app: &TestApp, bot: &str, turn: Uuid) -> Value {
    let snapshot = app
        .owner("GET", &format!("/v1/bots/{bot}/conversation"), None)
        .await;
    snapshot.body["turns"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == turn.to_string())
        .expect("the turn is in the snapshot")["skill_installs"][0]
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approved_card_installs_the_reviewed_files_stores_the_key_and_continues() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;

    let pending = install(&app, &token).await;
    assert_eq!(pending["status"], "pending", "{pending}");
    assert_eq!(pending["parked"], true);
    assert_eq!(pending["commit"], COMMIT);
    let card_id = pending["request_id"].as_str().unwrap().to_owned();
    wait(&app, turn, "waiting").await;

    let card = shown_card(&app, &main, turn).await;
    assert_eq!(card["status"], "pending");
    assert_eq!(card["skill_id"], "rea-growth");
    assert_eq!(
        card["description"],
        "Reverse engineer how a company grows on each channel."
    );
    assert_eq!(card["source"]["repo"], "acme/treg");
    assert_eq!(card["source"]["path"], "skills/rea-growth");
    assert_eq!(card["source"]["commit"], COMMIT);
    assert_eq!(card["env"], json!(["TREG_TOKEN"]));
    assert_eq!(card["secret"], "skill-rea-growth");
    assert_eq!(
        card["files"],
        json!([
            { "path": "SKILL.md", "size": SKILL_MD.len(), "script": false },
            { "path": "references/channels.md", "size": 11, "script": false },
            { "path": "scripts/teardown.py", "size": SCRIPT.len(), "script": true },
        ])
    );
    assert!(
        card.get("contents").is_none(),
        "the turn payload carries no file text"
    );
    let listed = app.owner("GET", &format!("/v1/bots/{main}"), None).await;
    assert_eq!(
        {
            let mut status = listed.body["bot"]["sidebar_status"].clone();
            assert!(
                status
                    .as_object_mut()
                    .unwrap()
                    .remove("since")
                    .unwrap()
                    .is_string()
            );
            status
        },
        json!({ "kind": "needs_approval", "preview": "Install skill rea-growth" })
    );

    let review = app
        .owner(
            "GET",
            &format!("/v1/bots/{main}/skill-installs/{card_id}"),
            None,
        )
        .await;
    assert_eq!(review.status, StatusCode::OK, "{}", review.body);
    assert_eq!(review.body["contents"]["SKILL.md"], SKILL_MD);
    assert_eq!(review.body["contents"]["scripts/teardown.py"], SCRIPT);

    let approve = |body: Value| {
        let app = &app;
        let path = format!("/v1/bots/{main}/skill-installs/{card_id}/approve");
        async move { app.owner("POST", &path, Some(body)).await }
    };
    let mismatch = approve(json!({ "scope": "bot", "env": { "OTHER": CANARY } })).await;
    assert_eq!(
        mismatch.status,
        StatusCode::BAD_REQUEST,
        "{}",
        mismatch.body
    );
    assert_eq!(mismatch.body["error"]["code"], "env_mismatch");
    assert!(!mismatch.body.to_string().contains(CANARY));

    let done = approve(json!({ "scope": "bot", "env": { "TREG_TOKEN": CANARY } })).await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    assert_eq!(done.body["install"]["status"], "installed");
    assert_eq!(done.body["install"]["scope"], "bot");
    assert_eq!(done.body["install"]["key_stored"], true);
    assert_eq!(done.body["requeued"], true);
    assert!(!done.body.to_string().contains(CANARY));
    wait(&app, turn, "completed").await;

    let packet = synthetic.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("<skill id=\"rea-growth\" status=\"installed\" scope=\"this bot only\" key=\"stored as skill-rea-growth\"/>"),
        "{packet}"
    );
    assert!(packet.contains("[else.bot installed skills]"), "{packet}");
    assert!(
        packet.contains("- rea-growth: Reverse engineer how a company grows on each channel. [keys: task secret skill-rea-growth (TREG_TOKEN), set]"),
        "{packet}"
    );
    for packet in synthetic.packets().await {
        assert!(!packet.contains(CANARY), "a provider packet held the key");
    }
    let replay = approve(json!({ "scope": "all" })).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body["requeued"], false);
    assert_eq!(
        replay.body["install"]["scope"], "bot",
        "a retry changes nothing"
    );

    let env = else_api::db::task_secrets::resolve(
        &app.db.pool,
        app.state.secret_key.as_deref(),
        &["skill-rea-growth".to_owned()],
    )
    .await
    .unwrap();
    assert_eq!(env.vars.len(), 1);
    assert_eq!(
        (env.vars[0].0.as_str(), env.vars[0].1.as_str()),
        ("TREG_TOKEN", CANARY)
    );

    let skills = app.owner("GET", "/v1/settings/skills", None).await;
    let skill = &skills.body["skills"][0];
    assert_eq!(skill["id"], "rea-growth");
    assert_eq!(skill["scope"], "bot");
    assert_eq!(skill["bot_id"], main);
    assert_eq!(skill["installed_by"], main);
    assert_eq!(skill["key_stored"], true);
    assert_eq!(skill["files"].as_array().unwrap().len(), 3);
    let shown = app
        .owner("GET", "/v1/settings/skills/rea-growth", None)
        .await;
    assert_eq!(shown.body["contents"]["scripts/teardown.py"], SCRIPT);

    let (_, token) = running(&app, &main).await;
    let read = tool(&app, &token, "skill_read", json!({ "id": "rea-growth" })).await;
    assert_eq!(read["content"], SKILL_MD, "{read}");
    assert_eq!(
        read["keys"],
        json!({ "secret": "skill-rea-growth", "env": ["TREG_TOKEN"], "stored": true })
    );
    let script = tool(
        &app,
        &token,
        "skill_read",
        json!({ "id": "rea-growth", "path": "scripts/teardown.py" }),
    )
    .await;
    assert_eq!(script["content"], SCRIPT);
    let helper = app.create_bot("Helper").await;
    let (_, other) = running(&app, &helper).await;
    let hidden = tool(&app, &other, "skill_read", json!({ "id": "rea-growth" })).await;
    assert!(
        hidden["error"].as_str().unwrap().starts_with("NOT_FOUND"),
        "{hidden}"
    );
    let again = install(&app, &token).await;
    assert!(
        again["error"]
            .as_str()
            .unwrap()
            .starts_with("ALREADY_INSTALLED"),
        "{again}"
    );

    let removed = app
        .owner("DELETE", "/v1/settings/skills/rea-growth", None)
        .await;
    assert_eq!(removed.status, StatusCode::NO_CONTENT);
    let secrets = app.owner("GET", "/v1/task-secrets", None).await;
    assert_eq!(
        secrets.body["secrets"],
        json!([]),
        "the skill's key goes with it"
    );
    let gone = tool(&app, &token, "skill_read", json!({ "id": "rea-growth" })).await;
    assert!(gone["error"].as_str().unwrap().starts_with("NOT_FOUND"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_declined_card_installs_nothing_and_a_shared_skill_reaches_every_bot() {
    let Harness { app, synthetic } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let card_id = install(&app, &token).await["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait(&app, turn, "waiting").await;
    let declined = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/skill-installs/{card_id}/decline"),
            None,
        )
        .await;
    assert_eq!(declined.status, StatusCode::OK, "{}", declined.body);
    assert_eq!(declined.body["install"]["status"], "declined");
    wait(&app, turn, "completed").await;
    assert!(
        synthetic
            .packets()
            .await
            .last()
            .unwrap()
            .contains("<skill id=\"rea-growth\" status=\"declined\"/>")
    );
    let late = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/skill-installs/{card_id}/approve"),
            Some(json!({ "scope": "all" })),
        )
        .await;
    assert_eq!(late.status, StatusCode::CONFLICT);
    let skills = app.owner("GET", "/v1/settings/skills", None).await;
    assert_eq!(skills.body["skills"], json!([]));

    let (turn, token) = running(&app, &main).await;
    let card_id = install(&app, &token).await["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait(&app, turn, "waiting").await;
    let done = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/skill-installs/{card_id}/approve"),
            Some(json!({ "scope": "all" })),
        )
        .await;
    assert_eq!(done.body["install"]["key_stored"], false, "{}", done.body);
    wait(&app, turn, "completed").await;
    let skill = &app.owner("GET", "/v1/settings/skills", None).await.body["skills"][0];
    assert_eq!(skill["scope"], "all");
    assert_eq!(skill["bot_id"], Value::Null);
    assert_eq!(skill["key_stored"], false);
    let helper = app.create_bot("Helper").await;
    let (_, other) = running(&app, &helper).await;
    let read = tool(
        &app,
        &other,
        "skill_read",
        json!({ "id": "rea-growth", "path": "references/channels.md" }),
    )
    .await;
    assert_eq!(read["content"], "# Channels\n", "{read}");
}

#[tokio::test(flavor = "multi_thread")]
async fn links_that_do_not_hold_a_skill_are_refused_before_any_card() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    for (url, code) in [
        ("https://gitlab.com/acme/treg", "INVALID_SKILL"),
        (
            "https://github.com/acme/treg/tree/dev/skills/rea-growth",
            "NOT_FOUND",
        ),
        (
            "https://github.com/acme/treg/tree/main/skills/missing",
            "NOT_FOUND",
        ),
        (
            "https://github.com/acme/treg/tree/main/skills/long",
            "INVALID_SKILL",
        ),
        (
            "https://github.com/acme/huge/tree/main/skills/rea-growth",
            "INVALID_SKILL",
        ),
    ] {
        let refused = tool(
            &app,
            &token,
            "skill_install",
            json!({ "url": url, "reason": "r" }),
        )
        .await;
        assert!(
            refused["error"].as_str().unwrap().starts_with(code),
            "{url}: {refused}"
        );
    }
    let values = tool(
        &app,
        &token,
        "skill_install",
        json!({ "url": URL, "reason": "r", "env": [CANARY] }),
    )
    .await;
    assert!(
        values["error"]
            .as_str()
            .unwrap()
            .starts_with("INVALID_REQUEST")
    );
    assert!(!values.to_string().contains(CANARY));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM skill_installs")
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id = ?1")
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap(),
        "running",
        "a refused install parks nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_file_reads_in_full_across_pages() {
    let Harness { app, .. } = harness().await;
    let main = app.main_bot().await;
    let (turn, token) = running(&app, &main).await;
    let pending = tool(
        &app,
        &token,
        "skill_install",
        json!({ "url": "https://github.com/acme/treg/tree/main/skills/big", "reason": "r" }),
    )
    .await;
    let card_id = pending["request_id"].as_str().unwrap().to_owned();
    wait(&app, turn, "waiting").await;
    let done = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/skill-installs/{card_id}/approve"),
            Some(json!({ "scope": "all" })),
        )
        .await;
    assert_eq!(done.status, StatusCode::OK, "{}", done.body);
    wait(&app, turn, "completed").await;
    let (_, token) = running(&app, &main).await;
    let mut text = String::new();
    let mut offset = Some(0);
    let mut pages = 0;
    while let Some(at) = offset {
        let page = tool(
            &app,
            &token,
            "skill_read",
            json!({ "id": "big", "path": "big.md", "offset": at }),
        )
        .await;
        assert_eq!(page["bytes"], BIG.len(), "{page}");
        text.push_str(page["content"].as_str().unwrap());
        offset = page["next_offset"].as_u64();
        pages += 1;
    }
    assert!(pages > 1, "a long file spans pages");
    assert_eq!(text, BIG);
}
