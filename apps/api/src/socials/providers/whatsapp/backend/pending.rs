use super::{Backend, PENDING, join, now, prefix};
use crate::socials::providers::whatsapp::vault::{Change, Op, Row, Vault};
use crate::socials::types::Error;
use whatsapp_rust::wacore::store::{
    error::{Result, StoreError},
    traits::{
        PendingInboundKey, PendingInboundLease, PendingInboundMutation, PendingInboundPayload,
        PendingInboundRecord, PendingInboundRevision, PendingInboundRow, pending_group_sender,
    },
};

/// Group senders the client may list for one pending message.
const GROUP_SENDERS: i64 = 401;
/// The longest sender spelling a pending group row may carry.
pub(super) const SENDER_BYTES: usize = 128;

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

/// The pending-inbound rows, which [`ProtocolStore`] forwards to.
pub(super) struct Pending<'a> {
    vault: &'a Vault,
}

impl Backend {
    pub(super) fn pending(&self) -> Pending<'_> {
        Pending { vault: &self.vault }
    }
}

impl Pending<'_> {
    pub(super) async fn store_pending_inbound(
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

    pub(super) async fn store_pending_inbound_batch(
        &self,
        rows: &[PendingInboundRow<'_>],
    ) -> Result<()> {
        let keys: Vec<String> = rows
            .iter()
            .map(|r| pending_key(r.chat, r.sender, r.id))
            .collect();
        let stamp = now();
        let rows: Vec<Row<'_>> = rows
            .iter()
            .zip(&keys)
            .map(|(r, key)| Row::new(key, r.message).stamped(stamp))
            .collect();
        Ok(self.vault.put_rows(PENDING, &rows).await?)
    }

    pub(super) async fn get_pending_inbound(
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

    pub(super) async fn delete_pending_inbound(
        &self,
        chat: &str,
        sender: &str,
        id: &str,
    ) -> Result<()> {
        let key = pending_key(chat, sender, id);
        Ok(self.vault.write(&[Op::Delete(PENDING, &key)]).await?)
    }

    pub(super) async fn delete_pending_inbound_batch(
        &self,
        keys: &[PendingInboundKey<'_>],
    ) -> Result<()> {
        let keys: Vec<String> = keys
            .iter()
            .map(|k| pending_key(k.chat, k.sender, k.id))
            .collect();
        Ok(self.vault.delete(PENDING, &keys).await?)
    }

    pub(super) async fn delete_expired_pending_inbound(
        &self,
        cutoff_timestamp: i64,
    ) -> Result<u32> {
        Ok(self.vault.expire(PENDING, cutoff_timestamp).await?)
    }

    pub(super) async fn read_pending_inbound_bounded(
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

    pub(super) async fn read_pending_inbound_group_bounded(
        &self,
        key: PendingInboundKey<'_>,
        max_bytes: usize,
        lease: PendingInboundLease,
    ) -> Result<Option<(String, PendingInboundRecord)>> {
        let prefix = prefix(&[key.chat, key.id]);
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

    pub(super) async fn replace_pending_inbound_if(
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

    pub(super) async fn delete_pending_inbound_if(
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
