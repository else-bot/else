//! The idempotent action ledger. A mutating action is recorded as
//! `dispatching`, keyed by account and action ID with a fingerprint of who
//! asked for what, before it is sent. A repeat of the same request returns
//! the recorded receipt; a different request under the same action ID is a
//! conflict. An action left dispatching by a crash or an interruption
//! becomes `uncertain` and is never replayed.

use std::marker::PhantomData;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::types::Json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

/// Largest receipt recorded; a larger one is replaced by a short notice.
pub const MAX_RESULT: usize = 256 * 1024;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%f+00:00', 'now')";

/// An action's state, as stored in `status`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, strum::IntoStaticStr,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "text", rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum Status {
    Dispatching,
    Succeeded,
    Failed,
    /// It may or may not have taken effect; never replayed.
    Uncertain,
}

/// How a dispatched action settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    Failed,
    Uncertain,
}

impl From<Outcome> for Status {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Succeeded => Self::Succeeded,
            Outcome::Failed => Self::Failed,
            Outcome::Uncertain => Self::Uncertain,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Start {
    /// Recorded as dispatching: send it, then [`Ledger::finish`].
    Dispatch,
    /// This action was already requested: its recorded receipt. Never send
    /// it again.
    Existing(Value),
}

#[derive(Debug)]
pub enum BeginError {
    /// The action ID was already used for a different request.
    Conflict,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for BeginError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(error)
    }
}

/// A bound value for one of a ledger's own columns.
pub enum Column<'a> {
    Int(i64),
    Text(Option<&'a str>),
}

/// The columns a ledger's table records beyond the shared ones.
pub trait Columns {
    const NAMES: &'static [&'static str];
    fn values(&self) -> Vec<Column<'_>>;
}

/// One action table. `via` names the drafts whose delivery runs through it,
/// so settling an action refreshes the turns showing them.
pub struct Ledger<C> {
    table: &'static str,
    key: &'static str,
    via: &'static str,
    /// Hashed ahead of the account ID. `None` hashes only the bot, turn and
    /// request, as social actions always have; changing either form would
    /// stop recorded action IDs from deduplicating.
    domain: Option<&'static [u8]>,
    columns: PhantomData<fn(C)>,
}

/// The receipt every ledger returns: the action ID and status, plus the
/// fields its settlement recorded.
pub fn receipt(action_id: Uuid, status: Status, result: Option<Json<Value>>) -> Value {
    match result {
        Some(result) => result.0,
        None => json!({ "action_id": action_id, "status": status }),
    }
}

/// The receipt a settlement records: [`receipt`]'s shape with `fields`, an
/// object, merged in, or a short notice when it is too large.
fn settlement(action_id: Uuid, status: Status, fields: Value) -> Value {
    let mut settled = json!({ "action_id": action_id, "status": status });
    if let (Some(settled), Value::Object(fields)) = (settled.as_object_mut(), fields) {
        for (key, value) in fields {
            settled.entry(key).or_insert(value);
        }
    }
    if settled.to_string().len() > MAX_RESULT {
        settled = json!({
            "action_id": action_id,
            "status": status,
            "omitted": "the result was too large to record",
        });
    }
    settled
}

impl<C: Columns> Ledger<C> {
    pub const fn new(
        table: &'static str,
        key: &'static str,
        via: &'static str,
        domain: Option<&'static [u8]>,
    ) -> Self {
        Self {
            table,
            key,
            via,
            domain,
            columns: PhantomData,
        }
    }

    fn fingerprint(
        &self,
        account: Uuid,
        bot: Uuid,
        turn: Uuid,
        request: &impl Serialize,
    ) -> Vec<u8> {
        let mut hasher = Sha256::new();
        if let Some(domain) = self.domain {
            hasher.update(domain);
            hasher.update(account.as_bytes());
        }
        hasher.update(bot.as_bytes());
        hasher.update(turn.as_bytes());
        hasher.update(serde_json::to_vec(request).expect("an action request serializes"));
        hasher.finalize().to_vec()
    }

    /// Record an action as dispatching, in the caller's immediate
    /// transaction after it authorized `request`.
    #[expect(
        clippy::too_many_arguments,
        reason = "one per ledger key column; a struct would only rename them"
    )]
    pub async fn begin(
        &self,
        conn: &mut SqliteConnection,
        account: Uuid,
        action: Uuid,
        bot: Uuid,
        turn: Uuid,
        request: &impl Serialize,
        columns: &C,
    ) -> Result<Start, BeginError> {
        let fingerprint = self.fingerprint(account, bot, turn, request);
        let existing: Option<(Vec<u8>, Status, Option<Json<Value>>)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT fingerprint, status, result FROM {} WHERE {} = ?1 AND action_id = ?2",
                self.table, self.key
            )))
            .bind(account)
            .bind(action)
            .fetch_optional(&mut *conn)
            .await?;
        if let Some((recorded, status, result)) = existing {
            if recorded != fingerprint {
                return Err(BeginError::Conflict);
            }
            return Ok(Start::Existing(receipt(action, status, result)));
        }
        let names = C::NAMES.join(", ");
        let slots: String = (0..C::NAMES.len())
            .map(|i| format!(", ?{}", i + 6))
            .collect();
        let sql = format!(
            "INSERT INTO {} ({}, action_id, bot_id, turn_id, fingerprint, status{}{names})
             VALUES (?1, ?2, ?3, ?4, ?5, 'dispatching'{slots})",
            self.table,
            self.key,
            if names.is_empty() { "" } else { ", " },
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(account)
            .bind(action)
            .bind(bot)
            .bind(turn)
            .bind(&fingerprint);
        for value in columns.values() {
            query = match value {
                Column::Int(v) => query.bind(v),
                Column::Text(v) => query.bind(v),
            };
        }
        query.execute(&mut *conn).await?;
        Ok(Start::Dispatch)
    }

    /// Settle a dispatching action and return the receipt the ledger holds,
    /// which keeps an earlier settlement such as an interruption marking it
    /// uncertain. `fields` is an object merged into the receipt.
    pub async fn finish(
        &self,
        pool: &SqlitePool,
        account: Uuid,
        action: Uuid,
        outcome: Outcome,
        fields: Value,
    ) -> Result<Value, sqlx::Error> {
        let status = Status::from(outcome);
        let settled = settlement(action, status, fields);
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {} SET status = ?3, result = ?4, finished_at = {NOW}
             WHERE {} = ?1 AND action_id = ?2 AND status = 'dispatching'",
            self.table, self.key
        )))
        .bind(account)
        .bind(action)
        .bind(status)
        .bind(Json(&settled))
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed > 0 {
            self.announce(&mut tx, account, Some(action)).await?;
        }
        let (status, result): (Status, Option<Json<Value>>) =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT status, result FROM {} WHERE {} = ?1 AND action_id = ?2",
                self.table, self.key
            )))
            .bind(account)
            .bind(action)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(receipt(action, status, result))
    }

    /// Mark an interrupted action, or with `None` every action of the
    /// account still dispatching, uncertain.
    pub async fn mark_uncertain(
        &self,
        pool: &SqlitePool,
        account: Uuid,
        action: Option<Uuid>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {} SET status = 'uncertain', finished_at = {NOW}
             WHERE {} = ?1 AND status = 'dispatching' AND (?2 IS NULL OR action_id = ?2)",
            self.table, self.key
        )))
        .bind(account)
        .bind(action)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed > 0 {
            self.announce(&mut tx, account, action).await?;
        }
        tx.commit().await
    }

    /// Before any action runs, or on a restored database: every action left
    /// dispatching becomes uncertain. Returns how many.
    pub async fn recover(&self, conn: &mut SqliteConnection) -> Result<u64, sqlx::Error> {
        Ok(sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {} SET status = 'uncertain', finished_at = {NOW} WHERE status = 'dispatching'",
            self.table
        )))
        .execute(conn)
        .await?
        .rows_affected())
    }

    /// An uncertain action found to have taken effect becomes succeeded,
    /// keeping its earlier receipt under `earlier`. Returns the action's
    /// status afterward.
    pub async fn confirm(
        &self,
        conn: &mut SqliteConnection,
        account: Uuid,
        action: Uuid,
        fields: Value,
    ) -> Result<Option<Status>, sqlx::Error> {
        let confirmed = settlement(action, Status::Succeeded, fields);
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {} SET status = 'succeeded',
                 result = json_set(?3, '$.earlier', json(coalesce(result, 'null')))
             WHERE {} = ?1 AND action_id = ?2 AND status = 'uncertain'",
            self.table, self.key
        )))
        .bind(account)
        .bind(action)
        .bind(Json(&confirmed))
        .execute(&mut *conn)
        .await?;
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT status FROM {} WHERE {} = ?1 AND action_id = ?2",
            self.table, self.key
        )))
        .bind(account)
        .bind(action)
        .fetch_optional(conn)
        .await
    }

    /// The receipt of an action this bot requested on this account.
    pub async fn receipt(
        &self,
        pool: &SqlitePool,
        account: Uuid,
        bot: Uuid,
        action: Uuid,
    ) -> Result<Option<Value>, sqlx::Error> {
        let row: Option<(Status, Option<Json<Value>>)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT status, result FROM {} WHERE {} = ?1 AND action_id = ?2 AND bot_id = ?3",
                self.table, self.key
            )))
            .bind(account)
            .bind(action)
            .bind(bot)
            .fetch_optional(pool)
            .await?;
        Ok(row.map(|(status, result)| receipt(action, status, result)))
    }

    async fn announce(
        &self,
        conn: &mut SqliteConnection,
        account: Uuid,
        action: Option<Uuid>,
    ) -> Result<(), sqlx::Error> {
        let turns: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT DISTINCT bot_id, turn_id FROM outgoing_drafts
             WHERE json_extract(delivery, '$.via') = ?3
               AND replace(json_extract(delivery, '$.account_id'), '-', '') = lower(hex(?1))
               AND (?2 IS NULL OR replace(json_extract(delivery, '$.action_id'), '-', '') = lower(hex(?2)))",
        )
        .bind(account)
        .bind(action)
        .bind(self.via)
        .fetch_all(&mut *conn)
        .await?;
        for (bot, turn) in turns {
            crate::db::events::turn_changed(conn, bot, turn).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_keep_the_layout_recorded_actions_were_hashed_with() {
        let (account, bot, turn) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        let request = json!({"a": 1});
        let hex = |print: Vec<u8>| print.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(
            hex(crate::connectors::store::ACTIONS.fingerprint(account, bot, turn, &request)),
            "d3af2b3b6f1d47d48a25af83ba9f7a2a7aefa4e0f525f3258d67d95a2187d84e"
        );
        assert_eq!(
            hex(crate::socials::store::ACTIONS.fingerprint(account, bot, turn, &request)),
            "e1e88fa08e6aaf7163b35eab98669bc70c7e854e8637ffab24b5ca0571b10e1e"
        );
    }
}
