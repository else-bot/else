//! Controlling a desktop: input, clipboard, and cursor.

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn control_sends_input_releases_it_and_excludes_other_controllers() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=watch").await;
    next_json(&mut socket).await;
    next_json(&mut socket).await;
    send(&mut socket, json!({"type":"mode","mode":"control"})).await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"control"})
    );
    s.wait_log("control open").await;
    assert_eq!(
        s.leases("desktop-control:%").await,
        1,
        "bot input batches and browser actions defer to this lease"
    );

    let mut other = s.open("mode=control").await;
    next_json(&mut other).await;
    assert_eq!(next_json(&mut other).await["code"], "control_in_use");
    assert_eq!(
        next_json(&mut other).await,
        json!({"type":"mode","mode":"watch"})
    );

    send(
        &mut socket,
        json!({"type":"pointer","x":500,"y":7,"buttons":1}),
    )
    .await;
    send(&mut socket, json!({"type":"key","key":0x61,"down":true})).await;
    send(&mut socket, json!({"type":"release"})).await;
    s.wait_log("control pointer 0 63 7").await;
    assert_eq!(
        s.log.input("control"),
        [
            "control pointer 1 63 7",
            "control key 97 1",
            "control key 97 0",
            "control pointer 0 63 7"
        ],
        "input clamped to the screen, then released"
    );

    send(&mut socket, json!({"type":"key","key":0xffe3,"down":true})).await;
    send(&mut socket, json!({"type":"mode","mode":"watch"})).await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );
    // The guest logs the close when it reads it, which can be after the ack.
    s.wait_log("control closed").await;
    let lines = s.log.lines();
    let closed = lines.iter().position(|l| l == "control closed");
    let released = lines.iter().position(|l| l == "control key 65507 0");
    assert!(
        released.is_some() && released < closed,
        "leaving control releases held keys, then closes: {lines:?}"
    );
    send(&mut socket, json!({"type":"key","key":0x62,"down":true})).await;
    assert_eq!(
        s.leases("desktop-control:%").await,
        0,
        "control handed back"
    );

    send(&mut other, json!({"type":"mode","mode":"control"})).await;
    assert_eq!(
        next_json(&mut other).await,
        json!({"type":"mode","mode":"control"})
    );
    send(
        &mut other,
        json!({"type":"pointer","x":1,"y":2,"buttons":4}),
    )
    .await;
    send(&mut other, json!({"type":"key","key":0x63,"down":true})).await;
    s.wait_log("control key 99 1").await;
    drop(other);
    s.wait_log("control key 99 0").await;
    s.wait_log("control pointer 0 1 2").await;
    assert!(
        !s.log.lines().iter().any(|l| l.contains("key 98")),
        "input after leaving control is dropped"
    );
}

fn clipboard_and_keys(s: &Setup) -> Vec<String> {
    s.log
        .lines()
        .into_iter()
        .filter(|l| l.starts_with("control clipboard") || l.starts_with("control key"))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn pasted_text_reaches_the_guest_as_utf8_before_later_keys() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=watch").await;
    next_json(&mut socket).await;
    next_json(&mut socket).await;
    send(&mut socket, json!({"type":"clipboard","text":"watching"})).await;
    send(&mut socket, json!({"type":"mode","mode":"control"})).await;
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"control"})
    );
    s.wait_log("control open").await;

    send(
        &mut socket,
        json!({"type":"clipboard","text":"héllo ‘quoted’ 漢字 🙂\r\nnext"}),
    )
    .await;
    send(&mut socket, json!({"type":"key","key":0x76,"down":true})).await;
    s.wait_log("control key 118 1").await;
    assert_eq!(
        clipboard_and_keys(&s),
        [
            r#"control clipboard "héllo ‘quoted’ 漢字 🙂\nnext""#,
            "control key 118 1"
        ],
        "the text arrives whole, with Unix line ends, before the paste key"
    );

    send(
        &mut socket,
        json!({"type":"clipboard","text":"x".repeat(viewer::MAX_CLIPBOARD_BYTES + 1)}),
    )
    .await;
    assert_eq!(next_json(&mut socket).await["code"], "clipboard_too_large");
    send(&mut socket, json!({"type":"key","key":0x76,"down":false})).await;
    s.wait_log("control key 118 0").await;
    assert_eq!(
        clipboard_and_keys(&s).len(),
        3,
        "text from watch mode and text over the limit never reach the guest"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_older_image_takes_only_latin1_clipboard_text() {
    let s = setup(true, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=control").await;
    assert_eq!(next_json(&mut socket).await["type"], "size");
    assert_eq!(next_json(&mut socket).await["type"], "mode");
    send(&mut socket, json!({"type":"clipboard","text":"café"})).await;
    send(&mut socket, json!({"type":"clipboard","text":"‘quoted’"})).await;
    assert_eq!(
        next_json(&mut socket).await["code"],
        "clipboard_unsupported"
    );
    send(&mut socket, json!({"type":"key","key":0x76,"down":true})).await;
    s.wait_log("control key 118 1").await;
    assert_eq!(
        clipboard_and_keys(&s),
        [r#"control clipboard "café""#, "control key 118 1"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_native_cursor_is_named_only_while_controlling() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=control&cursor=native").await;
    let cursor = common::until("a cursor message", async {
        loop {
            let message = next_json(&mut socket).await;
            if message["type"] == "cursor" {
                return message;
            }
        }
    })
    .await;
    assert_eq!(cursor, json!({"type":"cursor","style":"default"}));
    s.wait_log("watch cursor shapes true").await;
    send(&mut socket, json!({"type":"mode","mode":"watch"})).await;
    s.wait_log("watch cursor shapes false").await;

    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );

    let mut painted = s.open("mode=control").await;
    wait_mode(&mut painted, "control").await;
    common::never_polled(
        "another request for cursor shapes",
        Duration::from_millis(200),
        || {
            let lines = s.log.lines();
            let requests = lines
                .iter()
                .filter(|l| *l == "watch cursor shapes true")
                .count();
            std::future::ready((requests != 1).then_some(lines))
        },
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_control_lease_hands_control_back_and_keeps_watching() {
    let s = setup(false, Duration::from_secs(300)).await;
    let mut socket = s.open("mode=watch").await;
    wait_mode(&mut socket, "watch").await;
    sqlx::query(
        "CREATE TRIGGER refuse_control BEFORE INSERT ON computer_leases
         WHEN NEW.holder LIKE 'desktop-control:%'
         BEGIN SELECT RAISE(ABORT, 'control refused by the test'); END",
    )
    .execute(&s.app.db.pool)
    .await
    .unwrap();

    send(&mut socket, json!({"type":"mode","mode":"control"})).await;
    assert_eq!(next_json(&mut socket).await["code"], "control_failed");
    assert_eq!(
        next_json(&mut socket).await,
        json!({"type":"mode","mode":"watch"})
    );
    assert_eq!(s.leases("desktop-control:%").await, 0);
    assert!(!s.log.lines().iter().any(|l| l == "control open"));

    sqlx::query("DROP TRIGGER refuse_control")
        .execute(&s.app.db.pool)
        .await
        .unwrap();
    let mut other = s.open("mode=control").await;
    assert_eq!(next_json(&mut other).await["type"], "size");
    assert_eq!(
        next_json(&mut other).await,
        json!({"type":"mode","mode":"control"}),
        "the failed attempt released its control claim"
    );
    assert_eq!(s.leases("desktop-control:%").await, 1);
    drop(socket);
}
