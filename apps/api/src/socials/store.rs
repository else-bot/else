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

const COLUMNS: &str = "id,provider,remote_id,display_name,username,state,health,generation,instructions,EXISTS(SELECT 1 FROM social_bindings sb WHERE sb.connection_id=social_connections.id AND json_extract(sb.access,'$.wake_policy')='all') AS message_content";
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
pub async fn set_state(pool: &SqlitePool, id: Uuid, state: &str) -> Result<Connection, Error> {
    let mut removed = connection(pool, id).await?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let updated=sqlx::query("UPDATE social_connections SET state=?2,generation=generation+1,health=CASE WHEN ?2='active' THEN 'connecting' ELSE ?2 END,credential=CASE WHEN ?2='removed' THEN NULL ELSE credential END,credential_salt=CASE WHEN ?2='removed' THEN NULL ELSE credential_salt END WHERE id=?1 AND state<>'removed'").bind(id).bind(state).execute(&mut *tx).await?;
    if updated.rows_affected() == 0 {
        return Err(Error::NotFound);
    }
    sqlx::query(
        "UPDATE social_deliveries SET status='revoked' WHERE connection_id=?1 AND status='pending'",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if state == "removed" {
        sqlx::query("DELETE FROM social_bindings WHERE connection_id=?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    removed.state = state.into();
    removed.generation += 1;
    removed.health = state.into();
    Ok(removed)
}
pub async fn bindings(pool: &SqlitePool, id: Uuid) -> Result<Vec<Binding>, Error> {
    let rows: Vec<(Uuid, Uuid, Json<BindingInput>)> =
        sqlx::query_as("SELECT id,bot_id,access FROM social_bindings WHERE connection_id=?1")
            .bind(id)
            .fetch_all(pool)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(bid, bot, Json(access))| Binding {
            id: bid,
            connection_id: id,
            bot_id: bot,
            access,
        })
        .collect())
}
pub async fn bind(
    pool: &SqlitePool,
    id: Uuid,
    bot: Uuid,
    input: BindingInput,
) -> Result<Binding, Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let binding = bind_in(&mut tx, id, bot, input).await?;
    tx.commit().await?;
    Ok(binding)
}

async fn bind_in(
    conn: &mut SqliteConnection,
    id: Uuid,
    bot: Uuid,
    input: BindingInput,
) -> Result<Binding, Error> {
    input.validate()?;
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM bots b,social_connections c WHERE b.id=?1 AND b.status='active' AND c.id=?2 AND c.state<>'removed')").bind(bot).bind(id).fetch_one(&mut *conn).await?;
    if !active {
        return Err(Error::NotFound);
    }
    let known: Vec<String> =
        sqlx::query_scalar("SELECT id FROM social_destinations WHERE connection_id=?1")
            .bind(id)
            .fetch_all(&mut *conn)
            .await?;
    if input.destinations.iter().any(|d| !known.contains(d)) {
        return Err(Error::Invalid(
            "A selected destination is not visible to this connection.",
        ));
    }
    let bid = Uuid::new_v4();
    sqlx::query("INSERT INTO social_bindings(id,connection_id,bot_id,access) VALUES(?1,?2,?3,?4) ON CONFLICT(connection_id,bot_id) DO UPDATE SET id=excluded.id,access=excluded.access").bind(bid).bind(id).bind(bot).bind(Json(&input)).execute(&mut *conn).await?;
    sqlx::query("UPDATE social_connections SET generation=generation+1 WHERE id=?1")
        .bind(id)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE social_deliveries SET status='revoked' WHERE connection_id=?1 AND bot_id=?2 AND status='pending'").bind(id).bind(bot).execute(&mut *conn).await?;
    Ok(Binding {
        id: bid,
        connection_id: id,
        bot_id: bot,
        access: input,
    })
}
pub async fn revoke(pool: &SqlitePool, id: Uuid, bot: Uuid) -> Result<(), Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("DELETE FROM social_bindings WHERE connection_id=?1 AND bot_id=?2")
        .bind(id)
        .bind(bot)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE social_connections SET generation=generation+1 WHERE id=?1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE social_deliveries SET status='revoked' WHERE connection_id=?1 AND bot_id=?2 AND status='pending'").bind(id).bind(bot).execute(&mut *tx).await?;
    tx.commit().await?;
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

pub async fn authorize(
    conn: &mut SqliteConnection,
    c: &Connection,
    bot: Uuid,
    turn: Uuid,
    destination: Option<&str>,
    write: bool,
    send: bool,
) -> Result<BindingInput, Error> {
    let row:Option<(Json<BindingInput>,String)>=sqlx::query_as("SELECT g.access,i.kind FROM social_bindings g JOIN social_connections c ON c.id=g.connection_id JOIN turns t ON t.bot_id=g.bot_id JOIN bots b ON b.id=t.bot_id JOIN items i ON i.id=t.owner_item_id WHERE c.id=?1 AND c.state='active' AND c.generation=?2 AND g.bot_id=?3 AND t.id=?4 AND t.status IN ('sending','running') AND b.status='active' AND NOT b.cancel_pending").bind(c.id).bind(c.generation).bind(bot).bind(turn).fetch_optional(&mut *conn).await?;
    let (Json(mut access), kind) = row.ok_or(Error::Forbidden)?;
    if kind == "connector" {
        let scoped: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT d.destination FROM social_deliveries d \
             JOIN social_bindings b ON b.id=d.binding_id AND b.connection_id=d.connection_id \
             WHERE d.turn_id=?1 AND d.connection_id=?2 AND b.bot_id=?3",
        )
        .bind(turn)
        .bind(c.id)
        .bind(bot)
        .fetch_all(&mut *conn)
        .await?;
        access.destinations.retain(|d| scoped.contains(d));
    }
    if !access.permissions.read
        || write && !access.permissions.write
        || send && !access.permissions.send
        || destination.is_some_and(|d| !access.destinations.iter().any(|x| x == d))
    {
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
    let old:Option<(Vec<u8>,String,Option<Json<Value>>)>=sqlx::query_as("SELECT fingerprint,status,result FROM social_actions WHERE connection_id=?1 AND action_id=?2").bind(c.id).bind(id).fetch_optional(&mut *tx).await?;
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
pub async fn finish_action(
    pool: &SqlitePool,
    connection: Uuid,
    action: Uuid,
    result: &Result<Value, Error>,
) -> Result<Value, Error> {
    let status = match result {
        Ok(_) => "succeeded",
        Err(Error::Invalid(_) | Error::Unsupported | Error::Forbidden) => "failed",
        Err(_) => "uncertain",
    };
    let value = match result {
        Ok(v) => v.clone(),
        Err(e) => json!({"error":e.to_string()}),
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE social_actions SET status=?3,result=?4 WHERE connection_id=?1 AND action_id=?2 AND status='dispatching'").bind(connection).bind(action).bind(status).bind(Json(&value)).execute(&mut *tx).await?;
    announce_drafts(&mut tx, connection, Some(action)).await?;
    tx.commit().await?;
    Ok(json!({"action_id":action,"status":status,"result":value}))
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
        let rows: Vec<(Uuid, Uuid, Json<BindingInput>)> =
            sqlx::query_as("SELECT id,bot_id,access FROM social_bindings WHERE connection_id=?1")
                .bind(c.id)
                .fetch_all(&mut *tx)
                .await?;
        for (bid, bot, Json(b)) in rows {
            if b.permissions.read
                && b.permissions.wake
                && b.destinations.contains(&event.destination.id)
                && match b.wake_policy {
                    WakePolicy::Never => false,
                    WakePolicy::All => true,
                    WakePolicy::Mentions => event.mentioned || event.direct,
                }
            {
                sqlx::query("INSERT OR IGNORE INTO social_deliveries(connection_id,event_id,bot_id,binding_id,destination,message_id) VALUES(?1,?2,?3,?4,?5,?6)").bind(c.id).bind(&event.event_id).bind(bot).bind(bid).bind(&event.destination.id).bind(&event.message_id).execute(&mut *tx).await?;
            }
        }
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
    completion: Option<(Uuid, BindingInput)>,
) -> Result<Setup, Error> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let setup: Setup = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {SETUP_COLUMNS} FROM social_setups WHERE id=?1"
    )))
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(Error::NotFound)?;
    let connection = completion.as_ref().map(|(id, _)| *id);
    let wanted = if connection.is_some() {
        "connected"
    } else {
        "cancelled"
    };
    if setup.status != "pending" {
        return if setup.status == wanted && setup.connection_id == connection {
            if let Some((cid, access)) = &completion {
                let saved: Option<Json<BindingInput>> = sqlx::query_scalar(
                    "SELECT access FROM social_bindings WHERE connection_id=?1 AND bot_id=?2",
                )
                .bind(cid)
                .bind(setup.bot_id)
                .fetch_optional(&mut *tx)
                .await?;
                if saved.as_ref().map(|saved| &saved.0) != Some(access) {
                    return Err(Error::Conflict);
                }
            }
            Ok(setup)
        } else {
            Err(Error::Conflict)
        };
    }
    if let Some((cid, access)) = completion {
        if !access.permissions.read {
            return Err(Error::Invalid("Enable reading to complete setup."));
        }
        let valid: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM social_connections WHERE id=?1 AND provider=?2 AND state='active')",
        ).bind(cid).bind(setup.provider).fetch_one(&mut *tx).await?;
        if !valid {
            return Err(Error::Forbidden);
        }
        bind_in(&mut tx, cid, setup.bot_id, access).await?;
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
