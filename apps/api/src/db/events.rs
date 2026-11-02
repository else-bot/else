//! Hooks for the per-bot event journal. Every mutation of visible history
//! or turn state calls one of these inside its transaction. The journal and
//! streaming arrive in POC-006; until then these record nothing.
#![allow(clippy::unused_async)]

use sqlx::{PgConnection, Postgres, Transaction};
use uuid::Uuid;

pub async fn turn_accepted(
    _tx: &mut Transaction<'_, Postgres>,
    _bot_id: Uuid,
    _turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    Ok(())
}

pub async fn turn_changed(
    _conn: &mut PgConnection,
    _bot_id: Uuid,
    _turn_id: Uuid,
) -> Result<(), sqlx::Error> {
    Ok(())
}

pub async fn item_upsert(
    _conn: &mut PgConnection,
    _bot_id: Uuid,
    _item_id: Uuid,
) -> Result<(), sqlx::Error> {
    Ok(())
}

pub async fn item_delta(
    _conn: &mut PgConnection,
    _bot_id: Uuid,
    _conversation_id: Uuid,
    _item_id: Uuid,
    _offset: i64,
    _text: &str,
) -> Result<(), sqlx::Error> {
    Ok(())
}
