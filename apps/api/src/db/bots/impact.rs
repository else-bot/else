use sqlx::SqliteConnection;
use uuid::Uuid;

/// What deleting a bot stops or cuts off, read when the owner is asked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Impact {
    pub category: Option<String>,
    /// Names of its enabled routines.
    pub routines: Vec<String>,
    /// Names of its enabled webhooks.
    pub webhooks: Vec<String>,
    /// Inbox addresses that wake it.
    pub mail_triggers: Vec<String>,
    /// Social chats that wake it, by connection name and channel.
    pub social_triggers: Vec<String>,
    pub tasks: Vec<Task>,
    /// Names of active bots that read its memory.
    pub memory_readers: Vec<String>,
}

/// A delegated task in progress that the deletion cancels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The other bot's name.
    pub with: String,
    /// The other bot delegated it to the deleted bot.
    pub incoming: bool,
    pub task: String,
}

pub async fn impact(conn: &mut SqliteConnection, bot_id: Uuid) -> Result<Impact, sqlx::Error> {
    let category = sqlx::query_scalar(
        "SELECT c.name FROM bots b JOIN bot_categories c ON c.id = b.category_id WHERE b.id = ?1",
    )
    .bind(bot_id)
    .fetch_optional(&mut *conn)
    .await?;
    let routines = sqlx::query_scalar(
        "SELECT name FROM routines WHERE bot_id = ?1 AND enabled AND deleted_at IS NULL
         ORDER BY name",
    )
    .bind(bot_id)
    .fetch_all(&mut *conn)
    .await?;
    let webhooks = sqlx::query_scalar(
        "SELECT name FROM webhooks WHERE bot_id = ?1 AND enabled AND deleted_at IS NULL
         ORDER BY name",
    )
    .bind(bot_id)
    .fetch_all(&mut *conn)
    .await?;
    let mail_triggers = sqlx::query_scalar(
        "SELECT a.address FROM connector_grants g JOIN connector_accounts a ON a.id = g.account_id
         WHERE g.bot_id = ?1 AND g.can_read AND g.wake_instructions IS NOT NULL
           AND a.state <> 'removed'
         ORDER BY a.address",
    )
    .bind(bot_id)
    .fetch_all(&mut *conn)
    .await?;
    let social_triggers = sqlx::query_scalar(
        "SELECT c.display_name || coalesce(' · ' || t.destination, '')
         FROM social_triggers t JOIN social_connections c ON c.id = t.connection_id
         WHERE t.bot_id = ?1 AND t.enabled AND c.state <> 'removed'
         ORDER BY c.display_name, t.destination",
    )
    .bind(bot_id)
    .fetch_all(&mut *conn)
    .await?;
    let tasks: Vec<(String, bool, String)> = sqlx::query_as(
        "SELECT o.name, d.target_bot_id = ?1, d.task
         FROM delegations d
         JOIN bots o ON o.id = iif(d.target_bot_id = ?1, d.parent_bot_id, d.target_bot_id)
         WHERE (d.target_bot_id = ?1 OR d.parent_bot_id = ?1)
           AND d.status IN ('queued', 'running')
         ORDER BY d.created_at, d.id",
    )
    .bind(bot_id)
    .fetch_all(&mut *conn)
    .await?;
    let memory_readers = sqlx::query_scalar(
        "SELECT b.name FROM memory_read_grants g JOIN bots b ON b.id = g.reader_bot_id
         WHERE g.target_bot_id = ?1 AND b.status = 'active'
         ORDER BY b.name",
    )
    .bind(bot_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(Impact {
        category,
        routines,
        webhooks,
        mail_triggers,
        social_triggers,
        tasks: tasks
            .into_iter()
            .map(|(with, incoming, task)| Task {
                with,
                incoming,
                task,
            })
            .collect(),
        memory_readers,
    })
}
