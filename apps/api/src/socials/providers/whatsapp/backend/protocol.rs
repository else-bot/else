use super::{
    BASE_KEY, Backend, DEVICE_LIST, GROUP_META, LID, PN, SENT_MSG, SK_DEVICE, TC_TOKEN, decode,
    encode, join, now, prefix,
};
use crate::socials::providers::whatsapp::vault::{Op, Row};
use async_trait::async_trait;
use std::sync::Arc;
use whatsapp_rust::wacore::store::{
    error::{Result, StoreError},
    traits::{
        DeviceListRecord, LidPnMappingEntry, PendingInboundKey, PendingInboundLease,
        PendingInboundMutation, PendingInboundPayload, PendingInboundRecord,
        PendingInboundRevision, PendingInboundRow, ProtocolStore, TcTokenEntry,
    },
};

impl Backend {
    /// Read the token row for `jid`, merge into it with `merge`, and write
    /// it back, all under one write lock so concurrent merges never lose an
    /// input.
    async fn merge_tc_token(
        &self,
        jid: &str,
        merge: impl FnOnce(Option<TcTokenEntry>) -> TcTokenEntry,
    ) -> Result<()> {
        let mut tx = self.vault.begin().await?;
        let current = tx
            .get(TC_TOKEN, jid)
            .await?
            .map(|bytes| decode(&bytes))
            .transpose()?;
        let value = encode(&merge(current))?;
        tx.write(&[Op::Put(TC_TOKEN, &Row::new(jid, &value))])
            .await?;
        Ok(tx.commit().await?)
    }
}

#[async_trait]
impl ProtocolStore for Backend {
    async fn get_sender_key_devices(&self, group_jid: &str) -> Result<Vec<(String, bool)>> {
        let prefix = prefix(&[group_jid]);
        Ok(self
            .vault
            .scan(SK_DEVICE, &prefix)
            .await?
            .into_iter()
            .map(|(key, value)| {
                (
                    key[prefix.len()..].to_owned(),
                    value.first().is_some_and(|b| *b == 1),
                )
            })
            .collect())
    }

    async fn set_sender_key_status(&self, group_jid: &str, entries: &[(&str, bool)]) -> Result<()> {
        let keys: Vec<String> = entries
            .iter()
            .map(|(device, _)| join(&[group_jid, device]))
            .collect();
        let rows: Vec<Row<'_>> = entries
            .iter()
            .zip(&keys)
            .map(|((_, has_key), key)| Row::new(key, if *has_key { &[1] } else { &[0] }))
            .collect();
        Ok(self.vault.put_rows(SK_DEVICE, &rows).await?)
    }

    async fn clear_sender_key_devices(&self, group_jid: &str) -> Result<()> {
        let prefix = prefix(&[group_jid]);
        Ok(self
            .vault
            .write(&[Op::DeletePrefix(SK_DEVICE, &prefix)])
            .await?)
    }

    async fn delete_sender_key_device_rows(&self, device_jids: &[&str]) -> Result<()> {
        if device_jids.is_empty() {
            return Ok(());
        }
        let doomed: Vec<String> = self
            .vault
            .keys(SK_DEVICE, "")
            .await?
            .into_iter()
            .filter(|key| {
                key.split_once('\0')
                    .is_some_and(|(_, device)| device_jids.contains(&device))
            })
            .collect();
        Ok(self.vault.delete(SK_DEVICE, &doomed).await?)
    }

    async fn clear_all_sender_key_devices(&self) -> Result<()> {
        Ok(self.vault.write(&[Op::DeletePrefix(SK_DEVICE, "")]).await?)
    }

    async fn get_lid_mapping(&self, lid: &str) -> Result<Option<LidPnMappingEntry>> {
        self.get_json(LID, lid).await
    }

    async fn get_pn_mapping(&self, phone: &str) -> Result<Option<LidPnMappingEntry>> {
        let Some(lid) = self.vault.get(PN, phone).await? else {
            return Ok(None);
        };
        let lid = String::from_utf8(lid)
            .map_err(|_| StoreError::Validation("stored LID is not text".into()))?;
        self.get_lid_mapping(&lid).await
    }

    async fn put_lid_mapping(&self, entry: &LidPnMappingEntry) -> Result<()> {
        let mut tx = self.vault.begin().await?;
        let old_phone = tx
            .get(LID, &entry.lid)
            .await?
            .map(|bytes| decode::<LidPnMappingEntry>(&bytes))
            .transpose()?
            .map(|old| old.phone_number)
            .filter(|phone| *phone != entry.phone_number);
        let value = encode(entry)?;
        let lid_row = Row::new(&entry.lid, &value);
        let pn_row = Row::new(&entry.phone_number, entry.lid.as_bytes());
        let mut ops = Vec::new();
        if let Some(phone) = &old_phone {
            ops.push(Op::Delete(PN, phone));
        }
        ops.push(Op::Put(PN, &pn_row));
        ops.push(Op::Put(LID, &lid_row));
        tx.write(&ops).await?;
        Ok(tx.commit().await?)
    }

    async fn get_all_lid_mappings(&self) -> Result<Vec<LidPnMappingEntry>> {
        self.vault
            .scan(LID, "")
            .await?
            .into_iter()
            .map(|(_, value)| decode(&value))
            .collect()
    }

    async fn save_base_key(&self, address: &str, message_id: &str, base_key: &[u8]) -> Result<()> {
        let key = join(&[address, message_id]);
        Ok(self
            .vault
            .put_stamped(BASE_KEY, &key, base_key, now())
            .await?)
    }

    async fn has_same_base_key(
        &self,
        address: &str,
        message_id: &str,
        current_base_key: &[u8],
    ) -> Result<bool> {
        Ok(self
            .vault
            .get(BASE_KEY, &join(&[address, message_id]))
            .await?
            .is_some_and(|stored| stored == current_base_key))
    }

    async fn delete_base_key(&self, address: &str, message_id: &str) -> Result<()> {
        self.delete_one(BASE_KEY, &join(&[address, message_id]))
            .await
    }

    async fn delete_expired_base_keys(&self, cutoff_timestamp: i64) -> Result<u32> {
        Ok(self.vault.expire(BASE_KEY, cutoff_timestamp).await?)
    }

    async fn update_device_list(&self, record: DeviceListRecord) -> Result<()> {
        let user = Arc::clone(&record.user);
        self.put_json(DEVICE_LIST, &user, &record).await
    }

    async fn get_devices(&self, user: &str) -> Result<Option<DeviceListRecord>> {
        self.get_json(DEVICE_LIST, user).await
    }

    async fn delete_devices(&self, user: &str) -> Result<()> {
        self.delete_one(DEVICE_LIST, user).await
    }

    async fn get_group_metadata(&self, group_jid: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.vault.get(GROUP_META, group_jid).await?)
    }

    async fn put_group_metadata(&self, group_jid: &str, blob: &[u8]) -> Result<()> {
        Ok(self.vault.put(GROUP_META, group_jid, blob).await?)
    }

    async fn delete_group_metadata(&self, group_jid: &str) -> Result<()> {
        self.delete_one(GROUP_META, group_jid).await
    }

    async fn get_tc_token(&self, jid: &str) -> Result<Option<TcTokenEntry>> {
        self.get_json(TC_TOKEN, jid).await
    }

    async fn put_tc_token(&self, jid: &str, entry: &TcTokenEntry) -> Result<()> {
        self.put_json(TC_TOKEN, jid, entry).await
    }

    async fn delete_tc_token(&self, jid: &str) -> Result<()> {
        self.delete_one(TC_TOKEN, jid).await
    }

    async fn get_all_tc_token_jids(&self) -> Result<Vec<String>> {
        Ok(self.vault.keys(TC_TOKEN, "").await?)
    }

    async fn delete_expired_tc_tokens(&self, token_cutoff: i64, sender_cutoff: i64) -> Result<u32> {
        let mut tx = self.vault.begin().await?;
        let mut doomed = Vec::new();
        for (jid, value) in tx.scan(TC_TOKEN, "").await? {
            let entry: TcTokenEntry = decode(&value)?;
            let token_live = !entry.token.is_empty() && entry.token_timestamp >= token_cutoff;
            let sender_live = entry.sender_timestamp.is_some_and(|ts| ts >= sender_cutoff);
            if !(token_live || sender_live) {
                doomed.push(jid);
            }
        }
        let ops: Vec<Op<'_>> = doomed.iter().map(|jid| Op::Delete(TC_TOKEN, jid)).collect();
        tx.write(&ops).await?;
        tx.commit().await?;
        Ok(u32::try_from(doomed.len()).unwrap_or(u32::MAX))
    }

    async fn touch_tc_token_sender_timestamp(
        &self,
        jid: &str,
        sender_timestamp: i64,
    ) -> Result<()> {
        self.merge_tc_token(jid, |current| match current {
            Some(mut entry) => {
                entry.sender_timestamp = Some(
                    entry
                        .sender_timestamp
                        .map_or(sender_timestamp, |e| e.max(sender_timestamp)),
                );
                entry
            }
            None => TcTokenEntry {
                token: Vec::new(),
                token_timestamp: sender_timestamp,
                sender_timestamp: Some(sender_timestamp),
            },
        })
        .await
    }

    async fn store_received_tc_token(
        &self,
        jid: &str,
        token: &[u8],
        token_timestamp: i64,
    ) -> Result<()> {
        self.merge_tc_token(jid, |current| match current {
            Some(mut entry) => {
                if entry.token.is_empty() || token_timestamp >= entry.token_timestamp {
                    entry.token = token.to_vec();
                    entry.token_timestamp = token_timestamp;
                }
                entry
            }
            None => TcTokenEntry {
                token: token.to_vec(),
                token_timestamp,
                sender_timestamp: None,
            },
        })
        .await
    }

    async fn store_sent_message(
        &self,
        chat_jid: &str,
        message_id: &str,
        payload: &[u8],
    ) -> Result<()> {
        let key = join(&[chat_jid, message_id]);
        Ok(self
            .vault
            .put_stamped(SENT_MSG, &key, payload, now())
            .await?)
    }

    async fn get_sent_message(&self, chat_jid: &str, message_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .vault
            .get(SENT_MSG, &join(&[chat_jid, message_id]))
            .await?)
    }

    async fn take_sent_message(&self, chat_jid: &str, message_id: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .vault
            .take(SENT_MSG, &join(&[chat_jid, message_id]))
            .await?)
    }

    async fn delete_expired_sent_messages(&self, cutoff_timestamp: i64) -> Result<u32> {
        Ok(self.vault.expire(SENT_MSG, cutoff_timestamp).await?)
    }

    async fn store_pending_inbound(
        &self,
        chat: &str,
        sender: &str,
        id: &str,
        message: &[u8],
    ) -> Result<()> {
        self.pending()
            .store_pending_inbound(chat, sender, id, message)
            .await
    }

    async fn store_pending_inbound_batch(&self, rows: &[PendingInboundRow<'_>]) -> Result<()> {
        self.pending().store_pending_inbound_batch(rows).await
    }

    async fn get_pending_inbound(
        &self,
        chat: &str,
        sender: &str,
        id: &str,
    ) -> Result<Option<Vec<u8>>> {
        self.pending().get_pending_inbound(chat, sender, id).await
    }

    async fn delete_pending_inbound(&self, chat: &str, sender: &str, id: &str) -> Result<()> {
        self.pending()
            .delete_pending_inbound(chat, sender, id)
            .await
    }

    async fn delete_pending_inbound_batch(&self, keys: &[PendingInboundKey<'_>]) -> Result<()> {
        self.pending().delete_pending_inbound_batch(keys).await
    }

    async fn delete_expired_pending_inbound(&self, cutoff_timestamp: i64) -> Result<u32> {
        self.pending()
            .delete_expired_pending_inbound(cutoff_timestamp)
            .await
    }

    async fn read_pending_inbound_bounded(
        &self,
        key: PendingInboundKey<'_>,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<Option<PendingInboundRecord>> {
        self.pending()
            .read_pending_inbound_bounded(key, max_bytes, lease)
            .await
    }

    async fn read_pending_inbound_group_bounded(
        &self,
        key: PendingInboundKey<'_>,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<Option<(String, PendingInboundRecord)>> {
        self.pending()
            .read_pending_inbound_group_bounded(key, max_bytes, lease)
            .await
    }

    async fn replace_pending_inbound_if(
        &self,
        key: PendingInboundKey<'_>,
        expected: PendingInboundRevision,
        replacement: PendingInboundPayload,
        max_bytes: usize,
    ) -> Result<PendingInboundMutation> {
        self.pending()
            .replace_pending_inbound_if(key, expected, replacement, max_bytes)
            .await
    }

    async fn delete_pending_inbound_if(
        &self,
        key: PendingInboundKey<'_>,
        expected: PendingInboundRevision,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<PendingInboundMutation> {
        self.pending()
            .delete_pending_inbound_if(key, expected, max_bytes, lease)
            .await
    }
}
