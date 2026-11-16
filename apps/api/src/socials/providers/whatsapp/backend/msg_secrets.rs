use super::{Backend, MSG_SECRET, join};
use crate::socials::providers::whatsapp::vault::{Op, Row};
use async_trait::async_trait;
use std::collections::HashMap;
use whatsapp_rust::wacore::{
    reporting_token::MESSAGE_SECRET_SIZE,
    store::{
        error::{Result, StoreError},
        traits::{
            MessageSecret, MsgSecretEntry, MsgSecretStore, StoredMessageSecret,
            merge_msg_secret_expiry, merge_msg_secret_message_ts,
        },
    },
};

/// A message secret row: the secret, then the parent message time.
fn secret_value(secret: &[u8; MESSAGE_SECRET_SIZE], message_ts: i64) -> Vec<u8> {
    let mut value = secret.to_vec();
    value.extend_from_slice(&message_ts.to_le_bytes());
    value
}

fn split_secret(value: &[u8]) -> Result<(&[u8], i64)> {
    let at = value
        .len()
        .checked_sub(8)
        .ok_or_else(|| StoreError::Validation("stored message secret is truncated".into()))?;
    let (secret, ts) = value.split_at(at);
    let ts = i64::from_le_bytes(ts.try_into().expect("eight bytes"));
    Ok((secret, ts))
}

#[async_trait]
impl MsgSecretStore for Backend {
    async fn put_msg_secrets(&self, entries: Vec<MsgSecretEntry>) -> Result<usize> {
        let stored = entries.len();
        let mut tx = self.vault.begin().await?;
        let mut merged: HashMap<String, (i64, Vec<u8>)> = HashMap::new();
        for entry in entries {
            let key = join(&[&entry.chat, &entry.sender, &entry.msg_id]);
            let previous = match merged.remove(&key) {
                Some((expires_at, value)) => Some((value, expires_at)),
                None => tx
                    .get_row(MSG_SECRET, &key)
                    .await?
                    .map(|(value, expires_at, _)| (value, expires_at)),
            };
            let (expires_at, message_ts) = match previous {
                Some((value, expires_at)) => (
                    merge_msg_secret_expiry(expires_at, entry.expires_at),
                    merge_msg_secret_message_ts(split_secret(&value)?.1, entry.message_ts),
                ),
                None => (entry.expires_at, entry.message_ts),
            };
            merged.insert(key, (expires_at, secret_value(&entry.secret, message_ts)));
        }
        let rows: Vec<Row<'_>> = merged
            .iter()
            .map(|(key, (expires_at, value))| Row::new(key, value).stamped(*expires_at))
            .collect();
        let ops: Vec<Op<'_>> = rows.iter().map(|row| Op::Put(MSG_SECRET, row)).collect();
        tx.write(&ops).await?;
        tx.commit().await?;
        Ok(stored)
    }

    async fn get_stored_msg_secret(
        &self,
        chat: &str,
        sender: &str,
        msg_id: &str,
    ) -> Result<Option<StoredMessageSecret>> {
        let Some(value) = self
            .vault
            .get(MSG_SECRET, &join(&[chat, sender, msg_id]))
            .await?
        else {
            return Ok(None);
        };
        let (secret, message_ts) = split_secret(&value)?;
        let secret = MessageSecret::try_from(secret).map_err(StoreError::InvalidMessageSecret)?;
        Ok(Some(StoredMessageSecret::new(secret, Some(message_ts))))
    }

    async fn delete_expired_msg_secrets(&self, cutoff_timestamp: i64) -> Result<u32> {
        Ok(self.vault.expire_due(MSG_SECRET, cutoff_timestamp).await?)
    }
}
