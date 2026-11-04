//! POC-014: the summary tree and saved views, against real SQLite with
//! the application's own storage functions and compactor.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use axum::http::StatusCode;
use common::TestApp;
use else_api::app::memory_tree::NodeId;
use else_api::db::memory::{self, Kind, NewEntry};
use else_api::db::memory_tree::{self, Completion};
use else_api::grants::{DEFAULT_TTL, Tool};
use else_api::memory_worker;
use serde_json::{Value, json};
use uuid::Uuid;

async fn small_recipe(app: &TestApp) {
    let reply = app
        .owner(
            "PATCH",
            "/v1/settings/memory",
            Some(json!({ "node_bytes": 64, "view_high": 600, "view_low": 300 })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
}

async fn add(app: &TestApp, bot: Uuid, key: &str, body: &str) -> i64 {
    let mut tx = app.db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let seqs = memory::append(
        &mut tx,
        bot,
        &NewEntry {
            request_key: key,
            kind: Kind::OwnerMessage,
            body,
            source: json!({}),
            supersedes_seq: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    seqs[0]
}

/// The labeled leaf text of every entry, by sequence.
async fn leaf_texts(app: &TestApp, bot: Uuid) -> HashMap<i64, String> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT seq, body FROM memory_entry WHERE bot_id = ?1")
            .bind(bot)
            .fetch_all(&app.db.pool)
            .await
            .unwrap();
    rows.into_iter()
        .map(|(seq, body)| (seq, format!("Owner: {body}")))
        .collect()
}

/// Every exact node's text is exactly the originals of its range, in order.
async fn assert_exact_coverage(app: &TestApp, bot: Uuid) -> memory_tree::SavedView {
    let saved = memory_tree::load_view(&app.db.pool, bot).await.unwrap();
    assert!(
        saved.valid,
        "the saved view is an aligned contiguous prefix"
    );
    let ids: Vec<NodeId> = saved.nodes.iter().map(|n| n.node).collect();
    assert!(else_api::app::memory_tree::covers(
        &ids,
        saved.published_seq + 1
    ));
    let leaves = leaf_texts(app, bot).await;
    for node in &saved.nodes {
        let expected: Vec<String> = (node.node.start..node.node.end())
            .map(|seq| leaves[&seq].clone())
            .collect();
        assert_eq!(
            node.body.as_deref(),
            Some(expected.join("\n").as_str()),
            "{node:?}"
        );
    }
    saved
}

#[tokio::test(flavor = "multi_thread")]
async fn exact_tree_publishes_in_order_compacts_and_keeps_every_range() {
    let app = TestApp::installed().await;
    small_recipe(&app).await;
    let bot: Uuid = app.create_bot("Tree").await.parse().unwrap();
    let mut merged_once = false;
    for batch in 0..12 {
        for i in 0..10 {
            let n = batch * 10 + i;
            add(&app, bot, &format!("k{n}"), &format!("m{n}")).await;
        }
        memory_worker::drain(&app.db.pool).await.unwrap();
        let saved = assert_exact_coverage(&app, bot).await;
        assert_eq!(
            saved.published_seq,
            batch * 10 + 9,
            "everything fits a node"
        );
        merged_once |= saved.nodes.iter().any(|n| n.node.span > 1);
        let bytes = else_api::app::memory_tree::WRAPPER_BYTES
            + saved
                .nodes
                .iter()
                .map(|n| else_api::app::memory_tree::line_bytes(n.node, n.body.as_deref().unwrap()))
                .sum::<usize>();
        if !saved.compacting {
            assert!(bytes <= 600, "{bytes}");
        }
    }
    assert!(merged_once, "compaction merged exact parents");
    // Joins that no longer fit a node wait for a summary instead of being cut.
    let blocked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_job WHERE bot_id = ?1 AND blocked_reason = 'summary_unavailable'",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert!(blocked > 0);
    let oversized: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM memory_node WHERE bot_id = ?1 AND length(CAST(body AS BLOB)) > 64",
    )
    .bind(bot)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(oversized, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_original_waits_for_a_summary_and_coverage_is_explicit() {
    let app = TestApp::installed().await;
    small_recipe(&app).await;
    let bot = app.create_bot("Long").await;
    let bot_id: Uuid = bot.parse().unwrap();
    add(&app, bot_id, "a", "short one").await;
    add(&app, bot_id, "b", &"long ".repeat(40)).await; // 200 bytes > 64
    add(&app, bot_id, "c", "after the long one").await;
    memory_worker::drain(&app.db.pool).await.unwrap();
    let saved = assert_exact_coverage(&app, bot_id).await;
    assert_eq!(
        saved.published_seq, 0,
        "publication stops at the first missing leaf"
    );
    assert_eq!(saved.blocked, vec![("summary_unavailable".to_owned(), 1)]);
    // The later leaf is built and waits; the original stays readable.
    let later = memory_tree::node(&app.db.pool, bot_id, 1, NodeId::leaf(2))
        .await
        .unwrap();
    assert!(later.is_some());

    // The tools report the partial coverage and still reach every original.
    let reply = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let token = app
        .scheduler
        .grants()
        .issue(
            bot_id,
            conversation,
            turn,
            vec![Tool::ReadMemory],
            DEFAULT_TTL,
        )
        .await;
    let call = |name: &'static str, args: Value| {
        let token = token.clone();
        let app = &app;
        async move {
            let reply = app
                .call(
                    "POST",
                    "/tools/v1/mcp",
                    Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                                 "params": { "name": name, "arguments": args } })),
                    Some(&token),
                )
                .await;
            let result = &reply.body["result"];
            assert_ne!(result["isError"], true, "{name}: {result}");
            serde_json::from_str::<Value>(result["content"][0]["text"].as_str().unwrap()).unwrap()
        }
    };
    let view = call("memory_view", json!({})).await;
    assert_eq!(view["covered"], json!({ "start_seq": 0, "end_seq": 0 }));
    assert_eq!(view["pending"]["start_seq"], 1);
    assert_eq!(view["blocked"][0]["reason"], "summary_unavailable");
    assert_eq!(view["nodes"][0]["text"], "Owner: short one");
    let leaf = call("memory_zoom", json!({ "start_seq": 1, "span": 1 })).await;
    assert_eq!(leaf["parts"][0]["original"]["body"], "long ".repeat(40));
    let pair = call("memory_zoom", json!({ "start_seq": 0, "span": 2 })).await;
    assert_eq!(pair["parts"][0]["built"], true);
    assert_eq!(
        pair["parts"][1]["built"], false,
        "unbuilt halves are explicit"
    );
    let date = call("memory_date", json!({ "seq": 0, "end_seq": 2 })).await;
    assert_eq!(date["dates"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn replaced_leases_and_concurrent_workers_never_publish_twice() {
    let app = TestApp::installed().await;
    small_recipe(&app).await;
    let bot: Uuid = app.create_bot("Race").await.parse().unwrap();
    add(&app, bot, "first", "first").await;
    // A worker claims, then stalls past its lease; another takes over.
    let slow = memory_tree::claim(&app.db.pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    sqlx::query("UPDATE memory_job SET lease_until = strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now', '-1 seconds')")
        .execute(&app.db.pool)
        .await
        .unwrap();
    let fresh = memory_tree::claim(&app.db.pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!((slow.node, slow.generation), (fresh.node, fresh.generation));
    assert_ne!(slow.token, fresh.token);
    let input = vec![0u8; 32];
    assert_eq!(
        memory_tree::complete(
            &app.db.pool,
            &slow,
            "verbatim",
            "Owner: first",
            &input,
            json!({})
        )
        .await
        .unwrap(),
        Completion::Stale
    );
    assert_eq!(
        memory_tree::complete(
            &app.db.pool,
            &fresh,
            "verbatim",
            "Owner: first",
            &input,
            json!({})
        )
        .await
        .unwrap(),
        Completion::Done
    );

    // Three workers race over many jobs; every range is built once.
    for n in 1..80 {
        add(&app, bot, &format!("r{n}"), &format!("r{n}")).await;
    }
    let mut workers = tokio::task::JoinSet::new();
    for _ in 0..3 {
        let pool = app.db.pool.clone();
        workers.spawn(async move { memory_worker::drain(&pool).await.unwrap() });
    }
    while workers.join_next().await.is_some() {}
    memory_worker::drain(&app.db.pool).await.unwrap();
    let saved = assert_exact_coverage(&app, bot).await;
    assert_eq!(saved.published_seq, 79);

    // A deleted bot's late completion is fenced.
    add(&app, bot, "late", "late").await;
    let late = memory_tree::claim(&app.db.pool, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    app.owner("DELETE", &format!("/v1/bots/{bot}"), None).await;
    assert_eq!(
        memory_tree::complete(
            &app.db.pool,
            &late,
            "verbatim",
            "Owner: late",
            &input,
            json!({})
        )
        .await
        .unwrap(),
        Completion::Stale
    );
    assert!(
        memory_tree::claim(&app.db.pool, Duration::from_secs(60))
            .await
            .unwrap()
            .is_none(),
        "no claims for a deleted bot"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_keeps_the_saved_view_and_a_rebuild_catches_up_and_switches() {
    let app = TestApp::installed().await;
    small_recipe(&app).await;
    let bot = app.create_bot("Saved").await;
    let bot_id: Uuid = bot.parse().unwrap();
    for n in 0..40 {
        add(&app, bot_id, &format!("s{n}"), &format!("s{n}")).await;
    }
    memory_worker::drain(&app.db.pool).await.unwrap();
    let before = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    let originals: String = sqlx::query_scalar(
        "SELECT group_concat(lower(hex(body_sha256)), ',' ORDER BY seq) FROM memory_entry WHERE bot_id = ?1",
    )
    .bind(bot_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();

    let app = app.restart().await;
    let after = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    assert_eq!(before, after, "restart loads the exact saved view");

    // A new recipe applies through an explicit rebuild into generation 2.
    let bigger = app
        .owner(
            "PATCH",
            "/v1/settings/memory",
            Some(json!({ "node_bytes": 128, "view_high": 900, "view_low": 600 })),
        )
        .await;
    assert_eq!(bigger.status, StatusCode::OK);
    let started = app
        .owner("POST", &format!("/v1/bots/{bot}/memory/rebuild"), None)
        .await;
    assert_eq!(started.status, StatusCode::ACCEPTED);
    assert_eq!(started.body["building_generation"], 2);
    // Appends during the rebuild reach both generations.
    for n in 40..50 {
        add(&app, bot_id, &format!("s{n}"), &format!("s{n}")).await;
    }
    let still = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    assert_eq!(
        still["generation"], 1,
        "the old view serves until the switch"
    );
    memory_worker::drain(&app.db.pool).await.unwrap();
    let rebuilt = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    assert_eq!(rebuilt["generation"], 2);
    assert_eq!(rebuilt["building_generation"], Value::Null);
    assert_eq!(
        rebuilt["published_seq"], 49,
        "caught up with appends after the watermark"
    );
    assert!(rebuilt["revision"].as_i64() > after["revision"].as_i64());
    assert_exact_coverage(&app, bot_id).await;
    let unchanged: String = sqlx::query_scalar(
        "SELECT group_concat(lower(hex(body_sha256)), ',' ORDER BY seq) FROM memory_entry
         WHERE bot_id = ?1 AND seq < 40",
    )
    .bind(bot_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(unchanged, originals, "originals were not rewritten");
    let old_nodes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM memory_node WHERE bot_id = ?1 AND generation = 1")
            .bind(bot_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert!(old_nodes > 0, "the old generation is kept");
    let recipe: Value = sqlx::query_scalar(
        "SELECT recipe FROM memory_generation WHERE bot_id = ?1 AND generation = 2",
    )
    .bind(bot_id)
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    assert_eq!(recipe["node_bytes"], 128);
}

/// Review fixes: truncation is explicit, long backlogs keep publishing,
/// rebuilds switch only after catching up, the compaction view is saved,
/// and damaged views are reported, not normalized.
#[tokio::test(flavor = "multi_thread")]
async fn cut_text_is_marked_with_where_the_rest_is() {
    let app = TestApp::installed().await;
    let reply = app
        .owner(
            "PATCH",
            "/v1/settings/memory",
            Some(json!({ "node_bytes": 20000, "view_high": 200000, "view_low": 100000 })),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let bot = app.create_bot("Wide").await;
    let bot_id: Uuid = bot.parse().unwrap();
    let long = "w".repeat(10_009);
    add(&app, bot_id, "long", &long).await;
    memory_worker::drain(&app.db.pool).await.unwrap();
    let reply = app
        .send(&bot, Uuid::new_v4(), "[synthetic:hang]", Some("synthetic"))
        .await;
    let turn: Uuid = reply.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_turn(&turn.to_string(), &["running"]).await;
    let conversation: Uuid = sqlx::query_scalar("SELECT conversation_id FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    let token = app
        .scheduler
        .grants()
        .issue(
            bot_id,
            conversation,
            turn,
            vec![Tool::ReadMemory],
            DEFAULT_TTL,
        )
        .await;
    let reply = app
        .call(
            "POST",
            "/tools/v1/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": { "name": "memory_view", "arguments": {} } })),
            Some(&token),
        )
        .await;
    let view: Value =
        serde_json::from_str(reply.body["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let node = &view["nodes"][0];
    assert_eq!(node["truncated"], true);
    assert_eq!(node["text_bytes"], 10_016, "\"Owner: \" + 10,009 bytes");
    assert_eq!(node["continue_with"]["tool"], "memory_read");
    assert_eq!(node["continue_with"]["start_seq"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_backlog_longer_than_one_batch_keeps_publishing() {
    let app = TestApp::installed().await;
    let bot: Uuid = app.create_bot("Backlog").await.parse().unwrap();
    add(&app, bot, "first", "first").await;
    // Seed 10,050 built leaves behind a stalled first leaf.
    sqlx::query("UPDATE memory_store SET next_seq = 10050 WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memory_node (bot_id, generation, start_seq, span, body, method, input_hash)
         WITH RECURSIVE seqs(s) AS (VALUES (1) UNION ALL SELECT s+1 FROM seqs WHERE s < 10049) SELECT ?1, 1, s, 1, 'Owner: x', 'verbatim', ?2 FROM seqs",
    )
    .bind(bot)
    .bind(vec![0u8; 32])
    .execute(&app.db.pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM memory_job WHERE bot_id = ?1")
        .bind(bot)
        .execute(&app.db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO memory_node (bot_id, generation, start_seq, span, body, method, input_hash)
         VALUES (?1, 1, 0, 1, 'Owner: first', 'verbatim', ?2)",
    )
    .bind(bot)
    .bind(vec![0u8; 32])
    .execute(&app.db.pool)
    .await
    .unwrap();
    memory_worker::drain(&app.db.pool).await.unwrap();
    let saved = memory_tree::load_view(&app.db.pool, bot).await.unwrap();
    assert_eq!(
        saved.published_seq, 10049,
        "published through the whole backlog"
    );
    assert!(saved.valid);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rebuild_switches_only_after_reaching_the_append_watermark() {
    let app = TestApp::installed().await;
    small_recipe(&app).await;
    let bot = app.create_bot("Catch").await;
    let bot_id: Uuid = bot.parse().unwrap();
    for n in 0..5 {
        add(&app, bot_id, &format!("u{n}"), &format!("u{n}")).await;
    }
    // Nothing built or published yet; the rebuild must not switch at once.
    let started = app
        .owner("POST", &format!("/v1/bots/{bot}/memory/rebuild"), None)
        .await;
    assert_eq!(started.body["building_generation"], 2);
    let view = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    assert_eq!(view["generation"], 1);
    assert_eq!(view["building_generation"], 2);
    memory_worker::drain(&app.db.pool).await.unwrap();
    let view = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    assert_eq!(view["generation"], 2);
    assert_eq!(view["published_seq"], 4);

    // A leaf that needs a summary holds the rebuild open; nothing is lost.
    add(&app, bot_id, "long", &"long ".repeat(40)).await;
    let started = app
        .owner("POST", &format!("/v1/bots/{bot}/memory/rebuild"), None)
        .await;
    assert_eq!(started.body["building_generation"], 3);
    memory_worker::drain(&app.db.pool).await.unwrap();
    let view = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    assert_eq!(view["generation"], 2);
    assert_eq!(view["building_generation"], 3);
    assert_eq!(view["published_seq"], 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_compaction_view_is_saved_and_damaged_views_are_reported() {
    let app = TestApp::installed().await;
    let reply = app
        .owner(
            "PATCH",
            "/v1/settings/memory",
            Some(
                json!({ "node_bytes": 64, "view_high": 1200, "view_low": 900,
                         "context_high": 400, "context_low": 200 }),
            ),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let bot = app.create_bot("Ctx").await;
    let bot_id: Uuid = bot.parse().unwrap();
    for n in 0..60 {
        add(&app, bot_id, &format!("c{n}"), &format!("c{n}")).await;
    }
    memory_worker::drain(&app.db.pool).await.unwrap();
    let saved = memory_tree::load_view(&app.db.pool, bot_id).await.unwrap();
    assert!(saved.valid);
    assert!(else_api::app::memory_tree::covers(&saved.context, 60));
    assert!(
        saved.context.len() < saved.nodes.len(),
        "the compaction view is coarser"
    );
    let before = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    let app = app.restart().await;
    let after = app
        .owner("GET", &format!("/v1/bots/{bot}/memory/view"), None)
        .await
        .body;
    assert_eq!(before["context_nodes"], after["context_nodes"]);

    // A malformed element is reported and preserved, never normalized.
    sqlx::query(
        "UPDATE memory_store SET view_nodes = json_insert(view_nodes, '$[#]', 'damaged') WHERE bot_id = ?1",
    )
    .bind(bot_id)
    .execute(&app.db.pool)
    .await
    .unwrap();
    let damaged: Value =
        sqlx::query_scalar("SELECT view_nodes FROM memory_store WHERE bot_id = ?1")
            .bind(bot_id)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
    assert!(
        !memory_tree::load_view(&app.db.pool, bot_id)
            .await
            .unwrap()
            .valid
    );
    add(&app, bot_id, "after", "after").await;
    memory_worker::drain(&app.db.pool).await.unwrap();
    let kept: Value = sqlx::query_scalar("SELECT view_nodes FROM memory_store WHERE bot_id = ?1")
        .bind(bot_id)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
    assert_eq!(kept, damaged, "publication paused instead of rewriting");

    // A view pointing at a generation that does not exist is invalid.
    let other: Uuid = app.create_bot("Gen").await.parse().unwrap();
    add(&app, other, "g", "g").await;
    sqlx::query("UPDATE memory_store SET generation = 999 WHERE bot_id = ?1")
        .bind(other)
        .execute(&app.db.pool)
        .await
        .unwrap();
    assert!(
        !memory_tree::load_view(&app.db.pool, other)
            .await
            .unwrap()
            .valid
    );
}
