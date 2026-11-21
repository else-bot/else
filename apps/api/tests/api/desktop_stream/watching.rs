//! Watching a desktop: authentication, frames, and leases.

use super::*;

async fn next_frame(socket: &mut Socket) -> Vec<u8> {
    common::until("a binary frame", async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Binary(png))) => return png.to_vec(),
                Some(Ok(_)) => {}
                other => panic!("stream ended: {other:?}"),
            }
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stream_authenticates_and_never_wakes_unless_asked() {
    let s = setup(false, Duration::from_secs(300)).await;
    assert_eq!(s.connect("mode=watch", None).await.err(), Some(401));
    assert_eq!(
        s.connect("mode=watch", Some("wrong")).await.err(),
        Some(401)
    );
    assert_eq!(s.connect("mode=look", Some(TOKEN)).await.err(), Some(400));

    let reply = s
        .app
        .owner("GET", &format!("/v1/bots/{}/computer", s.bot), None)
        .await;
    assert_eq!(reply.body["desktop_available"], true, "{:?}", reply.body);

    let new_bot = s.app.create_bot("New viewer").await;
    let absent = s
        .app
        .owner("GET", &format!("/v1/bots/{new_bot}/computer"), None)
        .await;
    assert_eq!(
        absent.body,
        json!({
            "bot_id": new_bot,
            "state": "absent",
            "desired_state": "auto",
            "desktop_available": true,
            "default_memory_mib": 1024,
        })
    );

    let bot_id: Uuid = s.bot.parse().unwrap();
    s.manager.owner_stop(bot_id).await.unwrap();
    s.manager.owner_resume(bot_id).await.unwrap();
    let calls = s.runtime.events().len();
    let mut socket = s.open("mode=watch").await;
    ends_with(&mut socket, "computer_not_running").await;
    assert_eq!(s.runtime.events().len(), calls, "nothing woke");

    s.manager.owner_stop(bot_id).await.unwrap();
    let calls = s.runtime.events().len();
    let mut socket = s.open("mode=watch&wake=true").await;
    ends_with(&mut socket, "computer_stopped").await;
    assert_eq!(s.runtime.events().len(), calls, "nothing woke");

    s.manager.owner_resume(bot_id).await.unwrap();
    let mut socket = s.open("mode=watch&wake=true").await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"size","width":64,"height":48})
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );
    assert!(
        s.runtime.events().iter().any(|c| c.starts_with("Running")),
        "an explicit wake starts the computer"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watching_sends_frames_and_no_input_reaches_the_desktop() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=watch").await;
    assert_eq!(next_json(&mut socket).await["type"], "size");
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );
    let png = next_frame(&mut socket).await;
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "a PNG frame");
    send(
        &mut socket,
        json!({"type":"pointer","x":5,"y":6,"buttons":1}),
    )
    .await;
    send(&mut socket, json!({"type":"key","key":0x61,"down":true})).await;
    send(&mut socket, json!({"type":"release"})).await;
    next_frame(&mut socket).await;
    next_frame(&mut socket).await;
    assert!(s.log.input("watch").is_empty(), "{:?}", s.log.lines());
    assert!(
        !s.log.lines().iter().any(|l| l.starts_with("control")),
        "watching never connects to the controlling server: {:?}",
        s.log.lines()
    );
    assert_eq!(s.live_viewer_leases().await, 1);
    drop(socket);
    s.wait_log("watch closed").await;
    common::until(
        "the viewer lease to end with the stream",
        common::poll(|| async { (s.live_viewer_leases().await == 0).then_some(()) }),
    )
    .await;
    assert_eq!(
        s.live_viewer_leases().await,
        0,
        "the lease ends with the stream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn streams_renew_their_lease_and_end_on_owner_stop_or_delete() {
    let s = setup(false, Duration::from_secs(3)).await;
    let mut socket = s.open("mode=control").await;
    next_json(&mut socket).await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"control"})
    );
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(s.live_viewer_leases().await, 1, "renewed past its TTL");

    send(&mut socket, json!({"type":"key","key":0x61,"down":true})).await;
    s.wait_log("control key 97 1").await;
    let stopped = s
        .app
        .owner("POST", &format!("/v1/bots/{}/computer/stop", s.bot), None)
        .await;
    assert_eq!(stopped.status, axum::http::StatusCode::NO_CONTENT);
    ends_with(&mut socket, "computer_stopped").await;
    s.wait_log("control key 97 0").await;

    s.app
        .owner("POST", &format!("/v1/bots/{}/computer/resume", s.bot), None)
        .await;
    let mut socket = s.open("mode=watch&wake=true").await;
    next_json(&mut socket).await;
    next_json(&mut socket).await;
    s.app
        .owner("DELETE", &format!("/v1/bots/{}", s.bot), None)
        .await;
    ends_with(&mut socket, "bot_deleted").await;
    assert_eq!(s.live_viewer_leases().await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_image_can_be_taken_over_but_not_watched() {
    let s = setup(true, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=watch").await;
    ends_with(&mut socket, "desktop_watch_unsupported").await;
    s.wait_log("control closed").await;

    let mut socket = s.open("mode=control").await;
    assert_eq!(next_json(&mut socket).await["type"], "size");
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"control"})
    );
    assert!(next_frame(&mut socket).await.starts_with(b"\x89PNG"));
    send(&mut socket, json!({"type":"mode","mode":"watch"})).await;
    assert_eq!(
        next_json(&mut socket).await["code"],
        "desktop_watch_unsupported"
    );
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_waking_stream_waits_for_a_backup_but_an_attaching_one_does_not() {
    let s = setup(false, Duration::from_secs(300)).await;
    let bot_id: Uuid = s.bot.parse().unwrap();
    s.manager.owner_stop(bot_id).await.unwrap();
    s.manager.owner_resume(bot_id).await.unwrap();
    let held = s.app.owner("POST", "/v1/maintenance/update", None).await;
    assert_eq!(held.body["ready"], true, "{}", held.body);

    let calls = s.runtime.events().len();
    let mut socket = s.open("mode=watch&wake=true").await;
    ends_with(&mut socket, "maintenance").await;
    let mut socket = s.open("mode=watch").await;
    ends_with(&mut socket, "computer_not_running").await;
    assert_eq!(s.runtime.events().len(), calls, "nothing woke");
    assert_eq!(s.leases("desktop-stream:%").await, 0);

    let reopened = s.app.owner("DELETE", "/v1/maintenance/update", None).await;
    assert_eq!(reopened.body["reopened"], true);
    let mut socket = s.open("mode=watch&wake=true").await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"size","width":64,"height":48})
    );
    let held = s.app.owner("POST", "/v1/maintenance/update", None).await;
    assert_eq!(
        held.body["ready"], true,
        "an open stream lets the gate close"
    );
    drop(socket);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_that_fails_after_its_lease_releases_it() {
    let s = setup(false, Duration::from_secs(300)).await;
    sqlx::query("UPDATE bot_computers SET desktop_generation = NULL WHERE bot_id = ?1")
        .bind(s.bot.parse::<Uuid>().unwrap())
        .execute(&s.app.db.pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_desktop BEFORE UPDATE OF desktop_generation ON bot_computers
         WHEN NEW.desktop_generation IS NOT NULL BEGIN SELECT RAISE(ABORT, 'disk full'); END",
    )
    .execute(&s.app.db.pool)
    .await
    .unwrap();

    let mut socket = s.open("mode=watch").await;
    ends_with(&mut socket, "computer_unavailable").await;
    assert_eq!(s.leases("desktop-stream:%").await, 0);
}
