//! POC-013: the original memory log and scoped retrieval, against real
//! PostgreSQL and the real router.

mod common;

use std::borrow::Cow;

use axum::http::StatusCode;
use common::{TestApp, TestDb};
use else_api::db::memory::{self, AppendError, Kind, NewEntry};
use else_api::grants::{DEFAULT_TTL, Tool};
use serde_json::{Value, json};
use uuid::Uuid;

fn entry<'a>(key: &'a str, body: &'a str) -> NewEntry<'a> {
    NewEntry {
        request_key: key,
        kind: Kind::AssistantMessage,
        body,
        source: json!({ "test": key }),
        supersedes_seq: None,
    }
}

async fn append(app: &TestApp, bot: Uuid, entry: &NewEntry<'_>) -> Result<Vec<i64>, AppendError> {
    let mut tx = app.db.pool.begin().await.unwrap();
    let result = memory::append(&mut tx, bot, entry).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

async fn seqs(app: &TestApp, bot: Uuid) -> Vec<i64> {
    sqlx::query_scalar("SELECT seq FROM memory_entry WHERE bot_id = $1 ORDER BY seq")
        .bind(bot)
        .fetch_all(&app.db.pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_appends_retries_and_rollbacks_keep_order_and_content() {
    let app = TestApp::installed().await;
    let a: Uuid = app.create_bot("A", "a").await.parse().unwrap();
    let b: Uuid = app.create_bot("B", "b").await.parse().unwrap();

    // 30 distinct appends and 10 retries of one key, across two bots, at once.
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..30 {
        let pool = app.db.pool.clone();
        let bot = if i % 3 == 0 { b } else { a };
        tasks.spawn(async move {
            let key = format!("k{i}");
            let body = format!("fact {i}");
            let mut tx = pool.begin().await.unwrap();
            let seqs = memory::append(&mut tx, bot, &entry(&key, &body))
                .await
                .unwrap();
            tx.commit().await.unwrap();
            (key, seqs)
        });
    }
    for _ in 0..10 {
        let pool = app.db.pool.clone();
        tasks.spawn(async move {
            let mut tx = pool.begin().await.unwrap();
            let seqs = memory::append(&mut tx, a, &entry("retry", "same content"))
                .await
                .unwrap();
            tx.commit().await.unwrap();
            ("retry".to_owned(), seqs)
        });
    }
    let mut retry_seqs = Vec::new();
    while let Some(done) = tasks.join_next().await {
        let (key, seqs) = done.unwrap();
        if key == "retry" {
            retry_seqs.push(seqs);
        }
    }
    retry_seqs.dedup();
    assert_eq!(
        retry_seqs.len(),
        1,
        "every retry returned the same sequence"
    );
    // Bot-local, contiguous, zero-based.
    assert_eq!(seqs(&app, a).await, (0..21).collect::<Vec<i64>>());
    assert_eq!(seqs(&app, b).await, (0..10).collect::<Vec<i64>>());
    let retries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_entry WHERE bot_id = $1 AND request_key = 'retry'",
    )
    .bind(a)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(retries, 1);

    // Same key, different content: a conflict, nothing written.
    assert!(matches!(
        append(&app, a, &entry("retry", "other content")).await,
        Err(AppendError::Conflict)
    ));

    // A rolled-back append consumes no sequence.
    let mut tx = app.db.pool.begin().await.unwrap();
    memory::append(&mut tx, a, &entry("rolled", "rolled back"))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let next = append(&app, a, &entry("after", "after rollback"))
        .await
        .unwrap();
    assert_eq!(next, vec![21]);
    let rolled: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_entry WHERE request_key = 'rolled'")
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(rolled, 0);

    // Long originals split at UTF-8 boundaries and reassemble exactly.
    let long = "é€x".repeat(25_000); // 150,000 bytes
    let parts = append(&app, a, &entry("long", &long)).await.unwrap();
    assert_eq!(parts, vec![22, 23, 24]);
    let bodies: Vec<String> = sqlx::query_scalar(
        "SELECT body FROM memory_entry WHERE bot_id = $1 AND request_key = 'long' ORDER BY part_index",
    )
    .bind(a)
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert!(bodies.iter().all(|b| b.len() <= memory::MAX_PART_BYTES));
    assert_eq!(bodies.concat(), long);

    // Originals are immutable.
    for statement in [
        "UPDATE memory_entry SET body = 'changed' WHERE seq = 0",
        "DELETE FROM memory_entry WHERE seq = 0",
    ] {
        assert!(
            sqlx::query(statement).execute(&app.db.pool).await.is_err(),
            "{statement}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn conversation_messages_enter_memory_once_with_their_source() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Work", "work").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let request = Uuid::new_v4();
    let sent = app
        .send(&bot, request, "My locker code is 4417.", Some("synthetic"))
        .await;
    assert_eq!(sent.status, StatusCode::ACCEPTED);
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    // A retried send is not a new memory.
    let retried = app
        .send(&bot, request, "My locker code is 4417.", Some("synthetic"))
        .await;
    assert_eq!(retried.status, StatusCode::OK);

    let log = app
        .owner("GET", &format!("/v1/bots/{bot}/memory"), None)
        .await
        .body;
    let entries = log["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{log}");
    assert_eq!(entries[0]["kind"], "owner_message");
    assert_eq!(entries[0]["authority"], "owner");
    assert_eq!(entries[0]["body"], "My locker code is 4417.");
    assert_eq!(entries[0]["source"]["item_id"], sent.body["item"]["id"]);
    assert_eq!(entries[1]["kind"], "assistant_message");
    assert_eq!(entries[1]["authority"], "assistant");
    assert_eq!(entries[1]["source"]["turn_id"], sent.body["turn"]["id"]);
    // A rejected send (deleted bot) adds nothing.
    app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    let rejected = app
        .send(&bot, Uuid::new_v4(), "after deletion", Some("synthetic"))
        .await;
    assert_eq!(rejected.status, StatusCode::GONE);
    assert_eq!(
        seqs(&app, bot_id).await,
        vec![0, 1],
        "retained, nothing added"
    );
    assert!(matches!(
        append(&app, bot_id, &entry("late", "late")).await,
        Err(AppendError::BotInactive)
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_explicit_owner_corrections_supersede_owner_entries() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Family", "family").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let other: Uuid = app.create_bot("Other", "other").await.parse().unwrap();
    let sent = app
        .send(
            &bot,
            Uuid::new_v4(),
            "Dentist is on Tuesday.",
            Some("synthetic"),
        )
        .await;
    app.wait_turn(sent.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let other_owner = append(
        &app,
        other,
        &NewEntry {
            request_key: "o",
            kind: Kind::OwnerMessage,
            body: "unrelated",
            source: json!({}),
            supersedes_seq: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(other_owner, vec![0]);

    let path = format!("/v1/bots/{bot}/memory/corrections");
    let correct = |seq: i64, text: &str| {
        app.owner(
            "POST",
            &path,
            Some(json!({ "request_id": Uuid::new_v4(), "supersedes_seq": seq, "text": text })),
        )
    };
    // The assistant's reply (seq 1) is not an owner assertion.
    let refused = correct(1, "no").await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert_eq!(refused.body["error"]["code"], "invalid_correction");
    // Unknown sequence in this bot (the other bot's entry does not count).
    assert_eq!(correct(9, "no").await.status, StatusCode::BAD_REQUEST);
    let accepted = correct(0, "Dentist moved to Thursday.").await;
    assert_eq!(accepted.status, StatusCode::CREATED, "{}", accepted.body);
    assert_eq!(accepted.body["seq"], 2);

    // Model or tool text cannot become an owner correction, even in SQL.
    let forged = sqlx::query(
        "INSERT INTO memory_entry (bot_id, seq, request_key, request_fingerprint, kind, authority,
             body, body_sha256, source, original_sha256, supersedes_seq)
         VALUES ($1, 3, 'forged', $2, 'owner_correction', 'assistant', 'x', $2, '{}', $2, 0)",
    )
    .bind(bot_id)
    .bind(vec![0u8; 32])
    .execute(&app.db.pool)
    .await;
    assert!(forged.is_err());
    // Nor can a correction point at another bot's entry.
    assert!(matches!(
        append(
            &app,
            bot_id,
            &NewEntry {
                request_key: "x",
                kind: Kind::AssistantMessage,
                body: "x",
                source: json!({}),
                supersedes_seq: Some(0),
            }
        )
        .await,
        Err(AppendError::InvalidCorrection)
    ));

    // Search hides the replaced entry unless asked; reads show the link.
    let page = memory::search(&app.db.pool, bot_id, "dentist", false, None, 0)
        .await
        .unwrap();
    // The synthetic reply (seq 1) echoes the text; the replaced owner entry is hidden.
    let mut found: Vec<i64> = page.hits.iter().map(|h| h.seq).collect();
    found.sort();
    assert_eq!(found, vec![1, 2]);
    let page = memory::search(&app.db.pool, bot_id, "dentist", true, None, 0)
        .await
        .unwrap();
    let mut found: Vec<i64> = page.hits.iter().map(|h| h.seq).collect();
    found.sort();
    assert_eq!(found, vec![0, 1, 2]);
    let original = &page.hits.iter().find(|h| h.seq == 0).unwrap();
    assert_eq!(original.superseded_by, vec![2]);
}

/// The owner's original among the hits (synthetic replies echo the text).
fn owner_hit(hits: &Value) -> Value {
    hits["structuredContent"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["authority"] == "owner")
        .cloned()
        .unwrap()
}

async fn rpc(app: &TestApp, token: &str, method: &str, params: Value) -> common::Reply {
    app.call(
        "POST",
        "/tools/v1/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })),
        Some(token),
    )
    .await
}

async fn tool(app: &TestApp, token: &str, name: &str, args: Value) -> Value {
    let reply = rpc(
        app,
        token,
        "tools/call",
        json!({ "name": name, "arguments": args }),
    )
    .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let mut result = reply.body["result"].clone();
    // Results travel once, as JSON text; expose it parsed for assertions.
    assert!(
        result.to_string().len() <= memory::RESPONSE_BUDGET,
        "budget"
    );
    if result["isError"] != true {
        let text = result["content"][0]["text"].as_str().unwrap();
        result["structuredContent"] = serde_json::from_str(text).unwrap();
    }
    result
}

/// A running turn of `bot` and a memory grant for it.
async fn running_grant(app: &TestApp, bot: &str) -> (Uuid, String) {
    let reply = app
        .send(bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = $1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let token = app.scheduler.grants().issue(
        bot.parse().unwrap(),
        conversation,
        turn,
        vec![Tool::ReadMemory],
        DEFAULT_TTL,
    );
    (turn, token)
}

fn owner_entry<'a>(key: &'a str, body: &'a str, supersedes: Option<i64>) -> NewEntry<'a> {
    NewEntry {
        request_key: key,
        kind: if supersedes.is_some() {
            Kind::OwnerCorrection
        } else {
            Kind::OwnerMessage
        },
        body,
        source: json!({}),
        supersedes_seq: supersedes,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn responses_stay_within_budget_even_with_escaped_text() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Big", "big").await;
    let bot_id: Uuid = bot.parse().unwrap();
    // Quotes and control characters double or sextuple when escaped twice.
    let body = "\"\u{1}".repeat(400);
    for i in 0..20 {
        append(
            &app,
            bot_id,
            &entry(&format!("e{i}"), &format!("needle {i} {body}")),
        )
        .await
        .unwrap();
    }
    let (_, token) = running_grant(&app, &bot).await;
    let read = tool(
        &app,
        &token,
        "memory_read",
        json!({ "start_seq": 0, "count": 20 }),
    )
    .await;
    let shown = read["structuredContent"]["entries"]
        .as_array()
        .unwrap()
        .len();
    assert!(shown >= 1);
    if shown < 20 {
        assert_eq!(read["structuredContent"]["next_start_seq"], shown as i64);
    }
    let search = tool(&app, &token, "memory_search", json!({ "query": "needle" })).await;
    let hits = search["structuredContent"]["hits"]
        .as_array()
        .unwrap()
        .len();
    assert!(hits >= 1 && hits <= 20);
    // A long entry pages exactly: offsets inside a character are normalized
    // and `next_offset` continues without loss.
    let long = format!("é{}", "x".repeat(30_000));
    let seq = append(&app, bot_id, &entry("long", &long)).await.unwrap()[0];
    let mut offset = 1; // inside "é"
    let mut text = String::new();
    loop {
        let page = tool(
            &app,
            &token,
            "memory_read",
            json!({ "start_seq": seq, "count": 1, "offset": offset }),
        )
        .await;
        let e = &page["structuredContent"]["entries"][0];
        if text.is_empty() {
            assert_eq!(e["offset"], 0, "normalized to the character start");
        }
        text.push_str(e["body"].as_str().unwrap());
        match e["next_offset"].as_u64() {
            Some(next) => {
                assert_eq!(e["truncated"], true);
                offset = next;
            }
            None => {
                assert_eq!(e["truncated"], false);
                break;
            }
        }
    }
    assert_eq!(text, long);
}

#[tokio::test(flavor = "multi_thread")]
async fn search_pages_ignore_corrections_made_after_the_first_page() {
    let app = TestApp::installed().await;
    let bot: Uuid = app.create_bot("Pages", "pages").await.parse().unwrap();
    for i in 0..21 {
        append(
            &app,
            bot,
            &owner_entry(&format!("p{i}"), &format!("topic item {i}"), None),
        )
        .await
        .unwrap();
    }
    let first = memory::search(&app.db.pool, bot, "topic", false, None, 0)
        .await
        .unwrap();
    assert_eq!(first.hits.len(), 20);
    let seen: Vec<i64> = first.hits.iter().map(|h| h.seq).collect();
    let missing: Vec<i64> = (0..21).filter(|s| !seen.contains(s)).collect();
    assert_eq!(missing.len(), 1);
    // A correction of a hit on the first page, made between pages.
    append(
        &app,
        bot,
        &owner_entry("fix", "replaced wording", Some(seen[0])),
    )
    .await
    .unwrap();
    let second = memory::search(
        &app.db.pool,
        bot,
        "topic",
        false,
        Some(first.watermark),
        first.next_offset.unwrap(),
    )
    .await
    .unwrap();
    let rest: Vec<i64> = second.hits.iter().map(|h| h.seq).collect();
    assert_eq!(rest, missing, "the second page continues the same snapshot");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_append_racing_a_deletion_never_lands_after_it() {
    let app = TestApp::installed().await;
    let bot: Uuid = app.create_bot("Race", "race").await.parse().unwrap();
    // A deletion in progress holds the bot row.
    let mut deletion = app.db.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM bots WHERE id = $1 FOR UPDATE")
        .bind(bot)
        .execute(&mut *deletion)
        .await
        .unwrap();
    let pool = app.db.pool.clone();
    let append = tokio::spawn(async move {
        let mut tx = pool.begin().await.unwrap();
        let result = memory::append(&mut tx, bot, &entry("racer", "late")).await;
        if result.is_ok() {
            tx.commit().await.unwrap();
        }
        result.map(|_| ())
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(!append.is_finished(), "the append waits for the deletion");
    sqlx::query("UPDATE bots SET status = 'deleted', deleted_at = now() WHERE id = $1")
        .bind(bot)
        .execute(&mut *deletion)
        .await
        .unwrap();
    deletion.commit().await.unwrap();
    assert!(matches!(
        append.await.unwrap(),
        Err(AppendError::BotInactive)
    ));
    assert!(seqs(&app, bot).await.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_json_rpc_is_rejected_before_dispatch() {
    let app = TestApp::installed().await;
    let bot = app.create_bot("Rpc", "rpc").await;
    let (_, token) = running_grant(&app, &bot).await;
    let send = |body: Value| app.call("POST", "/tools/v1/mcp", Some(body), Some(&token));
    for (body, code) in [
        (json!({ "id": 1, "method": "tools/list" }), -32600),
        (
            json!({ "jsonrpc": "2.0", "id": null, "method": "tools/list" }),
            -32600,
        ),
        (
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": [1] }),
            -32600,
        ),
        (json!({ "jsonrpc": "2.0", "params": {} }), -32600),
        (
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "arguments": {} } }),
            -32602,
        ),
        (
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                 "params": { "name": "memory_bots", "arguments": [1] } }),
            -32602,
        ),
    ] {
        let reply = send(body.clone()).await;
        assert_eq!(
            reply.body["error"]["code"], code,
            "{body} -> {}",
            reply.body
        );
    }
    let logged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_entry WHERE kind = 'tool_activity'")
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert_eq!(logged, 0, "nothing ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_tools_read_own_memory_and_shared_memory_only() {
    let app = TestApp::installed().await;
    let me = app.create_bot("Family", "family").await;
    let work = app.create_bot("Work", "work").await;
    let fact = app
        .send(
            &work,
            Uuid::new_v4(),
            "Project codename is BLUEFIN.",
            Some("synthetic"),
        )
        .await;
    app.wait_turn(fact.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;
    let own = app
        .send(
            &me,
            Uuid::new_v4(),
            "School pickup is at 15:30.",
            Some("synthetic"),
        )
        .await;
    app.wait_turn(own.body["turn"]["id"].as_str().unwrap(), &["completed"])
        .await;

    // A running turn of `me` holds the grant.
    let reply = app
        .send(&me, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = $1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let grants = app.scheduler.grants();
    let token = grants.issue(
        me.parse().unwrap(),
        conversation,
        turn,
        vec![Tool::ReadMemory],
        DEFAULT_TTL,
    );

    let init = rpc(
        &app,
        &token,
        "initialize",
        json!({ "protocolVersion": "2025-06-18" }),
    )
    .await;
    assert_eq!(init.body["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init.body["result"]["serverInfo"]["name"], "elsebot");
    let note = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })),
            Some(&token),
        )
        .await;
    assert_eq!(note.status, StatusCode::ACCEPTED);
    let listed = rpc(&app, &token, "tools/list", json!({})).await;
    let names: Vec<&str> = listed.body["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["memory_bots", "memory_search", "memory_read"]);
    assert!(
        listed.body["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["annotations"]["readOnlyHint"] == true)
    );

    // Own memory, with provenance outside the text.
    let hits = tool(&app, &token, "memory_search", json!({ "query": "pickup" })).await;
    let hit = &owner_hit(&hits);
    assert_eq!(hit["body"], "School pickup is at 15:30.");
    assert_eq!(hit["authority"], "owner");
    assert_eq!(hit["bot_id"], me);
    assert_eq!(hit["source"]["item_id"], own.body["item"]["id"]);

    // Another bot's memory needs the owner's grant.
    let denied = tool(
        &app,
        &token,
        "memory_search",
        json!({ "query": "codename", "bot_id": work }),
    )
    .await;
    assert_eq!(denied["isError"], true);
    let listed = tool(&app, &token, "memory_bots", json!({})).await;
    assert_eq!(
        listed["structuredContent"]["bots"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let granted = app
        .owner("PUT", &format!("/v1/bots/{me}/memory-access/{work}"), None)
        .await;
    assert_eq!(granted.status, StatusCode::NO_CONTENT);
    let hits = tool(
        &app,
        &token,
        "memory_search",
        json!({ "query": "codename", "bot_id": work }),
    )
    .await;
    let hit = &owner_hit(&hits);
    assert_eq!(hit["body"], "Project codename is BLUEFIN.");
    assert_eq!(hit["bot_id"], work, "results keep their originating bot");
    let read = tool(
        &app,
        &token,
        "memory_read",
        json!({ "start_seq": 0, "count": 2, "bot_id": work }),
    )
    .await;
    assert_eq!(
        read["structuredContent"]["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(read["structuredContent"]["length"], 2);

    // Each call was logged to the caller's memory as tool activity only.
    let logged: Vec<(String, String)> = sqlx::query_as(
        "SELECT kind, authority FROM memory_entry WHERE bot_id = $1 AND kind = 'tool_activity'",
    )
    .bind(me.parse::<Uuid>().unwrap())
    .fetch_all(&app.db.pool)
    .await
    .unwrap();
    assert!(logged.len() >= 4);
    assert!(logged.iter().all(|(_, authority)| authority == "tool"));
    let work_entries = seqs(&app, work.parse().unwrap()).await;
    assert_eq!(
        work_entries,
        vec![0, 1],
        "reads never write the target's memory"
    );

    // Deleting the target retains its memory and ends shared access.
    app.owner("DELETE", &format!("/v1/bots/{work}"), None).await;
    let gone = tool(
        &app,
        &token,
        "memory_search",
        json!({ "query": "codename", "bot_id": work }),
    )
    .await;
    assert_eq!(gone["isError"], true);
    assert_eq!(seqs(&app, work.parse().unwrap()).await, vec![0, 1]);
    let owner_read = app
        .owner("GET", &format!("/v1/bots/{work}/memory"), None)
        .await;
    assert_eq!(owner_read.status, StatusCode::GONE);

    // A grant without the memory tool, the owner token, and a forged grant are refused.
    let history_only = grants.issue(
        me.parse().unwrap(),
        conversation,
        turn,
        vec![Tool::ReadHistory],
        DEFAULT_TTL,
    );
    let refused = rpc(
        &app,
        &history_only,
        "tools/call",
        json!({ "name": "memory_bots", "arguments": {} }),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(
        rpc(&app, common::TOKEN, "tools/list", json!({}))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        rpc(&app, "tg_forged", "tools/list", json!({})).await.status,
        StatusCode::UNAUTHORIZED
    );

    // When the turn ends its grant stops working.
    app.owner(
        "POST",
        &format!("/v1/bots/{me}/cancel"),
        Some(json!({ "request_id": Uuid::new_v4() })),
    )
    .await;
    app.wait_turn(&turn.to_string(), &["interrupted"]).await;
    let after = rpc(
        &app,
        &token,
        "tools/call",
        json!({ "name": "memory_bots", "arguments": {} }),
    )
    .await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn migration_backfills_existing_history_in_order() {
    let db = TestDb::empty().await;
    let before = sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            else_api::db::MIGRATOR
                .migrations
                .iter()
                .filter(|m| m.version < 6)
                .cloned()
                .collect(),
        ),
        ignore_missing: false,
        locking: true,
        no_tx: false,
        table_name: else_api::db::MIGRATOR.table_name.clone(),
        create_schemas: else_api::db::MIGRATOR.create_schemas.clone(),
    };
    before.run(&db.pool).await.unwrap();
    let bot = Uuid::new_v4();
    let conversation = Uuid::new_v4();
    sqlx::query("INSERT INTO bots (id, name, role) VALUES ($1, 'Old', 'old')")
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id) VALUES ($1, $2)")
        .bind(conversation)
        .bind(bot)
        .execute(&db.pool)
        .await
        .unwrap();
    let long = "ü".repeat(20_000); // 20,000 characters, 40,000 bytes
    let texts = [
        ("user", "first question".to_owned()),
        ("assistant", long.clone()),
        ("user", "second".to_owned()),
    ];
    for (n, (kind, text)) in texts.iter().enumerate() {
        let turn = Uuid::new_v4();
        let owner_item = Uuid::new_v4();
        let no = n as i64 + 1;
        sqlx::query(
            "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
                 owner_item_id, provider_id, status)
             VALUES ($1, $2, $3, $4, $5, $6, $7, 'synthetic', 'completed')",
        )
        .bind(turn)
        .bind(bot)
        .bind(conversation)
        .bind(Uuid::new_v4())
        .bind(vec![0u8; 32])
        .bind(no)
        .bind(owner_item)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO items (id, conversation_id, seq, turn_id, kind, text) VALUES ($1, $2, $3, $4, $5, $6)")
            .bind(Uuid::new_v4())
            .bind(conversation)
            .bind(no)
            .bind(turn)
            .bind(kind)
            .bind(text)
            .execute(&db.pool)
            .await
            .unwrap();
    }
    else_api::db::migrate(&db.pool).await.unwrap();
    let rows: Vec<(i64, String, String, i32, i32)> = sqlx::query_as(
        "SELECT seq, kind, body, part_index, part_count FROM memory_entry WHERE bot_id = $1 ORDER BY seq",
    )
    .bind(bot)
    .fetch_all(&db.pool)
    .await
    .unwrap();
    let kinds: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "owner_message",
            "assistant_message",
            "assistant_message",
            "owner_message"
        ]
    );
    assert_eq!(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    assert_eq!(
        format!("{}{}", rows[1].2, rows[2].2),
        long,
        "split, not truncated"
    );
    assert_eq!((rows[2].3, rows[2].4), (1, 2));
    let next: i64 = sqlx::query_scalar("SELECT next_seq FROM memory_store WHERE bot_id = $1")
        .bind(bot)
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(next, 4);
}
