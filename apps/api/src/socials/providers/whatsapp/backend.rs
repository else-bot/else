//! The WhatsApp client's key store, kept in sealed [`Vault`] rows so a linked
//! device survives restarts and its keys never touch disk in the clear. Each
//! store table is one vault space; behaviour mirrors the library's in-memory
//! reference backend.

use super::vault::{Change, Op, Row, Vault};
use crate::socials::types::Error;
use async_trait::async_trait;
use serde::{Serialize, de::DeserializeOwned};
use std::{collections::HashMap, sync::Arc};
use whatsapp_rust::bytes::Bytes;
use whatsapp_rust::wacore::{
    appstate::{hash::HashState, processor::AppStateMutationMAC},
    reporting_token::MESSAGE_SECRET_SIZE,
    store::{
        Device,
        error::{Result, StoreError},
        traits::{
            AppStateSyncKey, AppSyncStore, DeviceListRecord, DeviceStore, LidPnMappingEntry,
            MessageSecret, MsgSecretEntry, MsgSecretStore, PendingInboundKey, PendingInboundLease,
            PendingInboundMutation, PendingInboundPayload, PendingInboundRecord,
            PendingInboundRevision, PendingInboundRow, ProtocolStore, SignalStore,
            StoredMessageSecret, TcTokenEntry, merge_msg_secret_expiry,
            merge_msg_secret_message_ts, pending_group_sender,
        },
    },
};

const IDENTITY: &str = "identity";
const SESSION: &str = "session";
const PREKEY: &str = "prekey";
const SIGNED_PREKEY: &str = "signed_prekey";
const SENDER_KEY: &str = "sender_key";
const SYNC_KEY: &str = "sync_key";
const APP_VERSION: &str = "app_version";
const MUTATION_MAC: &str = "mutation_mac";
const SK_DEVICE: &str = "sk_device";
const LID: &str = "lid";
const PN: &str = "pn";
const BASE_KEY: &str = "base_key";
const DEVICE_LIST: &str = "device_list";
const GROUP_META: &str = "group_meta";
const TC_TOKEN: &str = "tc_token";
const SENT_MSG: &str = "sent_msg";
const MSG_SECRET: &str = "msg_secret";
const META: &str = "meta";
const LATEST_SYNC_KEY: &str = "latest_sync_key";
const DEVICE: &str = "device";
/// Decrypted messages the client holds until the receiver records them.
const PENDING: &str = "pending";
/// History chunks captured before their receipt, until the receiver
/// records their messages.
const HISTORY: &str = "history";
/// Group senders the client may list for one pending message.
const GROUP_SENDERS: i64 = 401;
/// The longest sender spelling a pending group row may carry.
const SENDER_BYTES: usize = 128;

pub struct Backend {
    vault: Vault,
}

impl Backend {
    pub fn new(vault: Vault) -> Self {
        Self { vault }
    }
}

impl From<Error> for StoreError {
    fn from(error: Error) -> Self {
        match error {
            Error::Crypto => Self::Validation("sealed device state could not be opened".into()),
            Error::Invalid(message) => Self::Validation(message.into()),
            other => Self::Database(Box::new(other)),
        }
    }
}

fn join(parts: &[&str]) -> String {
    parts.join("\0")
}

fn id(id: u32) -> String {
    format!("{id:010}")
}

fn parse_id(key: &str) -> Result<u32> {
    key.parse()
        .map_err(|_| StoreError::Validation("stored key id is not a number".into()))
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|e| StoreError::Serialization(Box::new(e)))
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|e| StoreError::Serialization(Box::new(e)))
}

/// `address` belongs to `user`: the user part followed by a device or server.
fn of_user(address: &str, user: &str) -> bool {
    address
        .strip_prefix(user)
        .is_some_and(|rest| rest.starts_with('@') || rest.starts_with(':'))
}

fn now() -> i64 {
    whatsapp_rust::wacore::time::now_secs()
}

/// A pending message's key: chat and id first, so a group's sender aliases
/// for one message share a prefix.
fn pending_key(chat: &str, sender: &str, id: &str) -> String {
    join(&[chat, id, sender])
}

fn revision(value: Option<&[u8]>) -> PendingInboundRevision {
    value.map_or(
        PendingInboundRevision::missing(),
        PendingInboundRevision::for_bytes,
    )
}

impl Backend {
    /// Keep a compressed history chunk until [`Self::history`] hands it to
    /// the receiver. Capturing the same `id` again replaces it.
    pub async fn capture_history(
        &self,
        id: &str,
        compressed: &[u8],
    ) -> std::result::Result<(), Error> {
        self.vault
            .put_rows(
                HISTORY,
                &[Row {
                    key: id,
                    value: compressed,
                    stamp: now(),
                    flag: false,
                }],
            )
            .await
    }

    /// Captured history chunks, oldest key first.
    pub async fn history(&self) -> std::result::Result<Vec<(String, Vec<u8>)>, Error> {
        self.vault.scan(HISTORY, "").await
    }

    pub async fn forget_history(&self, id: &str) -> std::result::Result<(), Error> {
        self.vault.delete(HISTORY, &[id.to_owned()]).await
    }

    async fn get_json<T: DeserializeOwned>(&self, space: &str, key: &str) -> Result<Option<T>> {
        self.vault
            .get(space, key)
            .await?
            .map(|bytes| decode(&bytes))
            .transpose()
    }

    async fn put_json<T: Serialize>(&self, space: &str, key: &str, value: &T) -> Result<()> {
        Ok(self.vault.put(space, key, &encode(value)?).await?)
    }

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
        tx.write(&[Op::Put(
            TC_TOKEN,
            &Row {
                key: jid,
                value: &value,
                stamp: 0,
                flag: false,
            },
        )])
        .await?;
        Ok(tx.commit().await?)
    }

    async fn delete_one(&self, space: &str, key: &str) -> Result<()> {
        Ok(self.vault.write(&[Op::Delete(space, key)]).await?)
    }

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
            .map(|(address, session)| Row {
                key: address,
                value: session,
                stamp: 0,
                flag: false,
            })
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
            .put_rows(
                PREKEY,
                &[Row {
                    key: &key,
                    value: record,
                    stamp: 0,
                    flag: uploaded,
                }],
            )
            .await?)
    }

    async fn store_prekeys_batch(&self, keys: &[(u32, Bytes)], uploaded: bool) -> Result<()> {
        let ids: Vec<String> = keys.iter().map(|(n, _)| id(*n)).collect();
        let rows: Vec<Row<'_>> = keys
            .iter()
            .zip(&ids)
            .map(|((_, record), key)| Row {
                key,
                value: record,
                stamp: 0,
                flag: uploaded,
            })
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
            .map(|(address, record)| Row {
                key: address,
                value: record,
                stamp: 0,
                flag: false,
            })
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

#[async_trait]
impl AppSyncStore for Backend {
    async fn get_sync_key(&self, key_id: &[u8]) -> Result<Option<AppStateSyncKey>> {
        self.get_json(SYNC_KEY, &hex::encode(key_id)).await
    }

    async fn set_sync_key(&self, key_id: &[u8], key: AppStateSyncKey) -> Result<()> {
        let name = hex::encode(key_id);
        let value = encode(&key)?;
        let key_row = Row {
            key: &name,
            value: &value,
            stamp: 0,
            flag: false,
        };
        let latest = Row {
            key: LATEST_SYNC_KEY,
            value: key_id,
            stamp: 0,
            flag: false,
        };
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
        let rows: Vec<Row<'_>> = added
            .iter()
            .map(|(key, mac)| Row {
                key,
                value: mac,
                stamp: 0,
                flag: false,
            })
            .collect();
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
        let version_row = Row {
            key: name,
            value: &version,
            stamp: 0,
            flag: false,
        };
        let (removed, added) = Self::mutation_mac_ops(name, removed_index_macs, added);
        let added: Vec<Row<'_>> = added
            .iter()
            .map(|(key, mac)| Row {
                key,
                value: mac,
                stamp: 0,
                flag: false,
            })
            .collect();
        let mut ops = vec![Op::Put(APP_VERSION, &version_row)];
        ops.extend(removed.iter().map(|key| Op::Delete(MUTATION_MAC, key)));
        ops.extend(added.iter().map(|row| Op::Put(MUTATION_MAC, row)));
        Ok(self.vault.write(&ops).await?)
    }

    async fn clear_mutation_macs(&self, name: &str) -> Result<()> {
        let prefix = format!("{name}\0");
        Ok(self
            .vault
            .write(&[Op::DeletePrefix(MUTATION_MAC, &prefix)])
            .await?)
    }

    async fn get_latest_sync_key_id(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.vault.get(META, LATEST_SYNC_KEY).await?)
    }
}

#[async_trait]
impl ProtocolStore for Backend {
    async fn get_sender_key_devices(&self, group_jid: &str) -> Result<Vec<(String, bool)>> {
        let prefix = format!("{group_jid}\0");
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
            .map(|((_, has_key), key)| Row {
                key,
                value: if *has_key { &[1] } else { &[0] },
                stamp: 0,
                flag: false,
            })
            .collect();
        Ok(self.vault.put_rows(SK_DEVICE, &rows).await?)
    }

    async fn clear_sender_key_devices(&self, group_jid: &str) -> Result<()> {
        let prefix = format!("{group_jid}\0");
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
        let lid_row = Row {
            key: &entry.lid,
            value: &value,
            stamp: 0,
            flag: false,
        };
        let pn_row = Row {
            key: &entry.phone_number,
            value: entry.lid.as_bytes(),
            stamp: 0,
            flag: false,
        };
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
            .put_rows(
                BASE_KEY,
                &[Row {
                    key: &key,
                    value: base_key,
                    stamp: now(),
                    flag: false,
                }],
            )
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
            .put_rows(
                SENT_MSG,
                &[Row {
                    key: &key,
                    value: payload,
                    stamp: now(),
                    flag: false,
                }],
            )
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
        self.store_pending_inbound_batch(&[PendingInboundRow {
            chat,
            sender,
            id,
            message,
        }])
        .await
    }

    async fn store_pending_inbound_batch(&self, rows: &[PendingInboundRow<'_>]) -> Result<()> {
        let keys: Vec<String> = rows
            .iter()
            .map(|r| pending_key(r.chat, r.sender, r.id))
            .collect();
        let stamp = now();
        let rows: Vec<Row<'_>> = rows
            .iter()
            .zip(&keys)
            .map(|(r, key)| Row {
                key,
                value: r.message,
                stamp,
                flag: false,
            })
            .collect();
        Ok(self.vault.put_rows(PENDING, &rows).await?)
    }

    async fn get_pending_inbound(
        &self,
        chat: &str,
        sender: &str,
        id: &str,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .vault
            .get(PENDING, &pending_key(chat, sender, id))
            .await?)
    }

    async fn delete_pending_inbound(&self, chat: &str, sender: &str, id: &str) -> Result<()> {
        self.delete_one(PENDING, &pending_key(chat, sender, id))
            .await
    }

    async fn delete_pending_inbound_batch(&self, keys: &[PendingInboundKey<'_>]) -> Result<()> {
        let keys: Vec<String> = keys
            .iter()
            .map(|k| pending_key(k.chat, k.sender, k.id))
            .collect();
        Ok(self.vault.delete(PENDING, &keys).await?)
    }

    async fn delete_expired_pending_inbound(&self, cutoff_timestamp: i64) -> Result<u32> {
        Ok(self.vault.expire(PENDING, cutoff_timestamp).await?)
    }

    async fn read_pending_inbound_bounded(
        &self,
        key: PendingInboundKey<'_>,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<Option<PendingInboundRecord>> {
        let value = self
            .vault
            .get_bounded(
                PENDING,
                &pending_key(key.chat, key.sender, key.id),
                max_bytes,
            )
            .await?;
        Ok(value.map(|bytes| PendingInboundRecord::new(bytes, lease)))
    }

    async fn read_pending_inbound_group_bounded(
        &self,
        key: PendingInboundKey<'_>,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<Option<(String, PendingInboundRecord)>> {
        let prefix = format!("{}\0", join(&[key.chat, key.id]));
        let mut refused = None;
        let picked = self
            .vault
            .pick_bounded(
                PENDING,
                &prefix,
                GROUP_SENDERS,
                SENDER_BYTES,
                max_bytes,
                |senders| {
                    let mut spellings = senders.iter().map(|s| Some(*s));
                    pending_group_sender(&mut spellings, key.sender).map_err(|error| {
                        refused = Some(error);
                        Error::Invalid("pending group sender")
                    })
                },
            )
            .await;
        match (picked, refused) {
            (Err(_), Some(error)) => Err(error),
            (picked, _) => Ok(
                picked?.map(|(sender, bytes)| (sender, PendingInboundRecord::new(bytes, lease)))
            ),
        }
    }

    async fn replace_pending_inbound_if(
        &self,
        key: PendingInboundKey<'_>,
        expected: PendingInboundRevision,
        replacement: PendingInboundPayload,
        max_bytes: usize,
    ) -> Result<PendingInboundMutation> {
        if replacement.bytes().len() > max_bytes {
            return Err(StoreError::Validation(format!(
                "pending inbound length {} exceeds limit {max_bytes}",
                replacement.bytes().len()
            )));
        }
        let installed = replacement.revision();
        let stamp = now();
        Ok(self
            .vault
            .update(
                PENDING,
                &pending_key(key.chat, key.sender, key.id),
                max_bytes,
                |current| {
                    let current = revision(current);
                    if current == installed {
                        (Change::Keep, PendingInboundMutation::Applied)
                    } else if current != expected {
                        (Change::Keep, PendingInboundMutation::Conflict)
                    } else {
                        let value = replacement.bytes().to_vec();
                        (
                            Change::Put { value, stamp },
                            PendingInboundMutation::Applied,
                        )
                    }
                },
            )
            .await?)
    }

    async fn delete_pending_inbound_if(
        &self,
        key: PendingInboundKey<'_>,
        expected: PendingInboundRevision,
        max_bytes: usize,
        _lease: PendingInboundLease,
    ) -> Result<PendingInboundMutation> {
        Ok(self
            .vault
            .update(
                PENDING,
                &pending_key(key.chat, key.sender, key.id),
                max_bytes,
                |current| {
                    if revision(current) != expected {
                        (Change::Keep, PendingInboundMutation::Conflict)
                    } else if current.is_none() {
                        (Change::Keep, PendingInboundMutation::Applied)
                    } else {
                        (Change::Delete, PendingInboundMutation::Applied)
                    }
                },
            )
            .await?)
    }
}

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
            .map(|(key, (expires_at, value))| Row {
                key,
                value,
                stamp: *expires_at,
                flag: false,
            })
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

#[async_trait]
impl DeviceStore for Backend {
    async fn save(&self, device: &Device) -> Result<()> {
        self.put_json(DEVICE, DEVICE, device).await
    }

    async fn load(&self) -> Result<Option<Device>> {
        let device: Option<Device> = self.get_json(DEVICE, DEVICE).await?;
        Ok(device.map(|mut device| {
            device.device_props =
                Arc::new(whatsapp_rust::wacore::store::device::DEVICE_PROPS.clone());
            device
        }))
    }

    async fn exists(&self) -> Result<bool> {
        Ok(self.vault.exists(DEVICE, DEVICE).await?)
    }

    async fn create(&self) -> Result<i32> {
        let mut tx = self.vault.begin().await?;
        if !tx.exists(DEVICE, DEVICE).await? {
            let value = encode(&Device::new())?;
            tx.write(&[Op::Put(
                DEVICE,
                &Row {
                    key: DEVICE,
                    value: &value,
                    stamp: 0,
                    flag: false,
                },
            )])
            .await?;
        }
        tx.commit().await?;
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socials::{store, types::DeviceKey};
    use sqlx::SqlitePool;
    use uuid::Uuid;

    async fn pool() -> SqlitePool {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::migrate(&db).await.unwrap();
        db
    }

    fn key(byte: u8) -> DeviceKey {
        DeviceKey(zeroize::Zeroizing::new([byte; 32]))
    }

    async fn backend(db: &SqlitePool, device: Uuid, byte: u8) -> Backend {
        Backend::new(Vault::new(db.clone(), device, &key(byte)))
    }

    #[tokio::test]
    async fn a_paired_device_loads_after_a_restart() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let first = backend(&db, device, 7).await;
        assert!(
            !first.exists().await.unwrap(),
            "a fresh store starts unpaired"
        );
        first.create().await.unwrap();
        let mut saved = first.load().await.unwrap().expect("create stores a device");
        assert!(saved.pn.is_none(), "a new device pairs by QR");
        saved.pn = Some("15551234567@s.whatsapp.net".parse().unwrap());
        saved.push_name = "Owner".into();
        first.save(&saved).await.unwrap();
        drop(first);

        let restarted = backend(&db, device, 7).await;
        assert!(restarted.exists().await.unwrap());
        restarted.create().await.unwrap();
        let loaded = restarted.load().await.unwrap().unwrap();
        assert_eq!(loaded.pn, saved.pn, "create keeps a paired device");
        assert_eq!(loaded.push_name, "Owner");
        assert_eq!(loaded.registration_id, saved.registration_id);
        assert_eq!(
            loaded.identity_key.public_key.public_key_bytes(),
            saved.identity_key.public_key.public_key_bytes()
        );
        assert_eq!(
            *loaded.device_props,
            *whatsapp_rust::wacore::store::device::DEVICE_PROPS,
            "runtime device props are restored on load"
        );
    }

    #[tokio::test]
    async fn signal_rows_behave_like_the_reference_store() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let b = backend(&db, device, 1).await;

        b.put_identity("100@s.whatsapp.net", [9; 32]).await.unwrap();
        assert_eq!(
            b.load_identity("100@s.whatsapp.net").await.unwrap(),
            Some([9; 32])
        );
        b.put_session("100:2@s.whatsapp.net", b"session")
            .await
            .unwrap();
        assert!(b.has_session("100:2@s.whatsapp.net").await.unwrap());
        assert!(b.has_signal_state_for_user("100").await.unwrap());
        assert!(
            !b.has_signal_state_for_user("10").await.unwrap(),
            "a user is a whole user part, not a prefix"
        );
        b.set_sender_key_status(
            "g@g.us",
            &[("100:3@s.whatsapp.net", true), ("200@lid", false)],
        )
        .await
        .unwrap();
        let mut addresses: Vec<String> = b
            .get_signal_addresses_for_user("100")
            .await
            .unwrap()
            .iter()
            .map(|a| a.to_string())
            .collect();
        addresses.sort();
        assert_eq!(
            addresses,
            [
                "100:2@s.whatsapp.net",
                "100:3@s.whatsapp.net",
                "100@s.whatsapp.net"
            ]
        );
        let mut devices = b.get_sender_key_devices("g@g.us").await.unwrap();
        devices.sort();
        assert_eq!(
            devices,
            [
                ("100:3@s.whatsapp.net".into(), true),
                ("200@lid".into(), false)
            ]
        );
        b.delete_sender_key_device_rows(&["200@lid"]).await.unwrap();
        assert_eq!(b.get_sender_key_devices("g@g.us").await.unwrap().len(), 1);
        b.clear_sender_key_devices("g@g.us").await.unwrap();
        assert!(b.get_sender_key_devices("g@g.us").await.unwrap().is_empty());

        assert_eq!(b.get_max_prekey_id().await.unwrap(), 0);
        b.store_prekeys_batch(
            &[
                (9, Bytes::from_static(b"a")),
                (10, Bytes::from_static(b"b")),
            ],
            false,
        )
        .await
        .unwrap();
        b.store_prekey(2, b"c", false).await.unwrap();
        assert_eq!(
            b.get_max_prekey_id().await.unwrap(),
            10,
            "ids order numerically, not as text"
        );
        b.mark_prekeys_uploaded(&[9]).await.unwrap();
        let (_, _, uploaded) = b.vault.get_row(PREKEY, &id(9)).await.unwrap().unwrap();
        assert!(uploaded);
        assert_eq!(b.load_prekey(10).await.unwrap().as_deref(), Some(&b"b"[..]));
        b.remove_prekey(10).await.unwrap();
        assert_eq!(b.get_max_prekey_id().await.unwrap(), 9);

        b.save_base_key("a", "m1", b"k").await.unwrap();
        assert!(b.has_same_base_key("a", "m1", b"k").await.unwrap());
        assert_eq!(b.delete_expired_base_keys(now() - 60).await.unwrap(), 0);
        assert_eq!(b.delete_expired_base_keys(now() + 60).await.unwrap(), 1);

        b.store_sent_message("c", "m", b"payload").await.unwrap();
        assert_eq!(
            b.take_sent_message("c", "m").await.unwrap().as_deref(),
            Some(&b"payload"[..])
        );
        assert_eq!(b.take_sent_message("c", "m").await.unwrap(), None);

        let secret = [4; MESSAGE_SECRET_SIZE];
        let entry = |expires_at, message_ts| MsgSecretEntry {
            chat: Arc::from("c"),
            sender: Arc::from("s"),
            msg_id: Arc::from("m"),
            secret,
            expires_at,
            message_ts,
        };
        b.put_msg_secrets(vec![entry(100, 50)]).await.unwrap();
        b.put_msg_secrets(vec![entry(80, 0)]).await.unwrap();
        let stored = b
            .get_stored_msg_secret("c", "s", "m")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.message_ts,
            Some(50),
            "a zero time never clobbers a known one"
        );
        assert_eq!(
            b.delete_expired_msg_secrets(99).await.unwrap(),
            0,
            "the later deadline wins"
        );
        assert_eq!(b.delete_expired_msg_secrets(100).await.unwrap(), 1);

        let mapping = |phone: &str| LidPnMappingEntry {
            lid: "300".into(),
            phone_number: phone.into(),
            created_at: 1,
            updated_at: 1,
            learning_source: "usync".into(),
        };
        b.put_lid_mapping(&mapping("111")).await.unwrap();
        b.put_lid_mapping(&mapping("222")).await.unwrap();
        assert!(
            b.get_pn_mapping("111").await.unwrap().is_none(),
            "a moved number unmaps"
        );
        assert_eq!(b.get_pn_mapping("222").await.unwrap().unwrap().lid, "300");

        b.commit_patch(
            "regular",
            HashState::default(),
            &[],
            &[AppStateMutationMAC {
                index_mac: vec![1],
                value_mac: vec![2],
            }],
        )
        .await
        .unwrap();
        assert_eq!(
            b.get_mutation_mac("regular", &[1]).await.unwrap(),
            Some(vec![2])
        );
        assert!(b.get_version("regular").await.unwrap().is_some());
        b.clear_mutation_macs("regular").await.unwrap();
        assert_eq!(b.get_mutation_mac("regular", &[1]).await.unwrap(), None);

        b.set_sync_key(&[5, 6], AppStateSyncKey::default())
            .await
            .unwrap();
        assert_eq!(b.get_latest_sync_key_id().await.unwrap(), Some(vec![5, 6]));
    }

    #[tokio::test]
    async fn concurrent_token_merges_keep_every_input() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let b = backend(&db, device, 1).await;
        let touches = (0..16).map(|i| b.touch_tc_token_sender_timestamp("u", 5000 + i));
        let tokens = (0..16).map(|i| {
            let token = vec![u8::try_from(i).unwrap()];
            let b = &b;
            async move { b.store_received_tc_token("u", &token, 4000 + i).await }
        });
        let (touched, stored) = tokio::join!(
            futures_util::future::join_all(touches),
            futures_util::future::join_all(tokens.rev())
        );
        touched.into_iter().chain(stored).for_each(|r| r.unwrap());
        let entry = b.get_tc_token("u").await.unwrap().unwrap();
        assert_eq!(
            entry.sender_timestamp,
            Some(5015),
            "a sender timestamp never regresses"
        );
        assert_eq!(entry.token, vec![15], "the newest token wins");
        assert_eq!(entry.token_timestamp, 4015);

        let secret = [4; MESSAGE_SECRET_SIZE];
        let entry = |expires_at, message_ts| MsgSecretEntry {
            chat: Arc::from("c"),
            sender: Arc::from("s"),
            msg_id: Arc::from("m"),
            secret,
            expires_at,
            message_ts,
        };
        let (first, second) = tokio::join!(
            b.put_msg_secrets(vec![entry(100, 50)]),
            b.put_msg_secrets(vec![entry(200, 0)])
        );
        first.unwrap();
        second.unwrap();
        let stored = b
            .get_stored_msg_secret("c", "s", "m")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.message_ts, Some(50), "the known time survives");
        assert_eq!(
            b.delete_expired_msg_secrets(199).await.unwrap(),
            0,
            "the later deadline survives"
        );
    }

    #[tokio::test]
    async fn another_key_cannot_open_the_store() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let owner = backend(&db, device, 1).await;
        owner
            .put_session("100@s.whatsapp.net", b"session")
            .await
            .unwrap();
        let stranger = backend(&db, device, 2).await;
        assert!(
            stranger.get_session("100@s.whatsapp.net").await.is_err(),
            "a wrong key fails instead of returning garbage"
        );
    }

    #[tokio::test]
    async fn stored_values_are_sealed_at_rest() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let b = backend(&db, device, 3).await;
        let marker = b"plaintext-marker-5f1c";
        b.put_session("100@s.whatsapp.net", marker).await.unwrap();
        b.put_sender_key("g@g.us", marker).await.unwrap();
        let values: Vec<Vec<u8>> = sqlx::query_scalar("SELECT value FROM social_device_state")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(values.len(), 2);
        for value in values {
            assert!(!value.windows(marker.len()).any(|w| w == marker));
        }
    }

    fn lease() -> PendingInboundLease {
        PendingInboundLease::new(Arc::new(()))
    }

    fn payload(bytes: &'static [u8]) -> PendingInboundPayload {
        PendingInboundPayload::new(Bytes::from_static(bytes), lease())
    }

    const KEY: PendingInboundKey<'static> = PendingInboundKey {
        chat: "1@g.us",
        sender: "2:3@s.whatsapp.net",
        id: "M1",
    };

    #[tokio::test]
    async fn a_pending_message_is_kept_until_deleted_and_expires_by_age() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let b = backend(&db, device, 7).await;
        b.store_pending_inbound("1@g.us", "2@s.whatsapp.net", "M1", b"one")
            .await
            .unwrap();
        b.store_pending_inbound("1@g.us", "2@s.whatsapp.net", "M2", b"two")
            .await
            .unwrap();
        assert_eq!(
            b.get_pending_inbound("1@g.us", "2@s.whatsapp.net", "M1")
                .await
                .unwrap()
                .as_deref(),
            Some(&b"one"[..])
        );
        b.delete_pending_inbound("1@g.us", "2@s.whatsapp.net", "M1")
            .await
            .unwrap();
        assert!(
            b.get_pending_inbound("1@g.us", "2@s.whatsapp.net", "M1")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            b.delete_expired_pending_inbound(now() - 60).await.unwrap(),
            0
        );
        assert_eq!(
            b.delete_expired_pending_inbound(now() + 60).await.unwrap(),
            1
        );
        assert!(
            b.get_pending_inbound("1@g.us", "2@s.whatsapp.net", "M2")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_pending_row_changes_only_from_the_revision_its_owner_saw() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let b = backend(&db, device, 7).await;
        let missing = PendingInboundRevision::missing();
        assert_eq!(
            b.delete_pending_inbound_if(KEY, missing, 16, lease())
                .await
                .unwrap(),
            PendingInboundMutation::Applied,
            "deleting an absent row that was expected absent is a no-op"
        );
        assert_eq!(
            b.replace_pending_inbound_if(KEY, missing, payload(b"first"), 16)
                .await
                .unwrap(),
            PendingInboundMutation::Applied
        );
        assert_eq!(
            b.replace_pending_inbound_if(KEY, missing, payload(b"first"), 16)
                .await
                .unwrap(),
            PendingInboundMutation::Applied,
            "installing the same bytes again is idempotent"
        );
        assert_eq!(
            b.replace_pending_inbound_if(KEY, missing, payload(b"other"), 16)
                .await
                .unwrap(),
            PendingInboundMutation::Conflict,
            "a stale expected revision is refused"
        );
        let stored = b
            .read_pending_inbound_bounded(KEY, 16, lease())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.bytes(), b"first");
        assert!(
            b.read_pending_inbound_bounded(KEY, 2, lease())
                .await
                .is_err(),
            "a row over the bound is an error, not absence"
        );
        assert_eq!(
            b.delete_pending_inbound_if(KEY, missing, 16, lease())
                .await
                .unwrap(),
            PendingInboundMutation::Conflict,
            "a present row is not deleted as if absent"
        );
        assert_eq!(
            b.delete_pending_inbound_if(KEY, stored.revision(), 16, lease())
                .await
                .unwrap(),
            PendingInboundMutation::Applied
        );
        assert!(
            b.read_pending_inbound_bounded(KEY, 16, lease())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_group_message_is_found_by_its_senders_device_alias() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let b = backend(&db, device, 7).await;
        b.replace_pending_inbound_if(KEY, PendingInboundRevision::missing(), payload(b"hi"), 16)
            .await
            .unwrap();
        let alias = PendingInboundKey {
            sender: "2@s.whatsapp.net",
            ..KEY
        };
        let (sender, record) = b
            .read_pending_inbound_group_bounded(alias, 16, lease())
            .await
            .unwrap()
            .expect("the device spelling matches its user");
        assert_eq!(sender, KEY.sender);
        assert_eq!(record.bytes(), b"hi");
        let other = PendingInboundKey {
            sender: "9@s.whatsapp.net",
            ..KEY
        };
        assert!(
            b.read_pending_inbound_group_bounded(other, 16, lease())
                .await
                .unwrap()
                .is_none()
        );
        b.replace_pending_inbound_if(
            PendingInboundKey {
                sender: "2:4@s.whatsapp.net",
                ..KEY
            },
            PendingInboundRevision::missing(),
            payload(b"again"),
            16,
        )
        .await
        .unwrap();
        assert!(
            b.read_pending_inbound_group_bounded(alias, 16, lease())
                .await
                .is_err(),
            "two device spellings of one sender are ambiguous"
        );
    }

    #[tokio::test]
    async fn a_captured_history_chunk_stays_until_forgotten() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let b = backend(&db, device, 7).await;
        b.capture_history("H1", b"zlib").await.unwrap();
        b.capture_history("H1", b"zlib").await.unwrap();
        assert_eq!(
            b.history().await.unwrap(),
            vec![("H1".into(), b"zlib".to_vec())]
        );
        b.forget_history("H1").await.unwrap();
        assert!(b.history().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_group_sender_longer_than_its_bound_is_refused_unread() {
        let db = pool().await;
        let device = store::new_device(&db).await.unwrap();
        let b = backend(&db, device, 7).await;
        let long = format!("{}@s.whatsapp.net", "2".repeat(SENDER_BYTES));
        b.replace_pending_inbound_if(
            PendingInboundKey {
                sender: &long,
                ..KEY
            },
            PendingInboundRevision::missing(),
            payload(b"hi"),
            16,
        )
        .await
        .unwrap();
        assert!(
            b.read_pending_inbound_group_bounded(KEY, 16, lease())
                .await
                .is_err(),
            "an oversized sender is an error, never absence"
        );
    }
}
