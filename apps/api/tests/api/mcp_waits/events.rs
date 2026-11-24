//! Waits that wake on an MCP event (SEP-3415) instead of status calls, and
//! fall back to status calls whenever the event cannot be watched.

use super::*;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream;

use else_api::db::mcp_waits;

const EVENT: &str = "thread.run_settled";

/// A Streamable HTTP MCP server on 2026-07-28 with Events beside the older
/// handshake, as T3 Code serves them. Its event log is the hub: a cursor is
/// the number of events before it.
struct Server {
    offers_events: bool,
    delivery: Vec<&'static str>,
    job: Mutex<&'static str>,
    status_calls: AtomicUsize,
    log: Mutex<Vec<Value>>,
    published: tokio::sync::watch::Sender<usize>,
    /// Streams to end right after they confirm.
    drop_streams: AtomicUsize,
    /// End the open stream after its next event.
    drop_after_event: AtomicBool,
    /// Answer status calls with an error.
    fail_status: AtomicBool,
    polls: AtomicUsize,
    /// End every stream with `notifications/events/terminated`.
    terminate: AtomicBool,
    /// Streams resuming from a cursor to end as if the event type's schema
    /// changed since that cursor.
    schema_changes: AtomicUsize,
    /// Report a gap on the next poll.
    gap: AtomicBool,
    /// The cursor each poll or stream asked for.
    asked: Mutex<Vec<Value>>,
    streams_opened: AtomicUsize,
    streams_closed: AtomicUsize,
    violations: Mutex<Vec<String>>,
}

impl Server {
    fn new(offers_events: bool, delivery: &[&'static str]) -> Arc<Self> {
        Arc::new(Self {
            offers_events,
            delivery: delivery.to_vec(),
            job: Mutex::new("working"),
            status_calls: AtomicUsize::new(0),
            log: Mutex::new(Vec::new()),
            published: tokio::sync::watch::channel(0).0,
            drop_streams: AtomicUsize::new(0),
            drop_after_event: AtomicBool::new(false),
            fail_status: AtomicBool::new(false),
            polls: AtomicUsize::new(0),
            terminate: AtomicBool::new(false),
            schema_changes: AtomicUsize::new(0),
            gap: AtomicBool::new(false),
            asked: Mutex::new(Vec::new()),
            streams_opened: AtomicUsize::new(0),
            streams_closed: AtomicUsize::new(0),
            violations: Mutex::new(Vec::new()),
        })
    }

    fn publish(&self, thread: &str) {
        let mut log = self.log.lock().unwrap();
        let n = log.len();
        log.push(json!({
            "eventId": format!("evt_{n}"), "name": EVENT, "timestamp": "2026-11-24T10:00:00Z",
            "data": { "type": EVENT, "threadId": thread, "runId": format!("run-{n}"),
                      "outcome": "completed" },
        }));
        self.published.send_replace(log.len());
    }

    fn position(&self, cursor: &Value) -> usize {
        cursor
            .as_str()
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| self.log.lock().unwrap().len())
    }

    fn violation(&self, what: String) {
        self.violations.lock().unwrap().push(what);
    }

    async fn serve(self: Arc<Self>) -> String {
        let router = axum::Router::new()
            .route("/mcp", axum::routing::post(handle))
            .with_state(self);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}/mcp")
    }
}

/// Counts a push stream as closed once the client drops it.
struct Open(Arc<Server>);

impl Drop for Open {
    fn drop(&mut self) {
        self.0.streams_closed.fetch_add(1, Ordering::SeqCst);
    }
}

fn reply(id: &Value, result: Value) -> Response {
    axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn notification(method: &str, mut params: Value, id: &Value) -> Event {
    params["_meta"] = json!({ "io.modelcontextprotocol/subscriptionId": id });
    Event::default()
        .data(json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string())
}

async fn handle(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    axum::Json(message): axum::Json<Value>,
) -> Response {
    let Some(id) = message.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let method = message["method"].as_str().unwrap_or_default().to_owned();
    let params = message["params"].clone();
    if method.starts_with("events/") || method == "server/discover" {
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let meta = &params["_meta"];
        if header("mcp-protocol-version") != Some("2026-07-28")
            || header("mcp-method") != Some(method.as_str())
            || meta["io.modelcontextprotocol/protocolVersion"] != "2026-07-28"
            || !meta["io.modelcontextprotocol/clientCapabilities"].is_object()
            || (method != "events/list"
                && method != "server/discover"
                && header("mcp-name") != params["name"].as_str())
        {
            server.violation(format!("{method}: {headers:?} {params}"));
        }
    }
    match method.as_str() {
        "initialize" => reply(
            &id,
            json!({ "protocolVersion": params["protocolVersion"], "capabilities": { "tools": {} },
                    "serverInfo": { "name": "t3", "version": "1" } }),
        ),
        "tools/list" => reply(
            &id,
            json!({ "tools": [{ "name": "task_status", "inputSchema": { "type": "object" },
                                "annotations": { "readOnlyHint": true } }] }),
        ),
        "tools/call" => {
            server.status_calls.fetch_add(1, Ordering::SeqCst);
            if server.fail_status.load(Ordering::SeqCst) {
                return reply(&id, json!({ "isError": true, "content": [] }));
            }
            let work = *server.job.lock().unwrap();
            reply(
                &id,
                json!({ "content": [], "structuredContent": { "workState": work } }),
            )
        }
        "server/discover" if server.offers_events => reply(
            &id,
            json!({ "resultType": "complete", "supportedVersions": ["2026-07-28"],
                    "capabilities": { "tools": {}, "extensions": { "io.modelcontextprotocol/events": {} } },
                    "ttlMs": 0, "cacheScope": "private" }),
        ),
        "server/discover" => axum::Json(json!({ "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": "Method not found" } }))
        .into_response(),
        "events/list" => reply(
            &id,
            json!({ "resultType": "complete", "events": [{
                "name": EVENT, "description": "A thread's turn ended.", "delivery": server.delivery,
                "inputSchema": { "type": "object", "properties": { "threadIds": { "type": "array" } } },
                "payloadSchema": { "type": "object" },
            }] }),
        ),
        "events/poll" => {
            server.polls.fetch_add(1, Ordering::SeqCst);
            server.asked.lock().unwrap().push(params["cursor"].clone());
            let log = server.log.lock().unwrap().clone();
            let from = server.position(&params["cursor"]).min(log.len());
            let fresh = params["cursor"].is_null();
            let events = if fresh { vec![] } else { log[from..].to_vec() };
            reply(
                &id,
                json!({ "resultType": "complete", "events": events, "cursor": log.len().to_string(),
                        "truncated": server.gap.swap(false, Ordering::SeqCst),
                        "hasMore": false, "nextPollMs": 200 }),
            )
        }
        "events/stream" => {
            server.asked.lock().unwrap().push(params["cursor"].clone());
            server.streams_opened.fetch_add(1, Ordering::SeqCst);
            let start = server.position(&params["cursor"]);
            let drop_now = server
                .drop_streams
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            let schema_changed = !params["cursor"].is_null()
                && server
                    .schema_changes
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok();
            let terminate = server.terminate.load(Ordering::SeqCst) || schema_changed;
            let changes = server.published.subscribe();
            let open = Open(server.clone());
            let state = (open, id, start, false, changes);
            let events = stream::unfold(
                state,
                move |(open, id, at, active, mut changes)| async move {
                    let server = open.0.clone();
                    if !active {
                        let confirm = notification(
                            "notifications/events/active",
                            json!({ "cursor": at.to_string(), "truncated": false, "heartbeatMs": 1000 }),
                            &id,
                        );
                        return Some((
                            Ok::<_, std::convert::Infallible>(confirm),
                            (open, id, at, true, changes),
                        ));
                    }
                    if drop_now {
                        return None;
                    }
                    if terminate {
                        let error = if schema_changed {
                            json!({ "code": -32026, "message": "Unsupported",
                                    "data": { "feature": "payloadSchema", "reason": "schema_changed" } })
                        } else {
                            json!({ "code": -32023, "message": "NotFound", "data": { "kind": "event" } })
                        };
                        let end = notification(
                            "notifications/events/terminated",
                            json!({ "error": error }),
                            &id,
                        );
                        return Some((Ok(end), (open, id, usize::MAX, true, changes)));
                    }
                    if at == usize::MAX {
                        return None;
                    }
                    loop {
                        let next = server.log.lock().unwrap().get(at).cloned();
                        if let Some(event) = next {
                            let mut params = event;
                            params["cursor"] = json!((at + 1).to_string());
                            let sent = notification("notifications/events/event", params, &id);
                            let after = if server.drop_after_event.swap(false, Ordering::SeqCst) {
                                usize::MAX
                            } else {
                                at + 1
                            };
                            return Some((Ok(sent), (open, id, after, true, changes)));
                        }
                        if tokio::time::timeout(Duration::from_secs(1), changes.changed())
                            .await
                            .is_err()
                        {
                            let beat = notification(
                                "notifications/events/heartbeat",
                                json!({ "cursor": at.to_string() }),
                                &id,
                            );
                            return Some((Ok(beat), (open, id, at, true, changes)));
                        }
                    }
                },
            );
            Sse::new(events).into_response()
        }
        _ => reply(&id, json!({})),
    }
}

fn installed(url: &str) -> Value {
    json!({"id":"t3-code","name":"T3 Code","description":"Delegate coding tasks","skill":"Check tasks with task_status.","documentation":"task_status {taskId}","config":{"type":"http","url":url}})
}

fn event_wait() -> Value {
    let mut args = wait_args();
    args["event"] = json!({ "name": EVENT, "arguments": { "threadIds": ["t1"] },
                            "match": { "/threadId": "t1" } });
    args
}

async fn event_row(app: &TestApp, turn: Uuid) -> (String, Option<String>, Option<String>) {
    sqlx::query_as("SELECT status, event_status, event_cursor FROM mcp_waits WHERE turn_id = ?1")
        .bind(turn)
        .fetch_one(&app.db.pool)
        .await
        .unwrap()
}

async fn until_count(what: &str, counter: &AtomicUsize, at_least: usize) {
    common::until(
        what,
        common::poll(|| async { (counter.load(Ordering::SeqCst) >= at_least).then_some(()) }),
    )
    .await;
}

/// Park the main bot on an event wait against `server`. Returns the
/// harness, bot, turn, the park result and what `events/list` listed.
async fn parked(server: &Arc<Server>) -> (common::recorder::Harness, String, Uuid, Value, Value) {
    let url = server.clone().serve().await;
    let h = harness(TestApp::builder()).await;
    let main = h.app.main_bot().await;
    let (turn, token) = h.running(&main, GATE).await;
    tool(&h.app, &token, "mcp_install", installed(&url))
        .await
        .ok();
    let listed = tool(
        &h.app,
        &token,
        "mcp_request",
        json!({ "id": "t3-code", "method": "events/list", "params": {} }),
    )
    .await;
    assert!(!listed.error, "{}", listed.body);
    let parked = tool(&h.app, &token, "mcp_wait", event_wait()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    h.app.wait_turn(turn, &["waiting"]).await;
    (
        h,
        main,
        turn,
        parked.result,
        listed.result["result"].clone(),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_wait_settles_when_its_event_arrives_without_status_calls() {
    let server = Server::new(true, &["webhook", "push", "poll"]);
    let (h, _, turn, parked, listed) = parked(&server).await;
    let app = &h.app;
    assert_eq!(listed["events"][0]["name"], EVENT, "{listed}");
    assert_eq!(parked["event"]["status"], "watching", "{parked}");
    until_count("the push stream opens", &server.streams_opened, 1).await;

    server.publish("t2");
    common::until(
        "the other thread's event advances the cursor",
        common::poll(|| async {
            (event_row(app, turn).await.2.as_deref() == Some("1")).then_some(())
        }),
    )
    .await;
    assert_eq!(
        event_row(app, turn).await.0,
        "pending",
        "another thread's run"
    );
    check_now(app).await;
    common::never_polled(
        "a status call while the subscription is live",
        Duration::from_secs(3),
        || async { (server.status_calls.load(Ordering::SeqCst) > 1).then_some(()) },
    )
    .await;

    server.publish("t1");
    app.wait_turn(turn, &["completed"]).await;
    let (status, delivered, _) = wait_row(app, turn).await;
    assert_eq!((status.as_str(), delivered), ("finished", Some(2)));
    assert_eq!(
        server.status_calls.load(Ordering::SeqCst),
        1,
        "only the call inside mcp_wait checked the status"
    );
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("run-1"),
        "the event is the result: {packet}"
    );
    until_count(
        "the settled wait ends its stream",
        &server.streams_closed,
        1,
    )
    .await;
    assert_eq!(*server.violations.lock().unwrap(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_stream_resumes_from_the_saved_cursor_without_missing_the_event() {
    let server = Server::new(true, &["push", "poll"]);
    let (h, _, turn, _, _) = parked(&server).await;
    let app = &h.app;
    until_count("the push stream opens", &server.streams_opened, 1).await;
    server.drop_after_event.store(true, Ordering::SeqCst);
    server.publish("t2");
    until_count("the first stream ends", &server.streams_closed, 1).await;
    until_count("the stream reopens", &server.streams_opened, 2).await;
    server.publish("t1");
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
    let asked = server.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [Value::Null, json!("0"), json!("1")],
        "mcp_wait started at the head, and the second stream resumed after t2's event"
    );
    let packet = h.packets().await.last().unwrap().clone();
    assert!(
        packet.contains("run-1"),
        "t1's event is the result: {packet}"
    );
    assert_eq!(server.status_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn push_streams_that_end_at_once_give_way_to_polling() {
    let server = Server::new(true, &["push", "poll"]);
    server.drop_streams.store(3, Ordering::SeqCst);
    let (h, _, turn, _, _) = parked(&server).await;
    let app = &h.app;
    until_count("polling takes over", &server.polls, 2).await;
    assert_eq!(server.streams_opened.load(Ordering::SeqCst), 3);
    server.publish("t1");
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
    assert_eq!(event_row(app, turn).await.1.as_deref(), Some("watching"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_event_wait_resumes_after_a_restart() {
    let server = Server::new(true, &["push", "poll"]);
    let (h, _, turn, _, _) = parked(&server).await;
    until_count("the push stream opens", &server.streams_opened, 1).await;
    let db = h.app.stop().await;
    server.publish("t1");
    let h = harness(TestApp::builder().db(db)).await;
    let app = &h.app;
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
    assert_eq!(server.asked.lock().unwrap().last(), Some(&json!("0")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_without_events_keeps_checking_the_status_tool() {
    let server = Server::new(false, &[]);
    let (h, _, turn, parked, listed) = parked(&server).await;
    let app = &h.app;
    assert_eq!(listed["events"], json!([]), "{listed}");
    assert!(listed["unsupported"].is_string(), "{listed}");
    assert_eq!(parked["event"]["status"], "unavailable", "{parked}");
    assert_eq!(event_row(app, turn).await.1.as_deref(), Some("dropped"));
    *server.job.lock().unwrap() = "result_available";
    check_now(app).await;
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
    assert_eq!(server.status_calls.load(Ordering::SeqCst), 2);
    assert_eq!(server.streams_opened.load(Ordering::SeqCst), 0);
}

async fn until_event_status(app: &TestApp, turn: Uuid, status: &str) {
    common::until(
        &format!("the event wait is {status}"),
        common::poll(|| async {
            (event_row(app, turn).await.1.as_deref() == Some(status)).then_some(())
        }),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_gap_checks_the_status_until_a_check_sees_the_job_running() {
    let server = Server::new(true, &["poll"]);
    let (h, _, turn, _, _) = parked(&server).await;
    let app = &h.app;
    server.fail_status.store(true, Ordering::SeqCst);
    server.gap.store(true, Ordering::SeqCst);
    until_count("the gap makes a status call due", &server.status_calls, 2).await;
    common::never_polled(
        "a failed status call closes the gap",
        Duration::from_secs(1),
        || async {
            let status = event_row(app, turn).await.1;
            (status.as_deref() != Some("gap")).then_some(status)
        },
    )
    .await;

    server.fail_status.store(false, Ordering::SeqCst);
    check_now(app).await;
    until_event_status(app, turn, "watching").await;
    let calls = server.status_calls.load(Ordering::SeqCst);
    check_now(app).await;
    common::never_polled(
        "a status call once the gap closed",
        Duration::from_secs(3),
        || async { (server.status_calls.load(Ordering::SeqCst) > calls).then_some(()) },
    )
    .await;
    server.publish("t1");
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
    assert_eq!(server.streams_opened.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_subscription_the_server_ends_switches_to_polling_when_offered() {
    let server = Server::new(true, &["push", "poll"]);
    server.terminate.store(true, Ordering::SeqCst);
    let (h, _, turn, _, _) = parked(&server).await;
    let app = &h.app;
    until_count("polling takes over", &server.polls, 2).await;
    server.publish("t1");
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
    assert_eq!(server.streams_opened.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscription_the_server_ends_falls_back_to_status_calls() {
    let server = Server::new(true, &["push"]);
    server.terminate.store(true, Ordering::SeqCst);
    let (h, _, turn, _, _) = parked(&server).await;
    let app = &h.app;
    until_event_status(app, turn, "dropped").await;
    *server.job.lock().unwrap() = "result_available";
    check_now(app).await;
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_or_expired_event_wait_ends_its_subscription_and_ignores_later_events() {
    let server = Server::new(true, &["push"]);
    let (h, main, turn, parked, _) = parked(&server).await;
    let app = &h.app;
    until_count("the push stream opens", &server.streams_opened, 1).await;
    let wait = parked["wait_id"].as_str().unwrap();
    let stopped = app
        .owner(
            "POST",
            &format!("/v1/bots/{main}/waits/{wait}/cancel"),
            None,
        )
        .await;
    assert_eq!(stopped.status, StatusCode::OK, "{}", stopped.body);
    app.wait_turn(turn, &["completed"]).await;
    let id: Uuid = wait.parse().unwrap();
    let settled = mcp_waits::settle_on_event(&app.db.pool, id, "{}")
        .await
        .unwrap();
    assert_eq!(settled, mcp_waits::EventChange::Ended, "Stop came first");
    until_count("Stop ends the stream", &server.streams_closed, 1).await;
    server.publish("t1");
    common::never_polled(
        "a later event settles the stopped wait",
        Duration::from_secs(1),
        || async {
            let status = wait_row(app, turn).await.0;
            (status != "stopped").then_some(status)
        },
    )
    .await;

    let (turn, token) = h.running(&main, GATE).await;
    let parked = tool(app, &token, "mcp_wait", event_wait()).await;
    assert_eq!(parked.result["status"], "waiting", "{}", parked.body);
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;
    until_count("the second wait's stream opens", &server.streams_opened, 2).await;
    let late: Uuid = parked.result["wait_id"].as_str().unwrap().parse().unwrap();
    sqlx::query(
        "UPDATE mcp_waits SET deadline_at = '2000-01-01T00:00:00.000+00:00' WHERE turn_id = ?1",
    )
    .bind(turn)
    .execute(&app.db.pool)
    .await
    .unwrap();
    let settled = mcp_waits::settle_on_event(&app.db.pool, late, "{}")
        .await
        .unwrap();
    assert_eq!(
        settled,
        mcp_waits::EventChange::Ended,
        "the deadline passed"
    );
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "expired");
    until_count("expiry ends the stream", &server.streams_closed, 2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reply_carries_the_same_event_wait_and_parks_anew_on_a_different_event() {
    let server = Server::new(true, &["push", "poll"]);
    let url = server.clone().serve().await;
    let h = harness(TestApp::builder()).await;
    let app = &h.app;
    let main = app.main_bot().await;
    let (turn, token) = h.running(&main, GATE_EVERY_ATTEMPT).await;
    tool(app, &token, "mcp_install", installed(&url)).await.ok();
    let parked = tool(app, &token, "mcp_wait", event_wait()).await.result;
    assert_eq!(parked["status"], "waiting", "{parked}");
    h.release(turn).await;
    app.wait_turn(turn, &["waiting"]).await;

    follow(app, &main, turn).await;
    let token = h.continued(turn, &token).await;
    let same = tool(app, &token, "mcp_wait", event_wait()).await;
    assert_eq!(same.result["wait_id"], parked["wait_id"], "{}", same.body);
    assert_eq!(same.result["event"]["status"], "watching", "{}", same.body);
    h.release(turn).await;
    rests_after(app, turn, 2, "waiting", "the reply carries the wait").await;

    follow(app, &main, turn).await;
    let token = h.continued(turn, &token).await;
    let mut other = event_wait();
    other["event"]["match"] = json!({ "/threadId": "t2" });
    let anew = tool(app, &token, "mcp_wait", other).await;
    assert_eq!(anew.result["status"], "waiting", "{}", anew.body);
    assert_ne!(anew.result["wait_id"], parked["wait_id"], "{}", anew.body);
    h.release(turn).await;
    rests_after(app, turn, 3, "waiting", "the reply parks on the new event").await;
    assert_eq!(
        carried(app, turn).await,
        vec![(1, "pending".into()), (3, "pending".into())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn signing_the_mcp_out_ends_its_event_stream_and_a_new_one_settles_the_wait() {
    let server = Server::new(true, &["push"]);
    let (h, main, turn, _, _) = parked(&server).await;
    let app = &h.app;
    until_count(
        "the bootstrap and watcher streams open",
        &server.streams_opened,
        2,
    )
    .await;
    until_count("the bootstrap stream ends", &server.streams_closed, 1).await;
    let signed_out = app
        .owner(
            "DELETE",
            &format!("/v1/bots/{main}/mcp/t3-code/connection"),
            None,
        )
        .await;
    assert_eq!(
        signed_out.status,
        StatusCode::NO_CONTENT,
        "{}",
        signed_out.body
    );
    until_count("the watcher's stream ends", &server.streams_closed, 2).await;
    until_count(
        "a stream opens with the new sign-in",
        &server.streams_opened,
        3,
    )
    .await;
    server.publish("t1");
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_event_schema_restarts_from_the_head_and_checks_the_status() {
    let server = Server::new(true, &["push"]);
    server.publish("t2");
    server.schema_changes.store(1, Ordering::SeqCst);
    let (h, _, turn, _, _) = parked(&server).await;
    let app = &h.app;
    until_count("the restart checks the status", &server.status_calls, 2).await;
    until_event_status(app, turn, "watching").await;
    let asked = server.asked.lock().unwrap().clone();
    assert_eq!(
        asked,
        [Value::Null, json!("1"), Value::Null, json!("1")],
        "the second stream starts from the head the restart took"
    );
    server.publish("t1");
    app.wait_turn(turn, &["completed"]).await;
    assert_eq!(wait_row(app, turn).await.0, "finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_status_check_chosen_before_a_gap_leaves_the_gap_due() {
    let server = Server::new(true, &["poll"]);
    let (h, _, turn, parked, _) = parked(&server).await;
    let db = h.app.stop().await;
    let pool = &db.pool;
    let id: Uuid = parked["wait_id"].as_str().unwrap().parse().unwrap();
    let stale = mcp_waits::Due {
        id,
        mcp_id: "t3-code".into(),
        tool: "task_status".into(),
        arguments: json!({ "taskId": "WEB-284" }),
        status_pointer: "/workState".into(),
        running_values: vec![json!("working")],
        gap: None,
    };
    let opened = mcp_waits::open_gap(pool, id, Some("9"), false)
        .await
        .unwrap();
    assert_eq!(
        opened,
        mcp_waits::EventChange::Applied(mcp_waits::EventStatus::Gap)
    );
    let running = mcp_waits::Check::Running(r#"{"workState":"working"}"#.into());
    assert!(!mcp_waits::record(pool, &stale, &running).await.unwrap());
    let (status, cursor, due): (Option<String>, Option<String>, bool) = sqlx::query_as(
        "SELECT event_status, event_cursor,
             next_check_at <= strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')
         FROM mcp_waits WHERE turn_id = ?1",
    )
    .bind(turn)
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        (status.as_deref(), cursor.as_deref(), due),
        (Some("gap"), Some("9"), true)
    );
}
