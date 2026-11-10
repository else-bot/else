use super::types::*;
use crate::credentials::SecretKey;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool, types::Json};
use std::{
    collections::HashMap,
    io::{Read, Write},
};
use uuid::Uuid;

const COLUMNS: &str = "id,provider,remote_id,display_name,username,state,health,generation,instructions,EXISTS(SELECT 1 FROM social_triggers st WHERE st.connection_id=social_connections.id AND st.enabled AND st.wake='all') AS message_content";
fn identity(
    key: &SecretKey,
    id: Uuid,
    provider: Provider,
    version: i64,
    salt: &[u8; 32],
) -> Result<age::x25519::Identity, Error> {
    crate::backup::identity_for_purpose(
        key,
        salt,
        &format!("else.bot/socials/v1\0{id}\0{}\0{version}", provider.name()),
    )
    .map_err(|_| Error::Crypto)
}
fn seal(
    key: &SecretKey,
    id: Uuid,
    provider: Provider,
    version: i64,
    token: &Token,
) -> Result<(Vec<u8>, Vec<u8>), Error> {
    let mut salt = [0; 32];
    getrandom::fill(&mut salt).map_err(|_| Error::Crypto)?;
    let recipient = identity(key, id, provider, version, &salt)?.to_public();
    let encryptor =
        age::Encryptor::with_recipients(std::iter::once(&recipient as &dyn age::Recipient))
            .map_err(|_| Error::Crypto)?;
    let mut cipher = Vec::new();
    let mut writer = encryptor
        .wrap_output(&mut cipher)
        .map_err(|_| Error::Crypto)?;
    writer
        .write_all(token.0.as_bytes())
        .map_err(|_| Error::Crypto)?;
    writer.finish().map_err(|_| Error::Crypto)?;
    Ok((salt.to_vec(), cipher))
}
pub async fn credential(
    pool: &SqlitePool,
    key: &SecretKey,
    c: &Connection,
) -> Result<Token, Error> {
    let (version,salt,cipher):(i64,Vec<u8>,Vec<u8>)=sqlx::query_as("SELECT credential_version,credential_salt,credential FROM social_connections WHERE id=?1 AND state<>'removed'").bind(c.id).fetch_one(pool).await?;
    let salt: [u8; 32] = salt.try_into().map_err(|_| Error::Crypto)?;
    let identity = identity(key, c.id, c.provider, version, &salt)?;
    let decryptor = age::Decryptor::new(&cipher[..]).map_err(|_| Error::Crypto)?;
    let mut reader = decryptor
        .decrypt(std::iter::once(&identity as &dyn age::Identity))
        .map_err(|_| Error::Crypto)?;
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    reader.read_to_end(&mut bytes).map_err(|_| Error::Crypto)?;
    Token::parse(String::from_utf8(bytes.to_vec()).map_err(|_| Error::Crypto)?)
}
pub async fn connections(pool: &SqlitePool) -> Result<Vec<Connection>, Error> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {COLUMNS} FROM social_connections WHERE state<>'removed' ORDER BY provider,display_name"))).fetch_all(pool).await?)
}
pub async fn connection(pool: &SqlitePool, id: Uuid) -> Result<Connection, Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM social_connections WHERE id=?1 AND state<>'removed'"
    )))
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or(Error::NotFound)
}
pub async fn create(
    pool: &SqlitePool,
    key: &SecretKey,
    provider: Provider,
    remote: &Identity,
    token: &Token,
    instructions: &str,
) -> Result<Connection, Error> {
    if instructions.len() > 8000 || instructions.contains('\0') {
        return Err(Error::Invalid("Instructions must be at most 8000 bytes."));
    }
    let id = Uuid::new_v4();
    let (salt, cipher) = seal(key, id, provider, 1, token)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM social_connections WHERE state<>'removed'")
            .fetch_one(&mut *tx)
            .await?;
    if total >= 32 {
        return Err(Error::Busy);
    }
    let existing:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social_connections WHERE provider=?1 AND remote_id=?2 AND state<>'removed')").bind(provider).bind(&remote.id).fetch_one(&mut *tx).await?;
    if existing {
        return Err(Error::Duplicate);
    }
    sqlx::query("INSERT INTO social_connections(id,provider,remote_id,display_name,username,instructions,credential_salt,credential) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)").bind(id).bind(provider).bind(&remote.id).bind(&remote.display_name).bind(&remote.username).bind(instructions).bind(salt).bind(cipher).execute(&mut *tx).await?;
    tx.commit().await?;
    connection(pool, id).await
}
pub async fn replace(
    pool: &SqlitePool,
    key: &SecretKey,
    c: &Connection,
    token: &Token,
) -> Result<(), Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let version:i64=sqlx::query_scalar("SELECT credential_version+1 FROM social_connections WHERE id=?1 AND generation=?2 AND state<>'removed'").bind(c.id).bind(c.generation).fetch_optional(&mut *tx).await?.ok_or(Error::Inactive)?;
    let (salt, cipher) = seal(key, c.id, c.provider, version, token)?;
    sqlx::query("UPDATE social_connections SET credential_version=?2,credential_salt=?3,credential=?4,generation=generation+1,health='connecting' WHERE id=?1").bind(c.id).bind(version).bind(salt).bind(cipher).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub enum StateChange {
    Changed(Connection),
    Unchanged(Connection),
}
/// Move a connection to `to`. Only a real change advances the generation,
/// revokes pending deliveries, and needs the worker to reload.
pub async fn set_state(
    pool: &SqlitePool,
    id: Uuid,
    to: ConnectionState,
) -> Result<StateChange, Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let from: ConnectionState =
        sqlx::query_scalar("SELECT state FROM social_connections WHERE id=?1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::NotFound)?;
    let Some(to) = from.transition(to)? else {
        return Ok(StateChange::Unchanged(stored(&mut tx, id).await?));
    };
    let removed = to == ConnectionState::Removed;
    let updated=sqlx::query("UPDATE social_connections SET state=?3,generation=generation+1,health=?4,credential=CASE WHEN ?5 THEN NULL ELSE credential END,credential_salt=CASE WHEN ?5 THEN NULL ELSE credential_salt END WHERE id=?1 AND state=?2").bind(id).bind(from).bind(to).bind(to.health()).bind(removed).execute(&mut *tx).await?;
    if updated.rows_affected() == 0 {
        return Err(Error::Conflict);
    }
    sqlx::query(
        "UPDATE social_deliveries SET status='revoked' WHERE connection_id=?1 AND status='pending'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if removed {
        let bots: Vec<Uuid> = sqlx::query_scalar(
            "DELETE FROM social_triggers WHERE connection_id=?1 RETURNING bot_id",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?;
        for bot in bots {
            crate::db::events::bot_updated(&mut tx, bot).await?;
        }
    }
    let connection = stored(&mut tx, id).await?;
    tx.commit().await?;
    Ok(StateChange::Changed(connection))
}
async fn stored(conn: &mut SqliteConnection, id: Uuid) -> Result<Connection, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLUMNS} FROM social_connections WHERE id=?1"
    )))
    .bind(id)
    .fetch_one(conn)
    .await
}
const TRIGGER_COLUMNS: &str = "t.id,t.connection_id,t.bot_id,c.provider,c.display_name AS connection_name,c.username,t.destination,json_extract(d.info,'$.name') AS destination_name,t.wake,t.instructions,t.enabled,t.created_at,t.updated_at,(SELECT max(x.settled_at) FROM social_deliveries x WHERE x.trigger_id=t.id AND x.turn_id IS NOT NULL) AS last_woke_at FROM social_triggers t JOIN social_connections c ON c.id=t.connection_id AND c.state<>'removed' LEFT JOIN social_destinations d ON d.connection_id=t.connection_id AND d.id=t.destination";

/// The bot's triggers, oldest first.
pub async fn triggers(conn: &mut SqliteConnection, bot: Uuid) -> Result<Vec<Trigger>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {TRIGGER_COLUMNS} WHERE t.bot_id=?1 ORDER BY t.created_at,t.id"
    )))
    .bind(bot)
    .fetch_all(conn)
    .await
}

async fn trigger(conn: &mut SqliteConnection, bot: Uuid, id: Uuid) -> Result<Trigger, Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {TRIGGER_COLUMNS} WHERE t.bot_id=?1 AND t.id=?2"
    )))
    .bind(bot)
    .bind(id)
    .fetch_optional(conn)
    .await?
    .ok_or(Error::NotFound)
}

/// Create or replace the calling bot's trigger for one chat (or any chat)
/// of a connection. Only work rooted in an owner message sets triggers.
pub async fn set_trigger(
    pool: &SqlitePool,
    turn: Uuid,
    input: &TriggerInput,
) -> Result<TriggerChange<Trigger>, Error> {
    input.validate()?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = crate::db::turns::asking_caller(&mut tx, turn)
        .await?
        .ok_or(Error::Forbidden)?;
    if crate::db::delegations::origin(&mut tx, turn).await?
        != crate::app::providers::Origin::OwnerRequest
    {
        return Err(Error::Forbidden);
    }
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM social_connections WHERE id=?1 AND state<>'removed')",
    )
    .bind(input.connection_id)
    .fetch_one(&mut *tx)
    .await?;
    if !active {
        return Err(Error::NotFound);
    }
    let before = wants_content(&mut tx, input.connection_id).await?;
    let destination = input.destination.as_deref();
    let existing: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM social_triggers WHERE connection_id=?1 AND bot_id=?2 AND coalesce(destination,'')=coalesce(?3,'')",
    )
    .bind(input.connection_id)
    .bind(caller.bot_id)
    .bind(destination)
    .fetch_optional(&mut *tx)
    .await?;
    let id = match existing {
        Some(id) => {
            sqlx::query(
                "UPDATE social_triggers SET wake=?2,instructions=?3,enabled=1,updated_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE id=?1",
            )
            .bind(id)
            .bind(input.wake)
            .bind(input.instructions.trim())
            .execute(&mut *tx)
            .await?;
            id
        }
        None => {
            let total: i64 =
                sqlx::query_scalar("SELECT count(*) FROM social_triggers WHERE bot_id=?1")
                    .bind(caller.bot_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if total >= MAX_TRIGGERS {
                return Err(Error::Invalid(
                    "This bot already has 32 triggers. Delete one first.",
                ));
            }
            let id = Uuid::new_v4();
            sqlx::query(
                "INSERT INTO social_triggers(id,connection_id,bot_id,destination,wake,instructions) VALUES(?1,?2,?3,?4,?5,?6)",
            )
            .bind(id)
            .bind(input.connection_id)
            .bind(caller.bot_id)
            .bind(destination)
            .bind(input.wake)
            .bind(input.instructions.trim())
            .execute(&mut *tx)
            .await?;
            id
        }
    };
    let saved = trigger(&mut tx, caller.bot_id, id).await?;
    let reconnect = before != wants_content(&mut tx, input.connection_id).await?;
    crate::db::events::bot_updated(&mut tx, caller.bot_id).await?;
    tx.commit().await?;
    Ok(TriggerChange {
        connection: saved.connection_id,
        value: saved,
        reconnect,
    })
}

const MAX_TRIGGERS: i64 = 32;

/// Whether a Discord connection's listener must ask for message content: an
/// enabled trigger wakes on every message. Telegram has no such switch.
async fn wants_content(conn: &mut SqliteConnection, connection: Uuid) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM social_triggers t JOIN social_connections c ON c.id=t.connection_id
         WHERE t.connection_id=?1 AND t.enabled AND t.wake='all' AND c.provider='discord')",
    )
    .bind(connection)
    .fetch_one(conn)
    .await
}

/// A trigger change. `reconnect` is set when the connection's listener must
/// restart to change what it receives; other changes leave in-flight actions
/// alone.
pub struct TriggerChange<T> {
    pub value: T,
    pub connection: Uuid,
    pub reconnect: bool,
}

/// Pause or resume one of the bot's triggers. A paused trigger's queued
/// messages are dropped.
pub async fn set_trigger_enabled(
    pool: &SqlitePool,
    bot: Uuid,
    id: Uuid,
    enabled: bool,
) -> Result<TriggerChange<Trigger>, Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let connection = trigger(&mut tx, bot, id).await?.connection_id;
    let before = wants_content(&mut tx, connection).await?;
    let changed = sqlx::query(
        "UPDATE social_triggers SET enabled=?3,updated_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE bot_id=?1 AND id=?2",
    )
    .bind(bot)
    .bind(id)
    .bind(enabled)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(Error::NotFound);
    }
    if !enabled {
        revoke_deliveries(&mut tx, id).await?;
    }
    let saved = trigger(&mut tx, bot, id).await?;
    let reconnect = before != wants_content(&mut tx, connection).await?;
    crate::db::events::bot_updated(&mut tx, bot).await?;
    tx.commit().await?;
    Ok(TriggerChange {
        value: saved,
        connection,
        reconnect,
    })
}

/// Delete one of the bot's triggers and drop its queued messages.
pub async fn delete_trigger(
    pool: &SqlitePool,
    bot: Uuid,
    id: Uuid,
) -> Result<TriggerChange<()>, Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let connection = trigger(&mut tx, bot, id).await?.connection_id;
    let before = wants_content(&mut tx, connection).await?;
    sqlx::query("DELETE FROM social_triggers WHERE bot_id=?1 AND id=?2")
        .bind(bot)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    revoke_deliveries(&mut tx, id).await?;
    let reconnect = before != wants_content(&mut tx, connection).await?;
    crate::db::events::bot_updated(&mut tx, bot).await?;
    tx.commit().await?;
    Ok(TriggerChange {
        value: (),
        connection,
        reconnect,
    })
}

async fn revoke_deliveries(conn: &mut SqliteConnection, trigger: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE social_deliveries SET status='revoked',settled_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE trigger_id=?1 AND status='pending'",
    )
    .bind(trigger)
    .execute(conn)
    .await?;
    Ok(())
}
pub async fn destinations(pool: &SqlitePool, id: Uuid) -> Result<Vec<Destination>, Error> {
    let rows: Vec<Json<Destination>> = sqlx::query_scalar(
        "SELECT info FROM social_destinations WHERE connection_id=?1 ORDER BY id LIMIT 200",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}
pub async fn save_destinations(
    pool: &SqlitePool,
    id: Uuid,
    list: &[Destination],
) -> Result<(), Error> {
    let mut tx = pool.begin().await?;
    for d in list {
        sqlx::query("INSERT INTO social_destinations(connection_id,id,info) VALUES(?1,?2,?3) ON CONFLICT(connection_id,id) DO UPDATE SET info=excluded.info").bind(id).bind(&d.id).bind(Json(d)).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// What `turn` of `bot` may do on connection `c` right now. Work for the
/// owner may use any chat; a turn an incoming message started may only read
/// the chats delivered to it.
pub async fn authorize(
    conn: &mut SqliteConnection,
    c: &Connection,
    bot: Uuid,
    turn: Uuid,
    destination: Option<&str>,
    write: bool,
    send: bool,
) -> Result<Access, Error> {
    let kind: Option<String> = sqlx::query_scalar(
        "SELECT i.kind FROM social_connections c JOIN turns t ON t.id=?4 JOIN bots b ON b.id=t.bot_id
         JOIN items i ON i.id=t.owner_item_id
         WHERE c.id=?1 AND c.state='active' AND c.generation=?2 AND t.bot_id=?3
           AND t.status IN ('sending','running') AND b.status='active' AND NOT b.cancel_pending",
    )
    .bind(c.id)
    .bind(c.generation)
    .bind(bot)
    .bind(turn)
    .fetch_optional(&mut *conn)
    .await?;
    let access = match kind.ok_or(Error::Forbidden)?.as_str() {
        "connector" => {
            let chats: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT destination FROM social_deliveries
                 WHERE turn_id=?1 AND connection_id=?2 AND bot_id=?3",
            )
            .bind(turn)
            .bind(c.id)
            .bind(bot)
            .fetch_all(&mut *conn)
            .await?;
            if chats.is_empty() {
                return Err(Error::Forbidden);
            }
            Access::Woken(chats)
        }
        _ => Access::Owner,
    };
    if (write || send) && !access.mutates() || destination.is_some_and(|d| !access.allows(d)) {
        return Err(Error::Forbidden);
    }
    Ok(access)
}
pub struct Action<'a> {
    pub bot: Uuid,
    pub turn: Uuid,
    pub id: Uuid,
    pub destination: &'a str,
    pub payload: &'a Value,
    pub send: bool,
}

pub async fn begin_action(
    pool: &SqlitePool,
    c: &Connection,
    Action {
        bot,
        turn,
        id,
        destination,
        payload,
        send,
    }: Action<'_>,
) -> Result<Option<Value>, Error> {
    let mut h = Sha256::new();
    h.update(bot.as_bytes());
    h.update(turn.as_bytes());
    h.update(serde_json::to_vec(payload).map_err(|_| Error::Conflict)?);
    let hash = h.finalize().to_vec();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    authorize(&mut tx, c, bot, turn, Some(destination), true, send).await?;
    let old:Option<(Vec<u8>,ActionStatus,Option<Json<Value>>)>=sqlx::query_as("SELECT fingerprint,status,result FROM social_actions WHERE connection_id=?1 AND action_id=?2").bind(c.id).bind(id).fetch_optional(&mut *tx).await?;
    if let Some((old, status, result)) = old {
        if old != hash {
            return Err(Error::Conflict);
        }
        return Ok(Some(
            json!({"action_id":id,"status":status,"result":result.map(|r|r.0)}),
        ));
    }
    sqlx::query("INSERT INTO social_actions(connection_id,action_id,bot_id,turn_id,fingerprint,destination,status) VALUES(?1,?2,?3,?4,?5,?6,'dispatching')").bind(c.id).bind(id).bind(bot).bind(turn).bind(hash).bind(destination).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(None)
}
/// Settle a dispatching action and return its stored receipt, which keeps
/// an earlier settlement, such as recovery marking it uncertain.
pub async fn finish_action(
    pool: &SqlitePool,
    connection: Uuid,
    action: Uuid,
    result: &Result<Value, Error>,
) -> Result<Value, Error> {
    let value = match result {
        Ok(v) => v.clone(),
        Err(e) => json!({"error":e.to_string()}),
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let settled=sqlx::query("UPDATE social_actions SET status=?3,result=?4 WHERE connection_id=?1 AND action_id=?2 AND status=?5").bind(connection).bind(action).bind(ActionStatus::settled(result)).bind(Json(&value)).bind(ActionStatus::Dispatching).execute(&mut *tx).await?;
    if settled.rows_affected() > 0 {
        announce_drafts(&mut tx, connection, Some(action)).await?;
    }
    let (status, result): (ActionStatus, Option<Json<Value>>) = sqlx::query_as(
        "SELECT status,result FROM social_actions WHERE connection_id=?1 AND action_id=?2",
    )
    .bind(connection)
    .bind(action)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({"action_id":action,"status":status,"result":result.map(|r|r.0)}))
}
pub async fn record(
    pool: &SqlitePool,
    c: &Connection,
    event: &Incoming,
    next_offset: i64,
) -> Result<(), Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let current:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social_connections WHERE id=?1 AND generation=?2 AND state='active')").bind(c.id).bind(c.generation).fetch_one(&mut *tx).await?;
    if !current {
        return Err(Error::Inactive);
    }
    sqlx::query("INSERT INTO social_destinations(connection_id,id,info) VALUES(?1,?2,?3) ON CONFLICT(connection_id,id) DO UPDATE SET info=CASE WHEN ?4='telegram' THEN excluded.info ELSE social_destinations.info END").bind(c.id).bind(&event.destination.id).bind(Json(&event.destination)).bind(c.provider).execute(&mut *tx).await?;
    let inserted=sqlx::query("INSERT OR IGNORE INTO social_messages(connection_id,destination,message_id,author_id,body,event_id) VALUES(?1,?2,?3,?4,?5,?6)").bind(c.id).bind(&event.destination.id).bind(&event.message_id).bind(&event.author_id).bind(&event.body).bind(&event.event_id).execute(&mut *tx).await?.rows_affected();
    if inserted > 0 && !event.from_bot {
        sqlx::query(
            "INSERT OR IGNORE INTO social_deliveries(connection_id,event_id,bot_id,trigger_id,destination,message_id)
             SELECT ?1,?2,t.bot_id,t.id,?3,?4 FROM social_triggers t JOIN bots b ON b.id=t.bot_id AND b.status='active'
             WHERE t.connection_id=?1 AND t.enabled AND (t.destination IS NULL OR t.destination=?3)
               AND (?5 OR t.wake='all')
             ORDER BY t.destination IS NULL",
        )
        .bind(c.id)
        .bind(&event.event_id)
        .bind(&event.destination.id)
        .bind(&event.message_id)
        .bind(event.mentioned || event.direct)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM social_messages WHERE connection_id=?1 AND rowid NOT IN (SELECT rowid FROM social_messages WHERE connection_id=?1 ORDER BY created_at DESC LIMIT 5000)").bind(c.id).execute(&mut *tx).await?;
    sqlx::query(
        "UPDATE social_deliveries SET status='expired',settled_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') \
         WHERE connection_id=?1 AND status='pending' AND NOT EXISTS (\
           SELECT 1 FROM social_messages m WHERE m.connection_id=social_deliveries.connection_id \
           AND m.event_id=social_deliveries.event_id)",
    )
    .bind(c.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE social_connections SET poll_offset=max(poll_offset,?2) WHERE id=?1 AND generation=?3").bind(c.id).bind(next_offset).bind(c.generation).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}
pub async fn local_messages(
    pool: &SqlitePool,
    id: Uuid,
    destination: &str,
    before: Option<&str>,
    query: Option<&str>,
    limit: u16,
) -> Result<Value, Error> {
    let rows:Vec<(String,String,String)>=sqlx::query_as("SELECT message_id,author_id,body FROM social_messages WHERE connection_id=?1 AND destination=?2 AND (?3 IS NULL OR length(message_id)<length(?3) OR (length(message_id)=length(?3) AND message_id<?3)) AND (?4 IS NULL OR instr(lower(body),lower(?4))>0) ORDER BY created_at DESC,length(message_id) DESC,message_id DESC LIMIT ?5").bind(id).bind(destination).bind(before).bind(query).bind(limit).fetch_all(pool).await?;
    Ok(
        json!({"messages":rows.into_iter().map(|(id,author,body)|json!({"id":id,"author_id":author,"body":body})).collect::<Vec<_>>(),"history_scope":"observed_since_connection"}),
    )
}
pub async fn recover(pool: &SqlitePool) -> Result<(), Error> {
    sqlx::query(
        "UPDATE social_actions SET status='uncertain',result='{}' WHERE status='dispatching'",
    )
    .execute(pool)
    .await?;
    Ok(())
}
pub async fn restore_passive(conn: &mut SqliteConnection) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE social_connections SET state='paused',health='restored',generation=generation+1 WHERE state<>'removed'").execute(&mut *conn).await?;
    sqlx::query("UPDATE social_deliveries SET status='cancelled' WHERE status='pending'")
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE turns SET status='interrupted',finished_at=strftime('%Y-%m-%dT%H:%M:%f+00:00','now') WHERE status IN ('queued','waiting') AND id IN (SELECT turn_id FROM social_deliveries)").execute(&mut *conn).await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Setup {
    pub id: Uuid,
    pub bot_id: Uuid,
    pub turn_id: Uuid,
    pub provider: Provider,
    pub reason: String,
    pub status: String,
    pub connection_id: Option<Uuid>,
}
const SETUP_COLUMNS: &str = "id,bot_id,turn_id,provider,reason,status,connection_id";
pub async fn setups(
    conn: &mut SqliteConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Setup>>, sqlx::Error> {
    let rows:Vec<Setup>=sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {SETUP_COLUMNS} FROM social_setups WHERE lower(hex(turn_id)) IN (SELECT replace(value,'-','') FROM json_each(?1))"))).bind(Json(ids)).fetch_all(conn).await?;
    let mut map: HashMap<Uuid, Vec<Setup>> = HashMap::new();
    for row in rows {
        map.entry(row.turn_id).or_default().push(row);
    }
    Ok(map)
}
pub async fn request_setup(
    pool: &SqlitePool,
    turn: Uuid,
    provider: Provider,
    reason: &str,
) -> Result<Setup, Error> {
    if reason.trim().is_empty() || reason.len() > 500 {
        return Err(Error::Invalid(
            "Explain why this connection is needed in 1 to 500 bytes.",
        ));
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let caller = crate::db::turns::asking_caller(&mut tx, turn)
        .await?
        .ok_or(Error::Forbidden)?;
    if !crate::db::turns::may_ask_owner(&mut tx, turn, &caller).await? {
        return Err(Error::Forbidden);
    }
    let external:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items i JOIN turns t ON t.owner_item_id=i.id WHERE t.id=?1 AND i.kind='connector')").bind(turn).fetch_one(&mut *tx).await?;
    if external {
        return Err(Error::Forbidden);
    }
    let existing: Option<Setup> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SETUP_COLUMNS} FROM social_setups WHERE turn_id=?1 AND provider=?2"
    )))
    .bind(turn)
    .bind(provider)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(old) = existing {
        return if old.reason == reason {
            Ok(old)
        } else {
            Err(Error::Conflict)
        };
    }
    if crate::db::turns::attempt_parked(&mut tx, turn, caller.attempt).await? {
        return Err(Error::Busy);
    }
    let setup:Setup=sqlx::query_as(sqlx::AssertSqlSafe(format!("INSERT INTO social_setups(id,bot_id,turn_id,attempt,provider,reason) VALUES(?1,?2,?3,?4,?5,?6) RETURNING {SETUP_COLUMNS}"))).bind(Uuid::new_v4()).bind(caller.bot_id).bind(turn).bind(caller.attempt).bind(provider).bind(reason).fetch_one(&mut *tx).await?;
    crate::db::events::turn_changed(&mut tx, caller.bot_id, turn).await?;
    tx.commit().await?;
    Ok(setup)
}
pub async fn settle_setup(
    pool: &SqlitePool,
    id: Uuid,
    connection: Option<Uuid>,
) -> Result<Setup, Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let setup: Setup = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SETUP_COLUMNS} FROM social_setups WHERE id=?1"
    )))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    let wanted = if connection.is_some() {
        "connected"
    } else {
        "cancelled"
    };
    if setup.status != "pending" {
        return if setup.status == wanted && setup.connection_id == connection {
            Ok(setup)
        } else {
            Err(Error::Conflict)
        };
    }
    if let Some(cid) = connection {
        let valid: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM social_connections WHERE id=?1 AND provider=?2 AND state='active')",
        ).bind(cid).bind(setup.provider).fetch_one(&mut *tx).await?;
        if !valid {
            return Err(Error::Forbidden);
        }
    }
    let next = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "UPDATE social_setups SET status=?2,connection_id=?3 WHERE id=?1 RETURNING {SETUP_COLUMNS}"
    )))
    .bind(id)
    .bind(wanted)
    .bind(connection)
    .fetch_one(&mut *tx)
    .await?;
    crate::db::turns::continue_waiting(&mut tx, setup.turn_id).await?;
    crate::db::events::turn_changed(&mut tx, setup.bot_id, setup.turn_id).await?;
    tx.commit().await?;
    Ok(next)
}

pub async fn setup_outcomes(
    conn: &mut SqliteConnection,
    turn: Uuid,
) -> Result<Vec<Setup>, sqlx::Error> {
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT {SETUP_COLUMNS} FROM social_setups WHERE turn_id=?1 AND status<>'pending' AND delivered_attempt IS NULL"))).bind(turn).fetch_all(conn).await
}

pub async fn announce_drafts(
    conn: &mut SqliteConnection,
    connection: Uuid,
    action: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    let rows:Vec<(Uuid,Uuid)>=sqlx::query_as("SELECT bot_id,turn_id FROM outgoing_drafts WHERE json_extract(delivery,'$.via')='social' AND replace(json_extract(delivery,'$.account_id'),'-','')=lower(hex(?1)) AND (?2 IS NULL OR replace(json_extract(delivery,'$.action_id'),'-','')=lower(hex(?2)))").bind(connection).bind(action).fetch_all(&mut *conn).await?;
    for (bot, turn) in rows {
        crate::db::events::turn_changed(conn, bot, turn).await?;
    }
    Ok(())
}
pub async fn uncertain(
    pool: &SqlitePool,
    connection: Uuid,
    action: Option<Uuid>,
) -> Result<(), Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE social_actions SET status='uncertain',result='{}' WHERE connection_id=?1 AND status='dispatching' AND (?2 IS NULL OR action_id=?2)").bind(connection).bind(action).execute(&mut *tx).await?;
    announce_drafts(&mut tx, connection, action).await?;
    tx.commit().await?;
    Ok(())
}
