//! The tables a turn can wait on. A row parks the attempt that made it
//! ([`super::turns::parked`]); once it settles the turn continues, and the
//! next attempt's request carries its outcome and records that attempt in
//! `delivered_attempt`. The `turn_holds` view unions these tables and says
//! which statuses are pending and unseen.

use std::collections::{BTreeMap, HashMap};

use sqlx::SqliteConnection;
use sqlx::types::Json;
use uuid::Uuid;

use super::in_ids;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Hold {
    Delegation,
    Secret,
    Question,
    Draft,
    Social,
    McpWait,
    JobWait,
    Skill,
    Handoff,
}

impl Hold {
    #[cfg(test)]
    const ALL: [Self; 9] = [
        Self::Delegation,
        Self::Secret,
        Self::Question,
        Self::Draft,
        Self::Social,
        Self::McpWait,
        Self::JobWait,
        Self::Skill,
        Self::Handoff,
    ];

    fn table(self) -> &'static str {
        match self {
            Self::Delegation => "delegations",
            Self::Secret => "secret_requests",
            Self::Question => "owner_questions",
            Self::Draft => "outgoing_drafts",
            Self::Social => "social_setups",
            Self::McpWait => "mcp_waits",
            Self::JobWait => "computer_job_waits",
            Self::Skill => "skill_installs",
            Self::Handoff => "computer_handoffs",
        }
    }
}

/// What a wait tool's create did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Created {
    /// This attempt waits and is parked.
    Waiting(Uuid),
    /// A pending wait from an earlier attempt of the turn already waits on
    /// the same thing; the turn keeps it after this attempt ends. This
    /// attempt holds nothing, so it is not parked.
    Carried(Uuid),
}

impl Created {
    pub fn id(self) -> Uuid {
        match self {
            Self::Waiting(id) | Self::Carried(id) => id,
        }
    }

    pub fn parked(self) -> bool {
        matches!(self, Self::Waiting(_))
    }

    /// The `status` the tool result reports.
    pub fn status(self) -> &'static str {
        match self {
            Self::Waiting(_) => "waiting",
            Self::Carried(_) => "already_waiting",
        }
    }
}

/// The settled holds an attempt's request carries for the first time, by
/// kind. Holds no empty list.
#[derive(Default)]
pub struct Delivered(BTreeMap<Hold, Vec<Uuid>>);

impl FromIterator<(Hold, Vec<Uuid>)> for Delivered {
    fn from_iter<I: IntoIterator<Item = (Hold, Vec<Uuid>)>>(iter: I) -> Self {
        Self(
            iter.into_iter()
                .filter(|(_, ids)| !ids.is_empty())
                .collect(),
        )
    }
}

impl Delivered {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Record that `attempt` of `turn_id` received these holds. Publishes
    /// the turn when it received a task's result.
    pub async fn mark(
        &self,
        conn: &mut SqliteConnection,
        bot_id: Uuid,
        turn_id: Uuid,
        attempt: i32,
    ) -> Result<(), sqlx::Error> {
        for (&hold, ids) in &self.0 {
            match hold {
                Hold::Delegation => {
                    super::delegations::mark_delivered(conn, turn_id, attempt, ids).await?;
                    super::events::turn_changed(conn, bot_id, turn_id).await?;
                }
                Hold::Question => {
                    super::owner_questions::mark_delivered(conn, turn_id, attempt, ids).await?;
                }
                _ => {
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "UPDATE {} SET delivered_attempt = ?3
                         WHERE turn_id = ?1 AND delivered_attempt IS NULL AND {}",
                        hold.table(),
                        in_ids("id", "?2")
                    )))
                    .bind(turn_id)
                    .bind(Json(ids))
                    .bind(attempt)
                    .execute(&mut *conn)
                    .await?;
                }
            }
        }
        Ok(())
    }
}

/// Group `rows` by the turn `turn_of` names, keeping their order.
pub fn by_turn<T>(
    rows: impl IntoIterator<Item = T>,
    turn_of: impl Fn(&T) -> Uuid,
) -> HashMap<Uuid, Vec<T>> {
    let mut grouped: HashMap<Uuid, Vec<T>> = HashMap::new();
    for row in rows {
        grouped.entry(turn_of(&row)).or_default().push(row);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::Hold;

    #[tokio::test]
    async fn every_table_in_turn_holds_is_a_hold() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let view: String =
            sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE name = 'turn_holds'")
                .fetch_one(&pool)
                .await
                .unwrap();
        let words: Vec<&str> = view.split_whitespace().collect();
        let in_view: BTreeSet<&str> = words
            .windows(2)
            .filter(|pair| pair[0].eq_ignore_ascii_case("from"))
            .map(|pair| pair[1])
            .collect();
        let held: BTreeSet<&str> = Hold::ALL.iter().map(|hold| hold.table()).collect();
        assert_eq!(in_view, held, "{view}");
    }
}
