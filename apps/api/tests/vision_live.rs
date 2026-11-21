mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use uuid::Uuid;

use common::live::{self, Install, finish, guest, note, runtime_name, turn_activity};

/// The model a provider runs, from `ELSE_LIVE_VISION_<PROVIDER>_MODEL`.
fn model(provider: &str, default: &str) -> String {
    std::env::var(format!("ELSE_LIVE_VISION_{provider}_MODEL")).unwrap_or_else(|_| default.into())
}
fn marker() -> String {
    const WORDS: &[&str] = &[
        "MAPLE", "COBALT", "PEBBLE", "LANTERN", "QUARTZ", "WILLOW", "EMBER", "HARBOR",
    ];
    let id = Uuid::new_v4();
    let b = id.as_bytes();
    format!(
        "{}-{}",
        WORDS[b[0] as usize % WORDS.len()],
        1000 + (u16::from_be_bytes([b[1], b[2]]) % 9000)
    )
}

fn draw(home: &Path, name: &str, word: &str) {
    guest(
        home,
        name,
        "pkill -f '[x]term -title note' || true",
        None,
        false,
    );
    guest(
        home,
        name,
        "umask 077 && cat > /tmp/.note",
        Some(word),
        false,
    );
    guest(
        home,
        name,
        "(DISPLAY=:99 /usr/bin/setsid xterm -title note -geometry 16x3+760+560 -fa 'DejaVu Sans Mono' -fs 28 \\
         -bg yellow -fg black -e sh -c 'w=$(cat /tmp/.note); rm -f /tmp/.note; printf \"\\\\n %s\" \"$w\"; exec sleep 1800' \\
         > /dev/null 2>&1 < /dev/null &); sleep 2; ls /tmp/.note 2>/dev/null | wc -l",
        None,
        false,
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn both_providers_read_pixels_from_their_desktop() {
    if !live::enabled("ELSE_LIVE_VISION") {
        eprintln!("SKIPPED: set ELSE_LIVE_VISION=1 for the POC-028 live run");
        return;
    }
    let started = Instant::now();
    let dir = tempfile::tempdir().unwrap();
    let log = live::log_to("vision");
    let db = common::TestDb::empty().await;
    let data = dir.path().join("installation");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700)).unwrap();
    let home = data.join("microsandbox");
    std::fs::create_dir_all(&home).unwrap();
    let _cleanup = live::Computers(home.clone());
    live::load_image(&home, dir.path());
    let install = Install {
        data: data.clone(),
        database_url: db.url.expose().to_owned(),
        log: log.clone(),
    };
    install.run(&["setup"]);
    let api = install.serve().await;
    let pool = db.pool.clone();
    for (provider, body) in [
        (
            "codex",
            json!({ "model": model("CODEX", "gpt-6.1-sol"), "reasoning_effort": "low", "service_tier": "default" }),
        ),
        (
            "claude",
            json!({ "model": model("CLAUDE", "claude-haiku-5-5"), "reasoning_effort": "low", "service_tier": "default" }),
        ),
    ] {
        let (status, body) = api
            .call(
                reqwest::Method::PATCH,
                &format!("/v1/settings/providers/{provider}"),
                Some(body),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        note(&format!("{provider} profile: {}", body["profile"]));
    }
    let (_, body) = api
        .call(
            reqwest::Method::POST,
            "/v1/bots",
            Some(json!({ "request_id": Uuid::new_v4(), "name": "Seer" })),
        )
        .await;
    let bot = body["bot"]["id"].as_str().unwrap().to_owned();
    let long = Duration::from_secs(600);

    note("== 1. the computer starts; the capture matches the display VNC serves");
    let turn = api
        .send(
            &bot,
            "codex",
            "Run `else-desktop` with computer_run and reply OK.",
        )
        .await;
    let (status, _) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed");
    let name = runtime_name(&pool, &bot).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let geometry = guest(
        &home,
        &name,
        "DISPLAY=:99 xwininfo -root | grep -E 'Width|Height' | tr -s ' ' | cut -d' ' -f3 | tr '\\n' x",
        None,
        false,
    );
    note(&format!("display :99 is {geometry}"));
    assert_eq!(geometry, "1280x800x");
    let x11vnc = guest(
        &home,
        &name,
        "tr '\\0' ' ' < /proc/$(pgrep -o x11vnc)/cmdline",
        None,
        true,
    );
    assert!(x11vnc.contains("-display :99"), "{x11vnc}");

    for (provider, word) in [("codex", marker()), ("claude", marker())] {
        note(&format!(
            "== 2. {provider} reads a code word drawn only into pixels"
        ));
        draw(&home, &name, &word);
        let turn = api
            .send(
                &bot,
                provider,
                "Take a screenshot of your computer's desktop with computer_screenshot. A yellow \\
                 window shows a short code word. Reply with exactly that code word and nothing else. \\
                 Do not use any other tool.",
            )
            .await;
        let (status, reply) = finish(&pool, turn, long).await;
        assert_eq!(status, "completed", "{reply}");
        assert!(
            reply.contains(&word),
            "{provider} read {reply:?}, the word was {word}"
        );
        let used = turn_activity(&pool, &bot, turn).await;
        note(&format!("{provider} tool activity: {used:?}"));
        assert!(
            used.iter()
                .any(|a| a.starts_with("computer_screenshot") && a.contains("1280x800"))
        );
        assert!(
            used.iter().all(|a| a.starts_with("computer_screenshot")),
            "only the screenshot"
        );
        assert!(
            used.iter().all(|a| !a.contains(&word)),
            "the word reached the model only as pixels"
        );

        note(&format!(
            "== 3. {provider} resumes its session after the image"
        ));
        let turn = api
            .send(
                &bot,
                provider,
                "What was that code word? Answer from the conversation, without tools.",
            )
            .await;
        let (status, reply) = finish(&pool, turn, long).await;
        assert_eq!(status, "completed", "{reply}");
        assert!(reply.contains(&word), "{reply}");
        let sessions: Vec<(String, String)> = sqlx::query_as(
            "SELECT native_session_id, status FROM provider_sessions
             WHERE bot_id = ?1 AND provider_id = ?2 ORDER BY created_at",
        )
        .bind(bot.parse::<Uuid>().unwrap())
        .bind(provider)
        .fetch_all(&pool)
        .await
        .unwrap();
        note(&format!("{provider} sessions: {sessions:?}"));
        assert_eq!(
            sessions.len(),
            1,
            "one native session across the image and the follow-up"
        );
        assert_eq!(sessions[0].1, "active");
    }

    note("== 4. a missing display is an explicit outcome");
    guest(
        &home,
        &name,
        "supervisorctl -c /etc/elsebot/supervisord.conf stop xvfb",
        None,
        true,
    );
    let turn = api
        .send(&bot, "codex", "Take a screenshot with computer_screenshot and tell me exactly what the tool returned.")
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    let lowered = reply.to_lowercase();
    assert!(lowered.contains("display"), "{reply}");
    guest(
        &home,
        &name,
        "supervisorctl -c /etc/elsebot/supervisord.conf start xvfb openbox",
        None,
        true,
    );

    note("== 5. Stop work after a screenshot was taken; the next turn is normal");
    let turn = api
        .send(
            &bot,
            "claude",
            "Take three screenshots, one at a time, and describe each in detail.",
        )
        .await;
    const SCREENSHOT_WAIT: Duration = Duration::from_secs(120);
    tokio::time::timeout(
        SCREENSHOT_WAIT,
        common::poll(|| async {
            (!turn_activity(&pool, &bot, turn).await.is_empty()).then_some(())
        }),
    )
    .await
    .expect("the bot took a screenshot");
    let (status, body) = api
        .call(
            reqwest::Method::POST,
            &format!("/v1/bots/{bot}/cancel"),
            Some(json!({ "request_id": Uuid::new_v4() })),
        )
        .await;
    assert!(status < 300, "{body}");
    let (status, _) = finish(&pool, turn, long).await;
    note(&format!(
        "stopped turn: {status}, screenshots taken: {}",
        turn_activity(&pool, &bot, turn).await.len()
    ));
    assert_eq!(status, "interrupted");
    assert!(
        turn_activity(&pool, &bot, turn).await.len() < 3,
        "stopped before finishing"
    );
    let turn = common::until("the API to accept the turn after cancellation", async {
        loop {
            let (status, body) = api
                .call(
                    reqwest::Method::POST,
                    &format!("/v1/bots/{bot}/turns"),
                    Some(
                        json!({ "request_id": Uuid::new_v4(), "provider_id": "claude",
                               "text": "Reply with the single word READY." }),
                    ),
                )
                .await;
            if status < 300 {
                break body["turn"]["id"]
                    .as_str()
                    .unwrap()
                    .parse::<Uuid>()
                    .unwrap();
            }
            assert_eq!(status, 409, "{body}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");
    note("== 6. the API dies during a screenshot turn (the provider loses its tools)");
    let mut api = api;
    let turn = api
        .send(
            &bot,
            "codex",
            "Take a screenshot with computer_screenshot, then describe the desktop.",
        )
        .await;
    common::until(
        &format!("turn {turn} to start running"),
        common::poll(|| async {
            let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id = ?1")
                .bind(turn)
                .fetch_one(&pool)
                .await
                .unwrap();
            (status == "running").then_some(())
        }),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    api.kill();
    let api = install.serve().await;
    let (status, _) = finish(&pool, turn, long).await;
    note(&format!("turn cut off by the API's death: {status}"));
    assert_ne!(status, "completed");
    let turn = api
        .send(&bot, "codex", "Reply with the single word AGAIN.")
        .await;
    let (status, reply) = finish(&pool, turn, long).await;
    assert_eq!(status, "completed", "{reply}");

    let violations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM turns WHERE bot_id = ?1 AND error_code IN ('policy_violation', 'profile_mismatch')",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(violations, 0, "no native tools or sub-agents appeared");
    let images: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT text AS t FROM items UNION ALL SELECT body FROM memory_entry) x
         WHERE t LIKE '%iVBORw0KGgo%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(images, 0, "no image data in the conversation or memory");
    assert!(
        !std::fs::read_to_string(&log)
            .unwrap()
            .contains("iVBORw0KGgo"),
        "nor the API log"
    );
    let holders = std::process::Command::new("grep")
        .args(["-rl", "--exclude-dir=microsandbox", "iVBORw0KGgo"])
        .arg(&data)
        .output()
        .unwrap();
    let holders = String::from_utf8_lossy(&holders.stdout)
        .trim()
        .replace(&data.display().to_string(), "$DATA");
    note(&format!("files holding image data: {holders}"));
    for path in holders.lines() {
        assert!(
            path.starts_with("$DATA/bots/") || path.starts_with("$DATA/artifacts/"),
            "{path}"
        );
    }
    note(&format!(
        "POC-028 live run passed in {:?}",
        started.elapsed()
    ));
}
