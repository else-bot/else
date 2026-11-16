//! Sealed key-value rows for one linked device's store. Keys stay readable
//! so the client can look rows up and sweep them; every value is sealed with
//! the connection's device key and bound to its row, so a value copied to
//! another row or store fails to open.

use crate::socials::types::{DeviceKey, Error};
use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305, XNonce,
    aead::{Aead, Payload},
};
use sqlx::SqlitePool;
use uuid::Uuid;

const NONCE: usize = 24;

pub struct Vault {
    pool: SqlitePool,
    store: Uuid,
    cipher: XChaCha20Poly1305,
}

/// One row to write: its key, value, and the plaintext `stamp` and `flag`
/// the client sweeps and filters on.
/// One step of an atomic [`Vault::write`].
pub enum Op<'a> {
    Put(&'a str, &'a Row<'a>),
    Delete(&'a str, &'a str),
    DeletePrefix(&'a str, &'a str),
    Flag(&'a str, &'a str, bool),
}

pub struct Row<'a> {
    pub key: &'a str,
    pub value: &'a [u8],
    pub stamp: i64,
    pub flag: bool,
}

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
        let row: Option<(Vec<u8>, i64, bool)> = sqlx::query_as(
            "SELECT value,stamp,flag FROM social_device_state WHERE store_id=?1 AND space=?2 AND key=?3",
        )
        .bind(self.store)
        .bind(space)
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(sealed, stamp, flag)| Ok((self.open(space, key, &sealed)?, stamp, flag)))
            .transpose()
    }

    pub async fn put(&self, space: &str, key: &str, value: &[u8]) -> Result<(), Error> {
        self.put_rows(
            space,
            &[Row {
                key,
                value,
                stamp: 0,
                flag: false,
            }],
        )
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        for op in ops {
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
                    sqlx::query(
                        "DELETE FROM social_device_state WHERE store_id=?1 AND space=?2 AND substr(CAST(key AS BLOB),1,length(CAST(?3 AS BLOB)))=CAST(?3 AS BLOB)",
                    )
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
        }
        tx.commit().await?;
        Ok(())
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

    /// Delete every row whose key starts with `prefix`.
    pub async fn delete_prefix(&self, space: &str, prefix: &str) -> Result<u64, Error> {
        Ok(sqlx::query(
            "DELETE FROM social_device_state WHERE store_id=?1 AND space=?2 AND substr(CAST(key AS BLOB),1,length(CAST(?3 AS BLOB)))=CAST(?3 AS BLOB)",
        )
        .bind(self.store)
        .bind(space)
        .bind(prefix)
        .execute(&self.pool)
        .await?
        .rows_affected())
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
        Ok(sqlx::query_scalar(
            "SELECT key FROM social_device_state WHERE store_id=?1 AND space=?2 AND substr(CAST(key AS BLOB),1,length(CAST(?3 AS BLOB)))=CAST(?3 AS BLOB) ORDER BY key",
        )
        .bind(self.store)
        .bind(space)
        .bind(prefix)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Rows whose key starts with `prefix`, in key order.
    pub async fn scan(&self, space: &str, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, Error> {
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT key,value FROM social_device_state WHERE store_id=?1 AND space=?2 AND substr(CAST(key AS BLOB),1,length(CAST(?3 AS BLOB)))=CAST(?3 AS BLOB) ORDER BY key",
        )
        .bind(self.store)
        .bind(space)
        .bind(prefix)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(key, sealed)| {
                let value = self.open(space, &key, &sealed)?;
                Ok((key, value))
            })
            .collect()
    }

    pub async fn exists(&self, space: &str, prefix: &str) -> Result<bool, Error> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM social_device_state WHERE store_id=?1 AND space=?2 AND substr(CAST(key AS BLOB),1,length(CAST(?3 AS BLOB)))=CAST(?3 AS BLOB))",
        )
        .bind(self.store)
        .bind(space)
        .bind(prefix)
        .fetch_one(&self.pool)
        .await?)
    }
}
