use super::{
    APP_VERSION, Backend, LATEST_SYNC_KEY, META, MUTATION_MAC, SYNC_KEY, encode, join, prefix,
};
use crate::socials::providers::whatsapp::vault::{Op, Row};
use async_trait::async_trait;
use whatsapp_rust::wacore::{
    appstate::{hash::HashState, processor::AppStateMutationMAC},
    store::{
        error::Result,
        traits::{AppStateSyncKey, AppSyncStore},
    },
};

impl Backend {
    fn mutation_mac_ops(
        name: &str,
        removed: &[Vec<u8>],
        added: &[AppStateMutationMAC],
    ) -> (Vec<String>, Vec<(String, Vec<u8>)>) {
        let removed = removed
            .iter()
            .map(|mac| join(&[name, &hex::encode(mac)]))
            .collect();
        let added = added
            .iter()
            .map(|m| {
                (
                    join(&[name, &hex::encode(&m.index_mac)]),
                    m.value_mac.clone(),
                )
            })
            .collect();
        (removed, added)
    }
}

#[async_trait]
impl AppSyncStore for Backend {
    async fn get_sync_key(&self, key_id: &[u8]) -> Result<Option<AppStateSyncKey>> {
        self.get_json(SYNC_KEY, &hex::encode(key_id)).await
    }

    async fn set_sync_key(&self, key_id: &[u8], key: AppStateSyncKey) -> Result<()> {
        let name = hex::encode(key_id);
        let value = encode(&key)?;
        let key_row = Row::new(&name, &value);
        let latest = Row::new(LATEST_SYNC_KEY, key_id);
        Ok(self
            .vault
            .write(&[Op::Put(SYNC_KEY, &key_row), Op::Put(META, &latest)])
            .await?)
    }

    async fn get_version(&self, name: &str) -> Result<Option<HashState>> {
        self.get_json(APP_VERSION, name).await
    }

    async fn delete_version(&self, name: &str) -> Result<()> {
        self.delete_one(APP_VERSION, name).await
    }

    async fn set_version(&self, name: &str, state: HashState) -> Result<()> {
        self.put_json(APP_VERSION, name, &state).await
    }

    async fn put_mutation_macs(
        &self,
        name: &str,
        _version: u64,
        mutations: &[AppStateMutationMAC],
    ) -> Result<()> {
        let (_, added) = Self::mutation_mac_ops(name, &[], mutations);
        let rows: Vec<Row<'_>> = added.iter().map(|(key, mac)| Row::new(key, mac)).collect();
        Ok(self.vault.put_rows(MUTATION_MAC, &rows).await?)
    }

    async fn get_mutation_mac(&self, name: &str, index_mac: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .vault
            .get(MUTATION_MAC, &join(&[name, &hex::encode(index_mac)]))
            .await?)
    }

    async fn delete_mutation_macs(&self, name: &str, index_macs: &[Vec<u8>]) -> Result<()> {
        let (removed, _) = Self::mutation_mac_ops(name, index_macs, &[]);
        Ok(self.vault.delete(MUTATION_MAC, &removed).await?)
    }

    async fn commit_patch(
        &self,
        name: &str,
        state: HashState,
        removed_index_macs: &[Vec<u8>],
        added: &[AppStateMutationMAC],
    ) -> Result<()> {
        let version = encode(&state)?;
        let version_row = Row::new(name, &version);
        let (removed, added) = Self::mutation_mac_ops(name, removed_index_macs, added);
        let added: Vec<Row<'_>> = added.iter().map(|(key, mac)| Row::new(key, mac)).collect();
        let mut ops = vec![Op::Put(APP_VERSION, &version_row)];
        ops.extend(removed.iter().map(|key| Op::Delete(MUTATION_MAC, key)));
        ops.extend(added.iter().map(|row| Op::Put(MUTATION_MAC, row)));
        Ok(self.vault.write(&ops).await?)
    }

    async fn clear_mutation_macs(&self, name: &str) -> Result<()> {
        let prefix = prefix(&[name]);
        Ok(self
            .vault
            .write(&[Op::DeletePrefix(MUTATION_MAC, &prefix)])
            .await?)
    }

    async fn get_latest_sync_key_id(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.vault.get(META, LATEST_SYNC_KEY).await?)
    }
}
