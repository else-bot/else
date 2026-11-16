//! The WhatsApp client's key store, kept in sealed [`Vault`] rows so a linked
//! device survives restarts and its keys never touch disk in the clear. Each
//! store table is one vault space; behaviour mirrors the library's in-memory
//! reference backend.

mod app_sync;
mod device;
mod msg_secrets;
mod pending;
mod protocol;
mod signal;
#[cfg(test)]
mod tests;

use super::vault::{Op, Vault};
use crate::socials::types::Error;
use serde::{Serialize, de::DeserializeOwned};
use whatsapp_rust::wacore::store::error::{Result, StoreError};

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

/// The key prefix every key built by [`join`] from `parts` and one more
/// part starts with.
fn prefix(parts: &[&str]) -> String {
    let mut prefix = join(parts);
    prefix.push('\0');
    prefix
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

impl Backend {
    /// Keep a compressed history chunk until [`Self::history`] hands it to
    /// the receiver. Capturing the same `id` again replaces it.
    pub async fn capture_history(
        &self,
        id: &str,
        compressed: &[u8],
    ) -> std::result::Result<(), Error> {
        self.vault.put_stamped(HISTORY, id, compressed, now()).await
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

    async fn delete_one(&self, space: &str, key: &str) -> Result<()> {
        Ok(self.vault.write(&[Op::Delete(space, key)]).await?)
    }
}
