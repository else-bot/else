//! Stopping the API while desktop streams are open.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_releases_held_input_and_refuses_new_streams() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=control").await;
    next_json(&mut socket).await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"control"})
    );
    send(
        &mut socket,
        json!({"type":"pointer","x":3,"y":4,"buttons":1}),
    )
    .await;
    send(&mut socket, json!({"type":"key","key":0xff53,"down":true})).await;
    s.wait_log("control key 65363 1").await;
    assert_eq!(s.leases("desktop-control:%").await, 1);
    assert_eq!(s.live_viewer_leases().await, 1);

    common::until(
        "the desktop streams to stop",
        s.app.state.stop_desktop_streams(),
    )
    .await;
    assert_eq!(
        s.leases("desktop-control:%").await,
        0,
        "bot input and browser actions are no longer deferred"
    );
    assert_eq!(
        s.leases("desktop-stream:%").await,
        0,
        "viewer lease released"
    );
    s.wait_log("control key 65363 0").await;
    s.wait_log("control pointer 0 3 4").await;
    s.wait_log("control closed").await;
    let lines = s.log.lines();
    let released = lines.iter().position(|l| l == "control key 65363 0");
    let closed = lines.iter().position(|l| l == "control closed");
    assert!(
        released.is_some() && closed.is_some() && released < closed,
        "held input is released before the control connection closes: {lines:?}"
    );
    ends_with(&mut socket, "server_stopping").await;
    s.wait_closed().await;

    let opened = s.log.lines().len();
    assert_eq!(
        s.connect("mode=control", Some(TOKEN)).await.err(),
        Some(503)
    );
    common::never_polled(
        "activity reaches the desktop after the drain",
        Duration::from_millis(200),
        || {
            let lines = s.log.lines();
            std::future::ready((lines.len() != opened).then_some(lines))
        },
    )
    .await;
    assert_eq!(s.leases("desktop-%").await, 0, "no lease after the drain");
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_abandons_a_stream_waiting_for_boot() {
    let s = setup(false, Duration::from_secs(300)).await;
    let bot_id: Uuid = s.bot.parse().unwrap();
    s.manager.owner_stop(bot_id).await.unwrap();
    s.manager.owner_resume(bot_id).await.unwrap();
    s.hang.store(true, Ordering::SeqCst);
    let mut socket = s.open("mode=control&wake=true").await;
    common::until("the stream to start a boot", s.booting.notified()).await;
    assert_eq!(
        s.leases("desktop-stream:%").await,
        1,
        "the boot granted a lease"
    );

    common::until(
        "the API to stop the stream during boot",
        s.app.state.stop_desktop_streams(),
    )
    .await;
    ends_with(&mut socket, "server_stopping").await;
    assert_eq!(s.leases("desktop-%").await, 0);
    assert!(s.log.lines().is_empty(), "{:?}", s.log.lines());
}

const LEASE_TICK: Duration = Duration::from_millis(1500);

async fn hold_key(s: &Setup, socket: &mut Socket) {
    wait_mode(socket, "control").await;
    send(socket, json!({"type":"key","key":0xff53,"down":true})).await;
    s.wait_log("control key 65363 1").await;
}

fn key_released(s: &Setup) -> bool {
    s.log
        .input("control")
        .iter()
        .any(|l| l == "control key 65363 0")
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_releases_input_a_non_reading_client_holds() {
    let s = setup(false, Duration::from_secs(300)).await;
    let tcp = tokio::net::TcpSocket::new_v4().unwrap();
    tcp.set_recv_buffer_size(1024).unwrap();
    let tcp = tcp.connect(s.addr.parse().unwrap()).await.unwrap();
    let mut request = format!(
        "ws://{}/v1/bots/{}/desktop/stream?mode=control",
        s.addr, s.bot
    )
    .into_client_request()
    .unwrap();
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    let (mut socket, _) =
        tokio_tungstenite::client_async(request, tokio_tungstenite::MaybeTlsStream::Plain(tcp))
            .await
            .unwrap();
    hold_key(&s, &mut socket).await;

    let (mut sink, unread) = socket.split();
    let flood =
        tokio::spawn(
            async move { while sink.send(Message::Text("not json".into())).await.is_ok() {} },
        );
    common::never_polled(
        "the held key is released before shutdown",
        Duration::from_secs(8),
        || std::future::ready(key_released(&s).then_some(())),
    )
    .await;

    // Shutdown must beat the 32 s writer flush limit (WRITE_LIMIT + CLOSE_LIMIT).
    const DRAIN_BEFORE_WRITER_TIMEOUT: Duration = Duration::from_secs(5);
    let drained = tokio::time::timeout(
        DRAIN_BEFORE_WRITER_TIMEOUT,
        s.app.state.stop_desktop_streams(),
    )
    .await
    .is_ok();
    flood.abort();
    drop(unread);
    assert!(
        key_released(&s),
        "a client that stops reading cannot keep a key held"
    );
    assert!(drained, "the stream does not wait on its client");
    assert_eq!(s.leases("desktop-%").await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_releases_input_before_waiting_on_a_busy_database() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=control").await;
    hold_key(&s, &mut socket).await;
    let pool = &s.app.db.pool;
    let mut held = Vec::new();
    // Take every free connection: the pool is exhausted once one acquire
    // waits this long, since the app's own loops keep the rest.
    const POOL_QUIET_WINDOW: Duration = Duration::from_millis(200);
    while let Ok(Ok(connection)) = tokio::time::timeout(POOL_QUIET_WINDOW, pool.acquire()).await {
        held.push(connection);
    }
    assert!(!held.is_empty());
    tokio::time::sleep(LEASE_TICK).await;

    let mut drain = Box::pin(s.app.state.stop_desktop_streams());
    // Input release must beat the pool's 30 s acquire timeout.
    const RELEASE_BEFORE_POOL_TIMEOUT: Duration = Duration::from_secs(3);
    tokio::time::timeout(
        RELEASE_BEFORE_POOL_TIMEOUT,
        common::poll(|| std::future::ready(key_released(&s).then_some(()))),
    )
    .await
    .expect("no database wait precedes the input release");
    common::never(
        "stream shutdown finishes while the database is blocked",
        Duration::from_millis(200),
        &mut drain,
    )
    .await;
    drop(held);
    common::until("streams to finish once the database answers", drain).await;
    assert_eq!(s.leases("desktop-%").await, 0);
    ends_with(&mut socket, "server_stopping").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_api_while_taking_control_leaks_no_lease_or_control() {
    let s = setup(false, Duration::from_secs(300)).await;
    let bot_id: Uuid = s.bot.parse().unwrap();
    let mut socket = s.open("mode=watch").await;
    wait_mode(&mut socket, "watch").await;
    let mut writer = s.app.db.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    sqlx::query("UPDATE bots SET name = name WHERE id = ?1")
        .bind(bot_id)
        .execute(&mut *writer)
        .await
        .unwrap();
    send(&mut socket, json!({"type":"mode","mode":"control"})).await;
    common::never(
        "a reply to the control request while the writer blocks the lease lookup",
        Duration::from_millis(300),
        next_json(&mut socket),
    )
    .await;

    let mut drain = Box::pin(s.app.state.stop_desktop_streams());
    common::never(
        "stream shutdown finishes while the writer blocks the lease lookup",
        Duration::from_millis(300),
        &mut drain,
    )
    .await;
    writer.commit().await.unwrap();
    common::until("the abandoned admission to be cleaned up", drain).await;
    assert_eq!(
        s.leases("desktop-%").await,
        0,
        "no lease outlives the drain"
    );
    assert!(
        s.manager.claim_control(bot_id, Uuid::new_v4()).await,
        "control ownership was released"
    );
    assert!(s.log.input("control").is_empty());
    ends_with(&mut socket, "server_stopping").await;
}
