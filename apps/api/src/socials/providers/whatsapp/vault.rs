//! Sealed key-value rows for one linked device's store. Keys stay readable
//! so the client can look rows up and sweep them; every value is sealed with
//! the connection's device key and bound to its row, so a value copied to
//! another row or store fails to open.

use crate::socials::types::{DeviceKey, Error};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

const NONCE: usize = 24;

/// The rows of one store and space whose key starts with the prefix bound
/// to `?3`, compared as bytes so `LIKE` wildcards and collation never apply.
macro_rules! under_prefix {
    () => {
        "store_id=?1 AND space=?2 AND substr(CAST(key AS BLOB),1,length(CAST(?3 AS BLOB)))=CAST(?3 AS BLOB)"
    };
}

pub struct Vault {
    pool: SqlitePool,
    store: Uuid,
    cipher: XChaCha20Poly1305,
}

/// One step of an atomic [`Vault::write`] or [`Txn::write`].
pub enum Op<'a> {
    Put(&'a str, &'a Row<'a>),
    Delete(&'a str, &'a str),
    DeletePrefix(&'a str, &'a str),
    Flag(&'a str, &'a str, bool),
}

/// One row to write: its key, value, and the plaintext `stamp` and `flag`
/// the client sweeps and filters on.
pub struct Row<'a> {
    pub key: &'a str,
    pub value: &'a [u8],
    pub stamp: i64,
    pub flag: bool,
}

impl<'a> Row<'a> {
    /// A row with no stamp and a clear flag.
    pub fn new(key: &'a str, value: &'a [u8]) -> Self {
        Self {
            key,
            value,
            stamp: 0,
            flag: false,
        }
    }

    pub fn stamped(self, stamp: i64) -> Self {
        Self { stamp, ..self }
    }

    pub fn flagged(self, flag: bool) -> Self {
        Self { flag, ..self }
    }
}

/// What [`Vault::update`] does to the row it read.
pub enum Change {
    Keep,
    Put { value: Vec<u8>, stamp: i64 },
    Delete,
}

/// Sealing adds the nonce and the authentication tag to every value.
const OVERHEAD: usize = NONCE + 16;

impl Vault {
    pub fn new(pool: SqlitePool, store: Uuid, key: &DeviceKey) -> Self {
        Self {
            pool,
            store,
            cipher: XChaCha20Poly1305::new(key.0.as_slice().into()),
        }
    }

    fn aad(&self, space: &str, key: &str) -> Vec<u8> {
        let mut aad = Vec::with_capacity(16 + space.len() + key.len() + 1);
        aad.extend_from_slice(self.store.as_bytes());
        aad.extend_from_slice(space.as_bytes());
        aad.push(0);
        aad.extend_from_slice(key.as_bytes());
        aad
    }

    fn seal(&self, space: &str, key: &str, value: &[u8]) -> Result<Vec<u8>, Error> {
        let mut nonce = [0; NONCE];
        getrandom::fill(&mut nonce).map_err(|_| Error::Crypto)?;
        let aad = self.aad(space, key);
        let sealed = self
            .cipher
            .encrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: value,
                    aad: &aad,
                },
            )
            .map_err(|_| Error::Crypto)?;
        let mut out = nonce.to_vec();
        out.extend_from_slice(&sealed);
        Ok(out)
    }

    fn open(&self, space: &str, key: &str, sealed: &[u8]) -> Result<Vec<u8>, Error> {
        if sealed.len() < NONCE {
            return Err(Error::Crypto);
        }
        let (nonce, body) = sealed.split_at(NONCE);
        let nonce: [u8; NONCE] = nonce.try_into().map_err(|_| Error::Crypto)?;
        let aad = self.aad(space, key);
        self.cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: body,
                    aad: &aad,
                },
            )
            .map_err(|_| Error::Crypto)
    }

    pub async fn get(&self, space: &str, key: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.get_row(space, key).await?.map(|(value, _, _)| value))
    }

    /// A row's value, stamp, and flag.
    pub async fn get_row(
        &self,
        space: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, i64, bool)>, Error> {
        let mut conn = self.pool.acquire().await?;
        self.read_row(&mut conn, space, key).await
    }

    pub async fn put(&self, space: &str, key: &str, value: &[u8]) -> Result<(), Error> {
        self.put_stamped(space, key, value, 0).await
    }

    /// Write one row with the plaintext `stamp` the client sweeps on.
    pub async fn put_stamped(
        &self,
        space: &str,
        key: &str,
        value: &[u8],
        stamp: i64,
    ) -> Result<(), Error> {
        self.put_rows(space, &[Row::new(key, value).stamped(stamp)])
            .await
    }

    /// Write `rows` in one transaction, replacing any with the same key.
    pub async fn put_rows(&self, space: &str, rows: &[Row<'_>]) -> Result<(), Error> {
        let ops: Vec<Op<'_>> = rows.iter().map(|row| Op::Put(space, row)).collect();
        self.write(&ops).await
    }

    pub async fn set_flag(&self, space: &str, keys: &[String], flag: bool) -> Result<(), Error> {
        let ops: Vec<Op<'_>> = keys.iter().map(|key| Op::Flag(space, key, flag)).collect();
        self.write(&ops).await
    }

    pub async fn delete(&self, space: &str, keys: &[String]) -> Result<(), Error> {
        let ops: Vec<Op<'_>> = keys.iter().map(|key| Op::Delete(space, key)).collect();
        self.write(&ops).await
    }

    /// Apply `ops` in order, all or none.
    pub async fn write(&self, ops: &[Op<'_>]) -> Result<(), Error> {
        if ops.is_empty() {
            return Ok(());
        }
        let mut tx = self.begin().await?;
        tx.write(ops).await?;
        tx.commit().await
    }

    /// Open a write transaction that holds SQLite's write lock from its first
    /// statement, so whatever it reads stays current until it commits.
    pub async fn begin(&self) -> Result<Txn<'_>, Error> {
        Ok(Txn {
            vault: self,
            tx: self.pool.begin_with("BEGIN IMMEDIATE").await?,
        })
    }

    async fn apply(&self, tx: &mut SqliteConnection, op: &Op<'_>) -> Result<(), Error> {
        match op {
            Op::Put(space, row) => {
                sqlx::query(
                    "INSERT INTO social_device_state(store_id,space,key,stamp,flag,value) VALUES(?1,?2,?3,?4,?5,?6)
                     ON CONFLICT(store_id,space,key) DO UPDATE SET stamp=excluded.stamp,flag=excluded.flag,value=excluded.value",
                )
                .bind(self.store)
                .bind(*space)
                .bind(row.key)
                .bind(row.stamp)
                .bind(row.flag)
                .bind(self.seal(space, row.key, row.value)?)
                .execute(&mut *tx)
                .await?;
            }
            Op::Delete(space, key) => {
                sqlx::query(
                    "DELETE FROM social_device_state WHERE store_id=?1 AND space=?2 AND key=?3",
                )
                .bind(self.store)
                .bind(*space)
                .bind(*key)
                .execute(&mut *tx)
                .await?;
            }
            Op::DeletePrefix(space, prefix) => {
                sqlx::query(concat!(
                    "DELETE FROM social_device_state WHERE ",
                    under_prefix!()
                ))
                .bind(self.store)
                .bind(*space)
                .bind(*prefix)
                .execute(&mut *tx)
                .await?;
            }
            Op::Flag(space, key, flag) => {
                sqlx::query(
                    "UPDATE social_device_state SET flag=?4 WHERE store_id=?1 AND space=?2 AND key=?3",
                )
                .bind(self.store)
                .bind(*space)
                .bind(*key)
                .bind(*flag)
                .execute(&mut *tx)
                .await?;
            }
        }
        Ok(())
    }

    async fn read_row(
        &self,
        tx: &mut SqliteConnection,
        space: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, i64, bool)>, Error> {
        let row: Option<(Vec<u8>, i64, bool)> = sqlx::query_as(
            "SELECT value,stamp,flag FROM social_device_state WHERE store_id=?1 AND space=?2 AND key=?3",
        )
        .bind(self.store)
        .bind(space)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?;
        row.map(|(sealed, stamp, flag)| Ok((self.open(space, key, &sealed)?, stamp, flag)))
            .transpose()
    }

    async fn read_scan(
        &self,
        tx: &mut SqliteConnection,
        space: &str,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, Error> {
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(concat!(
            "SELECT key,value FROM social_device_state WHERE ",
            under_prefix!(),
            " ORDER BY key"
        ))
        .bind(self.store)
        .bind(space)
        .bind(prefix)
        .fetch_all(&mut *tx)
        .await?;
        rows.into_iter()
            .map(|(key, sealed)| {
                let value = self.open(space, &key, &sealed)?;
                Ok((key, value))
            })
            .collect()
    }

    async fn read_exists(
        &self,
        tx: &mut SqliteConnection,
        space: &str,
        prefix: &str,
    ) -> Result<bool, Error> {
        Ok(sqlx::query_scalar(concat!(
            "SELECT EXISTS(SELECT 1 FROM social_device_state WHERE ",
            under_prefix!(),
            ")"
        ))
        .bind(self.store)
        .bind(space)
        .bind(prefix)
        .fetch_one(&mut *tx)
        .await?)
    }

    /// The value at `key` in an open transaction, refusing one longer than
    /// `max` bytes before reading it.
    async fn read_bounded(
        &self,
        tx: &mut sqlx::SqliteConnection,
        space: &str,
        key: &str,
        max: usize,
    ) -> Result<Option<Vec<u8>>, Error> {
        // The value is selected only within its bound, in the same statement
        // that measures it. The table is STRICT, so a value is always a BLOB.
        let bound = i64::try_from(max.saturating_add(OVERHEAD)).unwrap_or(i64::MAX);
        let row: Option<(i64, Option<Vec<u8>>)> = sqlx::query_as(
            "SELECT length(value), CASE WHEN length(value)<=?4 THEN value END FROM social_device_state WHERE store_id=?1 AND space=?2 AND key=?3",
        )
        .bind(self.store)
        .bind(space)
        .bind(key)
        .bind(bound)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((_, sealed)) = row else {
            return Ok(None);
        };
        let Some(sealed) = sealed else {
            return Err(Error::Invalid(
                "A stored WhatsApp row is larger than its bound.",
            ));
        };
        let value = self.open(space, key, &sealed)?;
        if value.len() > max {
            return Err(Error::Invalid(
                "A stored WhatsApp row is larger than its bound.",
            ));
        }
        Ok(Some(value))
    }

    /// The value at `key`, refusing one longer than `max` bytes.
    pub async fn get_bounded(
        &self,
        space: &str,
        key: &str,
        max: usize,
    ) -> Result<Option<Vec<u8>>, Error> {
        let mut tx = self.pool.begin().await?;
        let value = self.read_bounded(&mut tx, space, key, max).await?;
        tx.commit().await?;
        Ok(value)
    }

    /// Read the row at `key`, refusing one longer than `max` bytes, and apply
    /// the change `decide` picks for it in the same transaction.
    pub async fn update<T>(
        &self,
        space: &str,
        key: &str,
        max: usize,
        decide: impl FnOnce(Option<&[u8]>) -> (Change, T),
    ) -> Result<T, Error> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let current = self.read_bounded(&mut tx, space, key, max).await?;
        let (change, out) = decide(current.as_deref());
        let row;
        let op = match &change {
            Change::Keep => None,
            Change::Put { value, stamp } => {
                row = Row::new(key, value).stamped(*stamp);
                Some(Op::Put(space, &row))
            }
            Change::Delete => Some(Op::Delete(space, key)),
        };
        if let Some(op) = op {
            self.apply(&mut tx, &op).await?;
        }
        tx.commit().await?;
        Ok(out)
    }

    /// In one snapshot, list up to `limit` keys starting with `prefix`, let
    /// `choose` pick one by its suffix, and read that row bounded by `max`.
    /// A suffix longer than `max_suffix` bytes is refused before it is read.
    pub async fn pick_bounded(
        &self,
        space: &str,
        prefix: &str,
        limit: i64,
        max_suffix: usize,
        max: usize,
        choose: impl FnOnce(&[&str]) -> Result<Option<String>, Error>,
    ) -> Result<Option<(String, Vec<u8>)>, Error> {
        let mut tx = self.pool.begin().await?;
        let longest = i64::try_from(prefix.len().saturating_add(max_suffix)).unwrap_or(i64::MAX);
        let keys: Vec<Option<String>> = sqlx::query_scalar(concat!(
            "SELECT CASE WHEN length(CAST(key AS BLOB))<=?5 THEN key END",
            " FROM social_device_state WHERE ",
            under_prefix!(),
            " ORDER BY key LIMIT ?4"
        ))
        .bind(self.store)
        .bind(space)
        .bind(prefix)
        .bind(limit)
        .bind(longest)
        .fetch_all(&mut *tx)
        .await?;
        let keys: Vec<String> = keys
            .into_iter()
            .collect::<Option<_>>()
            .ok_or(Error::Invalid(
                "A stored WhatsApp key is longer than its bound.",
            ))?;
        let suffixes: Vec<&str> = keys
            .iter()
            .filter_map(|key| key.strip_prefix(prefix))
            .collect();
        let Some(suffix) = choose(&suffixes)? else {
            return Ok(None);
        };
        let key = format!("{prefix}{suffix}");
        let value = self
            .read_bounded(&mut tx, space, &key, max)
            .await?
            .ok_or(Error::Invalid(
                "A stored WhatsApp row disappeared while it was read.",
            ))?;
        tx.commit().await?;
        Ok(Some((suffix, value)))
    }

    /// Delete and return the row at `key`, so only one caller takes it.
    pub async fn take(&self, space: &str, key: &str) -> Result<Option<Vec<u8>>, Error> {
        let sealed: Option<Vec<u8>> = sqlx::query_scalar(
            "DELETE FROM social_device_state WHERE store_id=?1 AND space=?2 AND key=?3 RETURNING value",
        )
        .bind(self.store)
        .bind(space)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        sealed.map(|s| self.open(space, key, &s)).transpose()
    }

    /// Delete rows stamped before `cutoff`.
    pub async fn expire(&self, space: &str, cutoff: i64) -> Result<u32, Error> {
        let gone = sqlx::query(
            "DELETE FROM social_device_state WHERE store_id=?1 AND space=?2 AND stamp<?3",
        )
        .bind(self.store)
        .bind(space)
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(u32::try_from(gone).unwrap_or(u32::MAX))
    }

    /// Delete rows with a nonzero stamp at or before `cutoff`; a zero stamp
    /// never expires.
    pub async fn expire_due(&self, space: &str, cutoff: i64) -> Result<u32, Error> {
        let gone = sqlx::query(
            "DELETE FROM social_device_state WHERE store_id=?1 AND space=?2 AND stamp<>0 AND stamp<=?3",
        )
        .bind(self.store)
        .bind(space)
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(u32::try_from(gone).unwrap_or(u32::MAX))
    }

    /// The greatest key in `space`.
    pub async fn last_key(&self, space: &str) -> Result<Option<String>, Error> {
        Ok(sqlx::query_scalar(
            "SELECT max(key) FROM social_device_state WHERE store_id=?1 AND space=?2",
        )
        .bind(self.store)
        .bind(space)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Keys starting with `prefix`, in key order.
    pub async fn keys(&self, space: &str, prefix: &str) -> Result<Vec<String>, Error> {
        Ok(sqlx::query_scalar(concat!(
            "SELECT key FROM social_device_state WHERE ",
            under_prefix!(),
            " ORDER BY key"
        ))
        .bind(self.store)
        .bind(space)
        .bind(prefix)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Rows whose key starts with `prefix`, in key order.
    pub async fn scan(&self, space: &str, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, Error> {
        let mut conn = self.pool.acquire().await?;
        self.read_scan(&mut conn, space, prefix).await
    }

    pub async fn exists(&self, space: &str, prefix: &str) -> Result<bool, Error> {
        let mut conn = self.pool.acquire().await?;
        self.read_exists(&mut conn, space, prefix).await
    }
}

/// A [`Vault::begin`] transaction: reads see the rows as they stand under
/// the write lock, and writes land together on [`Txn::commit`]. Dropping it
/// rolls everything back.
pub struct Txn<'a> {
    vault: &'a Vault,
    tx: sqlx::Transaction<'static, sqlx::Sqlite>,
}

impl Txn<'_> {
    pub async fn get(&mut self, space: &str, key: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.get_row(space, key).await?.map(|(value, _, _)| value))
    }

    /// A row's value, stamp, and flag.
    pub async fn get_row(
        &mut self,
        space: &str,
        key: &str,
    ) -> Result<Option<(Vec<u8>, i64, bool)>, Error> {
        self.vault.read_row(&mut self.tx, space, key).await
    }

    /// Rows whose key starts with `prefix`, in key order.
    pub async fn scan(
        &mut self,
        space: &str,
        prefix: &str,
    ) -> Result<Vec<(String, Vec<u8>)>, Error> {
        self.vault.read_scan(&mut self.tx, space, prefix).await
    }

    pub async fn exists(&mut self, space: &str, prefix: &str) -> Result<bool, Error> {
        self.vault.read_exists(&mut self.tx, space, prefix).await
    }

    /// Apply `ops` in order inside this transaction.
    pub async fn write(&mut self, ops: &[Op<'_>]) -> Result<(), Error> {
        for op in ops {
            self.vault.apply(&mut self.tx, op).await?;
        }
        Ok(())
    }

    pub async fn commit(self) -> Result<(), Error> {
        Ok(self.tx.commit().await?)
    }
}
