//! Live checks that a newer owner message joins a provider's running reply
//! mid-tool and mid-text, and both end with everything asked.

use std::time::Duration;

use uuid::Uuid;

use super::{TestApp, poll};

const ENDED: &[&str] = &["completed", "failed", "interrupted", "uncertain"];

/// What happened to a reply and the message that joined it.
pub struct Joined {
    pub reply: String,
    pub message: String,
    /// The message was delivered while the reply's first attempt still ran.
    pub live: bool,
    pub text: String,
}

/// Wait until `turn` has streamed more than `chars` characters.
pub async fn wait_for_text(app: &TestApp, turn: Uuid, chars: i64) {
    const TEXT_WAIT: Duration = Duration::from_secs(120);
    tokio::time::timeout(
        TEXT_WAIT,
        poll(|| async {
            let n: i64 = sqlx::query_scalar(
                "SELECT coalesce(sum(length(text)), 0) FROM items WHERE turn_id = ?1 AND kind = 'assistant'",
            )
            .bind(turn)
            .fetch_one(&app.db.pool)
            .await
            .unwrap();
            (n > chars).then_some(())
        }),
    )
    .await
    .unwrap_or_else(|_| panic!("turn {turn} streamed no text beyond {chars} characters"));
}

/// Send `first` to `provider`, then `second` once `ready` returns for the
/// running reply, and follow both to their end.
pub async fn join<F, Fut>(
    app: &TestApp,
    bot: &str,
    provider: &str,
    first: &str,
    second: &str,
    ready: F,
) -> Joined
where
    F: FnOnce(Uuid) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let sent = app.send(bot, Uuid::new_v4(), first, Some(provider)).await;
    assert!(sent.status.is_success(), "{}", sent.body);
    let one: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    app.wait_live_turn(one, &["running"]).await;
    ready(one).await;
    let sent = app.send(bot, Uuid::new_v4(), second, Some(provider)).await;
    assert!(sent.status.is_success(), "{}", sent.body);
    assert_eq!(
        sent.body["turn"]["steered_into"],
        one.to_string(),
        "the message joins the running reply: {}",
        sent.body
    );
    let two: Uuid = sent.body["turn"]["id"].as_str().unwrap().parse().unwrap();
    let mut live = false;
    const JOIN_WAIT: Duration = Duration::from_secs(300);
    let mut last_status = String::new();
    let reply = tokio::time::timeout(JOIN_WAIT, async {
        loop {
            let (status, attempt, steer): (String, i64, Option<String>) = sqlx::query_as(
            "SELECT t.status, t.attempt, (SELECT s.status FROM turn_steers s WHERE s.turn_id = ?2)
             FROM turns t WHERE t.id = ?1",
        )
        .bind(one)
        .bind(two)
        .fetch_one(&app.db.pool)
        .await
        .unwrap();
            live |= status == "running" && attempt == 1 && steer.as_deref() == Some("delivered");
            if ENDED.contains(&status.as_str()) {
                break status;
            }
            last_status = status;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("turn {one} still {last_status}"));
    let message = app.wait_live_turn(two, ENDED).await;
    let text: String = sqlx::query_scalar(
        "SELECT coalesce(group_concat(text, char(10)), '') FROM (
             SELECT i.text FROM items i JOIN turns t ON t.id = i.turn_id
             WHERE t.bot_id = ?1 AND i.kind = 'assistant' ORDER BY i.seq)",
    )
    .bind(bot.parse::<Uuid>().unwrap())
    .fetch_one(&app.db.pool)
    .await
    .unwrap();
    eprintln!("[{provider}] join -> {reply}/{message}, live={live}: {text}");
    Joined {
        reply,
        message,
        live,
        text,
    }
}

/// A message sent while `provider` runs a shell command, and one sent while
/// it streams text, each join the reply, which answers both. `live` says
/// whether the provider takes a message mid-turn; otherwise the message
/// joins the reply's next continuation in the same session.
pub async fn joins_mid_tool_and_mid_text(app: &TestApp, provider: &str, live: bool) {
    let tool = app.create_bot("Tool").await;
    let joined = join(
        app,
        &tool,
        provider,
        "Synthetic test. Use your shell tool to run `sleep 15 && echo FIRST-DONE`, then \
         reply with its output.",
        "Synthetic test. Also reply with the word BANANA.",
        |_| tokio::time::sleep(Duration::from_secs(6)),
    )
    .await;
    assert_eq!(
        (joined.reply.as_str(), joined.message.as_str()),
        ("completed", "completed")
    );
    assert!(joined.text.contains("FIRST-DONE"), "{}", joined.text);
    assert!(joined.text.contains("BANANA"), "{}", joined.text);
    assert_eq!(joined.live, live, "mid-tool delivery");

    let prose = app.create_bot("Prose").await;
    let joined = join(
        app,
        &prose,
        provider,
        "Synthetic test. Count from 1 to 300, one number per line, then write DONE-COUNTING.",
        "Synthetic test. When you finish, also reply with the word BANANA.",
        |turn| async move { wait_for_text(app, turn, 20).await },
    )
    .await;
    assert_eq!(
        (joined.reply.as_str(), joined.message.as_str()),
        ("completed", "completed")
    );
    assert!(joined.text.contains("DONE-COUNTING"), "{}", joined.text);
    assert!(joined.text.contains("BANANA"), "{}", joined.text);
}
