use super::{
    Backend, IDENTITY, PREKEY, SENDER_KEY, SESSION, SIGNED_PREKEY, SK_DEVICE, id, of_user, parse_id,
};
use crate::socials::providers::whatsapp::vault::Row;
use async_trait::async_trait;
use std::sync::Arc;
use whatsapp_rust::bytes::Bytes;
use whatsapp_rust::wacore::store::{
    error::{Result, StoreError},
    traits::SignalStore,
};

#[async_trait]
impl SignalStore for Backend {
    async fn put_identity(&self, address: &str, key: [u8; 32]) -> Result<()> {
        Ok(self.vault.put(IDENTITY, address, &key).await?)
    }

    async fn load_identity(&self, address: &str) -> Result<Option<[u8; 32]>> {
        self.vault
            .get(IDENTITY, address)
            .await?
            .map(|bytes| {
                bytes
                    .try_into()
                    .map_err(|_| StoreError::Validation("stored identity is not 32 bytes".into()))
            })
            .transpose()
    }

    async fn delete_identity(&self, address: &str) -> Result<()> {
        self.delete_one(IDENTITY, address).await
    }

    async fn get_session(&self, address: &str) -> Result<Option<Bytes>> {
        Ok(self.vault.get(SESSION, address).await?.map(Bytes::from))
    }

    async fn put_session(&self, address: &str, session: &[u8]) -> Result<()> {
        Ok(self.vault.put(SESSION, address, session).await?)
    }

    async fn put_sessions_batch(&self, sessions: &[(Arc<str>, Bytes)]) -> Result<()> {
        let rows: Vec<Row<'_>> = sessions
            .iter()
            .map(|(address, session)| Row::new(address, session))
            .collect();
        Ok(self.vault.put_rows(SESSION, &rows).await?)
    }

    async fn delete_session(&self, address: &str) -> Result<()> {
        self.delete_one(SESSION, address).await
    }

    async fn has_signal_state_for_user(&self, user: &str) -> Result<bool> {
        for space in [SESSION, IDENTITY] {
            if self
                .vault
                .keys(space, user)
                .await?
                .iter()
                .any(|k| of_user(k, user))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn get_signal_addresses_for_user(&self, user: &str) -> Result<Vec<Arc<str>>> {
        let mut found: Vec<String> = Vec::new();
        for space in [SESSION, IDENTITY] {
            found.extend(self.vault.keys(space, user).await?);
        }
        for key in self.vault.keys(SK_DEVICE, "").await? {
            if let Some((_, device)) = key.split_once('\0') {
                found.push(device.to_owned());
            }
        }
        found.retain(|address| of_user(address, user));
        found.sort();
        found.dedup();
        Ok(found.into_iter().map(Arc::from).collect())
    }

    async fn store_prekey(&self, prekey: u32, record: &[u8], uploaded: bool) -> Result<()> {
        let key = id(prekey);
        Ok(self
            .vault
            .put_rows(PREKEY, &[Row::new(&key, record).flagged(uploaded)])
            .await?)
    }

    async fn store_prekeys_batch(&self, keys: &[(u32, Bytes)], uploaded: bool) -> Result<()> {
        let ids: Vec<String> = keys.iter().map(|(n, _)| id(*n)).collect();
        let rows: Vec<Row<'_>> = keys
            .iter()
            .zip(&ids)
            .map(|((_, record), key)| Row::new(key, record).flagged(uploaded))
            .collect();
        Ok(self.vault.put_rows(PREKEY, &rows).await?)
    }

    async fn load_prekey(&self, prekey: u32) -> Result<Option<Bytes>> {
        Ok(self.vault.get(PREKEY, &id(prekey)).await?.map(Bytes::from))
    }

    async fn mark_prekeys_uploaded(&self, ids: &[u32]) -> Result<()> {
        let keys: Vec<String> = ids.iter().map(|n| id(*n)).collect();
        Ok(self.vault.set_flag(PREKEY, &keys, true).await?)
    }

    async fn remove_prekey(&self, prekey: u32) -> Result<()> {
        self.delete_one(PREKEY, &id(prekey)).await
    }

    async fn get_max_prekey_id(&self) -> Result<u32> {
        self.vault
            .last_key(PREKEY)
            .await?
            .map_or(Ok(0), |key| parse_id(&key))
    }

    async fn store_signed_prekey(&self, prekey: u32, record: &[u8]) -> Result<()> {
        Ok(self.vault.put(SIGNED_PREKEY, &id(prekey), record).await?)
    }

    async fn load_signed_prekey(&self, prekey: u32) -> Result<Option<Vec<u8>>> {
        Ok(self.vault.get(SIGNED_PREKEY, &id(prekey)).await?)
    }

    async fn load_all_signed_prekeys(&self) -> Result<Vec<(u32, Vec<u8>)>> {
        self.vault
            .scan(SIGNED_PREKEY, "")
            .await?
            .into_iter()
            .map(|(key, record)| Ok((parse_id(&key)?, record)))
            .collect()
    }

    async fn remove_signed_prekey(&self, prekey: u32) -> Result<()> {
        self.delete_one(SIGNED_PREKEY, &id(prekey)).await
    }

    async fn put_sender_key(&self, address: &str, record: &[u8]) -> Result<()> {
        Ok(self.vault.put(SENDER_KEY, address, record).await?)
    }

    async fn put_sender_keys_batch(&self, sender_keys: &[(Arc<str>, Bytes)]) -> Result<()> {
        let rows: Vec<Row<'_>> = sender_keys
            .iter()
            .map(|(address, record)| Row::new(address, record))
            .collect();
        Ok(self.vault.put_rows(SENDER_KEY, &rows).await?)
    }

    async fn get_sender_key(&self, address: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.vault.get(SENDER_KEY, address).await?)
    }

    async fn delete_sender_key(&self, address: &str) -> Result<()> {
        self.delete_one(SENDER_KEY, address).await
    }
}
