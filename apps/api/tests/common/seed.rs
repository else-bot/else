//! Typed builders for the base rows a test seeds straight into the database.
//! Each builder fills the columns the test does not care about, so a new
//! `NOT NULL` column on one of these tables is added here once.
//!
//! Migration tests seed a historical schema and keep their raw SQL.

use else_api::db::turns::{ItemKind, TurnStatus};
use sqlx::{Acquire, Sqlite, SqliteExecutor, SqlitePool};
use uuid::Uuid;

/// A seeded bot and the owner conversation every bot has.
#[derive(Debug, Clone, Copy)]
pub struct Bot {
    pub id: Uuid,
    pub owner: Uuid,
}

/// Seed an active bot named `name` with its owner conversation.
pub async fn bot(pool: &SqlitePool, name: &str) -> Bot {
    let bot = Bot {
        id: Uuid::new_v4(),
        owner: Uuid::new_v4(),
    };
    sqlx::query("INSERT INTO bots (id, name) VALUES (?1, ?2)")
        .bind(bot.id)
        .bind(name)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations (id, bot_id, kind) VALUES (?1, ?2, 'owner')")
        .bind(bot.owner)
        .bind(bot.id)
        .execute(pool)
        .await
        .unwrap();
    bot
}

/// Seed a task conversation for `bot`, as a delegation opens one.
pub async fn task_conversation(pool: &SqlitePool, bot: Uuid) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO conversations (id, bot_id, kind) VALUES (?1, ?2, 'task')")
        .bind(id)
        .bind(bot)
        .execute(pool)
        .await
        .unwrap();
    id
}

/// A turn row to seed. It starts `queued` on attempt 0 as the next turn
/// of its bot's owner conversation, on the synthetic provider with a fresh
/// request and owner item ID.
/// Inserting it advances the conversation's `last_turn_no`, as creating a
/// real turn does.
#[must_use]
pub struct Turn {
    id: Uuid,
    bot: Uuid,
    conversation: Option<Uuid>,
    turn_no: Option<i64>,
    status: TurnStatus,
    attempt: i64,
    message: Option<(ItemKind, String)>,
    created_at: Option<String>,
}

pub fn turn(bot: Uuid) -> Turn {
    Turn {
        id: Uuid::new_v4(),
        bot,
        conversation: None,
        turn_no: None,
        status: TurnStatus::Queued,
        attempt: 0,
        message: None,
        created_at: None,
    }
}

impl Turn {
    pub fn id(mut self, id: Uuid) -> Self {
        self.id = id;
        self
    }

    /// Run the turn in another of the bot's conversations, such as a task.
    pub fn conversation(mut self, conversation: Uuid) -> Self {
        self.conversation = Some(conversation);
        self
    }

    pub fn turn_no(mut self, turn_no: i64) -> Self {
        self.turn_no = Some(turn_no);
        self
    }

    pub fn status(mut self, status: TurnStatus) -> Self {
        self.status = status;
        self
    }

    pub fn attempt(mut self, attempt: i64) -> Self {
        self.attempt = attempt;
        self
    }

    /// Also seed the item the turn answers, as the conversation's next item.
    pub fn message(mut self, kind: ItemKind, text: impl Into<String>) -> Self {
        self.message = Some((kind, text.into()));
        self
    }

    /// An RFC 3339 creation time, for tests that order turns by it.
    pub fn created_at(mut self, at: impl Into<String>) -> Self {
        self.created_at = Some(at.into());
        self
    }

    pub async fn insert<'c>(self, db: impl Acquire<'c, Database = Sqlite>) -> Uuid {
        let owner_item = Uuid::new_v4();
        let mut tx = db.begin().await.unwrap();
        let (conversation, turn_no): (Uuid, i64) = sqlx::query_as(
            "WITH c AS (
                 SELECT id, last_turn_no FROM conversations
                 WHERE bot_id = ?2 AND (id = ?3 OR (?3 IS NULL AND kind = 'owner'))
             )
             INSERT INTO turns (id, bot_id, conversation_id, request_id, request_hash, turn_no,
                 owner_item_id, provider_id, status, attempt, created_at)
             SELECT ?1, ?2, c.id, ?4, zeroblob(32), COALESCE(?5, c.last_turn_no + 1), ?6,
                 'synthetic', ?7, ?8, COALESCE(?9, strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now'))
             FROM c
             RETURNING conversation_id, turn_no",
        )
        .bind(self.id)
        .bind(self.bot)
        .bind(self.conversation)
        .bind(Uuid::new_v4())
        .bind(self.turn_no)
        .bind(owner_item)
        .bind(self.status)
        .bind(self.attempt)
        .bind(self.created_at)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        sqlx::query("UPDATE conversations SET last_turn_no = max(last_turn_no, ?2) WHERE id = ?1")
            .bind(conversation)
            .bind(turn_no)
            .execute(&mut *tx)
            .await
            .unwrap();
        if let Some((kind, text)) = self.message {
            sqlx::query("UPDATE conversations SET last_item_seq = last_item_seq + 1 WHERE id = ?1")
                .bind(conversation)
                .execute(&mut *tx)
                .await
                .unwrap();
            sqlx::query(
                "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
                 SELECT ?1, id, last_item_seq, ?3, ?4, ?5 FROM conversations WHERE id = ?2",
            )
            .bind(owner_item)
            .bind(conversation)
            .bind(self.id)
            .bind(kind)
            .bind(text)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        self.id
    }
}

/// Seed a `kind` item for seeded `turn` as the next item of its bot's owner
/// conversation, where the turn sink records every turn's messages, a
/// task's included.
pub async fn item<'c>(
    db: impl Acquire<'c, Database = Sqlite>,
    turn: Uuid,
    kind: ItemKind,
    text: &str,
) -> Uuid {
    let id = Uuid::new_v4();
    let mut tx = db.begin().await.unwrap();
    let conversation: Uuid = sqlx::query_scalar(
        "UPDATE conversations SET last_item_seq = last_item_seq + 1
         WHERE kind = 'owner' AND bot_id = (SELECT bot_id FROM turns WHERE id = ?1)
         RETURNING id",
    )
    .bind(turn)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO items (id, conversation_id, seq, turn_id, kind, text)
         SELECT ?1, id, last_item_seq, ?3, ?4, ?5 FROM conversations WHERE id = ?2",
    )
    .bind(id)
    .bind(conversation)
    .bind(turn)
    .bind(kind)
    .bind(text)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

/// Move a turn to `status` by hand, skipping the scheduler and the
/// lifecycle's source-state checks. The status triggers fire as they would
/// on a real transition.
pub async fn force_status(db: impl SqliteExecutor<'_>, turn: Uuid, status: TurnStatus) {
    let moved = sqlx::query("UPDATE turns SET status = ?2 WHERE id = ?1")
        .bind(turn)
        .bind(status)
        .execute(db)
        .await
        .unwrap();
    assert_eq!(moved.rows_affected(), 1, "turn {turn} exists");
}

/// A delegation row to seed, `queued` from attempt 1 of its root.
#[must_use]
pub struct Delegation {
    root: Uuid,
    parent_bot: Uuid,
    target_bot: Uuid,
    task: &'static str,
    conversation: Uuid,
    child: Uuid,
}

/// Start a delegation from `parent`'s `root` turn to `target`, whose task
/// runs as `child` in the target's task `conversation`.
pub fn delegation(
    root: Uuid,
    parent: Uuid,
    target: Uuid,
    conversation: Uuid,
    child: Uuid,
) -> Delegation {
    Delegation {
        root,
        parent_bot: parent,
        target_bot: target,
        task: "seeded task",
        conversation,
        child,
    }
}

impl Delegation {
    pub fn task(mut self, task: &'static str) -> Self {
        self.task = task;
        self
    }

    pub async fn insert(self, db: impl SqliteExecutor<'_>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO delegations (id, root_turn_id, parent_attempt, parent_bot_id,
                 target_bot_id, task, task_hash, conversation_id, child_turn_id)
             VALUES (?1, ?2, 1, ?3, ?4, ?5, zeroblob(32), ?6, ?7)",
        )
        .bind(id)
        .bind(self.root)
        .bind(self.parent_bot)
        .bind(self.target_bot)
        .bind(self.task)
        .bind(self.conversation)
        .bind(self.child)
        .execute(db)
        .await
        .unwrap();
        id
    }
}
