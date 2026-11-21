use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

use super::{Steered, Step, Unwind};
use crate::app::providers::TurnOutcome;
use crate::db::turns::TurnStatus;

async fn pool() -> SqlitePool {
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    crate::db::migrate(&pool).await.unwrap();
    pool
}

async fn bot(conn: &mut SqliteConnection) -> Uuid {
    let bot = Uuid::new_v4();
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, 'Bot')")
        .bind(bot)
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id) VALUES (?1, ?2)")
        .bind(Uuid::new_v4())
        .bind(bot)
        .execute(&mut *conn)
        .await
        .unwrap();
    bot
}

async fn turn(conn: &mut SqliteConnection, bot: Uuid, turn_no: i64, status: &str) -> Uuid {
    let turn = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id, status)
         SELECT ?1, ?2, id, ?3, zeroblob(32), ?4, ?5, 'codex', ?6
         FROM conversations WHERE bot_id = ?2 AND kind = 'owner'",
    )
    .bind(turn)
    .bind(bot)
    .bind(Uuid::new_v4())
    .bind(turn_no)
    .bind(Uuid::new_v4())
    .bind(status)
    .execute(&mut *conn)
    .await
    .unwrap();
    turn
}

/// A queued task `root` of `parent` handed to a new bot. Returns its
/// child turn and delegation.
async fn task(conn: &mut SqliteConnection, parent: Uuid, root: Uuid) -> (Uuid, Uuid) {
    let target = bot(conn).await;
    let (conversation, child, delegation) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    sqlx::query("INSERT INTO conversations (id, bot_id, kind) VALUES (?1, ?2, 'task')")
        .bind(conversation)
        .bind(target)
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
             owner_item_id, provider_id)
         VALUES (?1, ?2, ?3, ?4, zeroblob(32), 1, ?5, 'codex')",
    )
    .bind(child)
    .bind(target)
    .bind(conversation)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO delegations (id, root_turn_id, parent_attempt, parent_bot_id,
             target_bot_id, task, task_hash, conversation_id, child_turn_id)
         VALUES (?1, ?2, 1, ?3, ?4, 'Check it', zeroblob(32), ?5, ?6)",
    )
    .bind(delegation)
    .bind(root)
    .bind(parent)
    .bind(target)
    .bind(conversation)
    .bind(child)
    .execute(&mut *conn)
    .await
    .unwrap();
    (child, delegation)
}

async fn delegation_status(conn: &mut SqliteConnection, delegation: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM delegations WHERE id = ?1")
        .bind(delegation)
        .fetch_one(&mut *conn)
        .await
        .unwrap()
}

async fn row(conn: &mut SqliteConnection, turn: Uuid) -> (TurnStatus, Option<String>, bool) {
    sqlx::query_as("SELECT status, error_code, finished_at IS NOT NULL FROM turns WHERE id = ?1")
        .bind(turn)
        .fetch_one(&mut *conn)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_claim_starts_a_queued_turn_only_while_the_bot_is_idle() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let first = turn(&mut conn, bot, 1, "queued").await;
    let second = turn(&mut conn, bot, 2, "queued").await;
    assert!(super::claim(&mut conn, first, bot).await.unwrap());
    assert_eq!(row(&mut conn, first).await.0, TurnStatus::Preparing);
    assert!(
        !super::claim(&mut conn, second, bot).await.unwrap(),
        "the bot already has an active turn"
    );
    assert_eq!(row(&mut conn, second).await.0, TurnStatus::Queued);
}

#[tokio::test]
async fn an_attempt_advances_one_step_at_a_time() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let turn = turn(&mut conn, bot, 1, "preparing").await;
    assert!(
        !super::advance(&mut conn, turn, Step::Running)
            .await
            .unwrap(),
        "running needs sending first"
    );
    assert!(
        super::advance(&mut conn, turn, Step::Sending)
            .await
            .unwrap()
    );
    assert!(
        super::advance(&mut conn, turn, Step::Running)
            .await
            .unwrap()
    );
    assert_eq!(row(&mut conn, turn).await.0, TurnStatus::Running);
}

#[tokio::test]
async fn only_a_preparing_turn_fails_unstarted() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let preparing = turn(&mut conn, bot, 1, "preparing").await;
    let (child, delegation) = task(&mut conn, bot, preparing).await;
    super::fail_unstarted(&mut conn, bot, preparing, "no input")
        .await
        .unwrap();
    assert_eq!(
        row(&mut conn, preparing).await,
        (TurnStatus::Failed, Some("start_failed".into()), true)
    );
    assert_eq!(
        row(&mut conn, child).await,
        (TurnStatus::Interrupted, Some("cancelled".into()), true),
        "the task a continuation left pending stops with it"
    );
    assert_eq!(delegation_status(&mut conn, delegation).await, "cancelled");
    let running = turn(&mut conn, bot, 2, "running").await;
    super::fail_unstarted(&mut conn, bot, running, "no input")
        .await
        .unwrap();
    assert_eq!(
        row(&mut conn, running).await,
        (TurnStatus::Running, None, false)
    );
}

#[tokio::test]
async fn a_waiting_turn_with_nothing_held_continues() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let waiting = turn(&mut conn, bot, 1, "waiting").await;
    let running = turn(&mut conn, bot, 2, "running").await;
    assert!(super::continue_waiting(&mut conn, waiting).await.unwrap());
    assert_eq!(row(&mut conn, waiting).await.0, TurnStatus::Queued);
    assert!(!super::continue_waiting(&mut conn, running).await.unwrap());
    assert_eq!(row(&mut conn, running).await.0, TurnStatus::Running);
}

#[tokio::test]
async fn the_owners_stop_keeps_waits_until_a_full_cancel_replaces_it() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let turn = turn(&mut conn, bot, 1, "running").await;
    assert_eq!(super::unwind(&mut conn, turn).await.unwrap(), None);
    let cancelled = super::request_cancel(&mut conn, bot, 1, Unwind::KeepWaits)
        .await
        .unwrap();
    assert_eq!(cancelled, Some(turn));
    assert_eq!(
        super::unwind(&mut conn, turn).await.unwrap(),
        Some(Unwind::KeepWaits)
    );
    assert!(
        super::end_all(&mut conn, turn, "cancelled", "Deleted.")
            .await
            .unwrap()
    );
    assert_eq!(
        super::unwind(&mut conn, turn).await.unwrap(),
        Some(Unwind::EndAll)
    );
    assert!(
        !super::end_all(&mut conn, turn, "cancelled", "Deleted.")
            .await
            .unwrap(),
        "a full cancel does not widen again"
    );
    assert_eq!(
        super::request_cancel(&mut conn, bot, 1, Unwind::EndAll)
            .await
            .unwrap(),
        None,
        "a cancelling turn is not cancelled again"
    );
}

#[tokio::test]
async fn a_halt_parks_or_ends_accepted_turns_up_to_its_boundary() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let queued = turn(&mut conn, bot, 1, "queued").await;
    let waiting = turn(&mut conn, bot, 2, "waiting").await;
    let (_, delegation) = task(&mut conn, bot, waiting).await;
    let later = turn(&mut conn, bot, 3, "queued").await;
    assert!(
        super::park_on_tasks(&mut conn, bot, 2)
            .await
            .unwrap()
            .is_empty()
    );
    super::interrupt_accepted(&mut conn, bot, 2, Unwind::KeepWaits, "Stopped.")
        .await
        .unwrap();
    assert_eq!(
        row(&mut conn, queued).await,
        (TurnStatus::Interrupted, Some("cancelled".into()), true)
    );
    assert_eq!(
        row(&mut conn, waiting).await.0,
        TurnStatus::Waiting,
        "the owner's Stop leaves a turn parked on jobs and tasks alone"
    );
    assert_eq!(
        delegation_status(&mut conn, delegation).await,
        "queued",
        "and its task keeps going"
    );
    super::interrupt_accepted(&mut conn, bot, 2, Unwind::EndAll, "Deleted.")
        .await
        .unwrap();
    assert_eq!(
        row(&mut conn, waiting).await,
        (TurnStatus::Interrupted, Some("cancelled".into()), true)
    );
    assert_eq!(delegation_status(&mut conn, delegation).await, "cancelled");
    assert_eq!(
        row(&mut conn, later).await.0,
        TurnStatus::Queued,
        "after the boundary"
    );
}

#[tokio::test]
async fn a_task_cancel_ends_an_idle_turn_and_interrupts_a_running_one() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let queued = turn(&mut conn, bot, 1, "queued").await;
    let running = turn(&mut conn, bot, 2, "running").await;
    let done = turn(&mut conn, bot, 3, "completed").await;
    assert!(
        super::cancel_task(&mut conn, queued, bot, "cancelled", "Stop.")
            .await
            .unwrap()
    );
    assert_eq!(
        row(&mut conn, queued).await,
        (TurnStatus::Interrupted, Some("cancelled".into()), true)
    );
    assert!(
        super::cancel_task(&mut conn, running, bot, "cancelled", "Stop.")
            .await
            .unwrap()
    );
    assert_eq!(
        row(&mut conn, running).await,
        (TurnStatus::Cancelling, Some("cancelled".into()), false)
    );
    assert!(
        !super::cancel_task(&mut conn, done, bot, "cancelled", "Stop.")
            .await
            .unwrap()
    );
    assert_eq!(row(&mut conn, done).await.0, TurnStatus::Completed);
}

#[tokio::test]
async fn an_attempt_end_settles_only_an_active_turn() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let running = turn(&mut conn, bot, 1, "running").await;
    super::finish_attempt(&mut conn, bot, running, 1, &TurnOutcome::Completed)
        .await
        .unwrap();
    assert_eq!(
        row(&mut conn, running).await,
        (TurnStatus::Completed, None, true)
    );
    let interrupted = turn(&mut conn, bot, 2, "interrupted").await;
    super::finish_attempt(&mut conn, bot, interrupted, 1, &TurnOutcome::Completed)
        .await
        .unwrap();
    assert_eq!(row(&mut conn, interrupted).await.0, TurnStatus::Interrupted);
}

#[tokio::test]
async fn an_unconfirmed_joined_message_ends_uncertain_once() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let queued = turn(&mut conn, bot, 1, "queued").await;
    let done = turn(&mut conn, bot, 2, "completed").await;
    super::end_steered(&mut conn, bot, queued, Steered::Unconfirmed)
        .await
        .unwrap();
    assert_eq!(
        row(&mut conn, queued).await,
        (TurnStatus::Uncertain, Some("steer_uncertain".into()), true)
    );
    super::end_steered(&mut conn, bot, done, Steered::Unconfirmed)
        .await
        .unwrap();
    assert_eq!(
        row(&mut conn, done).await,
        (TurnStatus::Completed, None, false)
    );
}

#[tokio::test]
async fn a_restore_ends_every_unfinished_social_turn() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let queued = turn(&mut conn, bot, 1, "queued").await;
    let preparing = turn(&mut conn, bot, 2, "preparing").await;
    let other = self::bot(&mut conn).await;
    let running = turn(&mut conn, other, 1, "running").await;
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut conn)
        .await
        .unwrap();
    for (event, bot, turn) in [
        ("a", bot, queued),
        ("b", bot, preparing),
        ("c", other, running),
    ] {
        sqlx::query(
            "INSERT INTO social_deliveries
                 (connection_id, event_id, bot_id, trigger_id, destination, message_id, turn_id)
             VALUES (?1, ?2, ?3, ?1, 'chat', 'm', ?4)",
        )
        .bind(Uuid::nil())
        .bind(event)
        .bind(bot)
        .bind(turn)
        .execute(&mut conn)
        .await
        .unwrap();
    }
    super::end_restored(&mut conn, super::Restored::Social)
        .await
        .unwrap();
    for turn in [queued, preparing] {
        assert_eq!(
            row(&mut conn, turn).await,
            (
                TurnStatus::Interrupted,
                Some("social_restored".into()),
                true
            ),
            "a turn that never reached its provider is not resumed"
        );
    }
    assert_eq!(
        row(&mut conn, running).await,
        (TurnStatus::Uncertain, Some("social_restored".into()), true),
        "a turn that may have replied is never resent"
    );
}

async fn join(conn: &mut SqliteConnection, message: Uuid, into: Uuid) {
    sqlx::query("INSERT INTO turn_steers (turn_id, into_turn_id) VALUES (?1, ?2)")
        .bind(message)
        .bind(into)
        .execute(&mut *conn)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_message_joined_to_a_turn_that_fails_unstarted_runs_on_its_own() {
    let mut conn = pool().await.acquire().await.unwrap().detach();
    let bot = bot(&mut conn).await;
    let reply = turn(&mut conn, bot, 1, "preparing").await;
    let message = turn(&mut conn, bot, 2, "queued").await;
    join(&mut conn, message, reply).await;
    super::fail_unstarted(&mut conn, bot, reply, "no input")
        .await
        .unwrap();
    assert!(super::claim(&mut conn, message, bot).await.unwrap());
}

#[tokio::test]
async fn a_message_joined_to_a_task_the_owner_stops_runs_on_its_own() {
    let pool = pool().await;
    let mut conn = pool.acquire().await.unwrap();
    let parent = bot(&mut conn).await;
    let root = turn(&mut conn, parent, 1, "waiting").await;
    let (child, delegation) = task(&mut conn, parent, root).await;
    let target: Uuid = sqlx::query_scalar("SELECT bot_id FROM turns WHERE id = ?1")
        .bind(child)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    let message = turn(&mut conn, target, 1, "queued").await;
    join(&mut conn, message, child).await;
    drop(conn);
    crate::db::waits::stop(&pool, parent, delegation)
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(row(&mut conn, child).await.0, TurnStatus::Interrupted);
    assert!(super::claim(&mut conn, message, target).await.unwrap());
}
