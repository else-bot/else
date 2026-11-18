//! Recently read messages, so the owner's reader opens an email a bot read
//! earlier that day without signing in to the mail server again. It lives
//! in memory only. A message's bytes never change under one UIDVALIDITY and
//! UID, but it can leave its folder: moving or deleting it through the
//! account drops its entry, and a change made elsewhere shows once the
//! entry ages out.

use super::types::{Fetched, MessageRef};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use tokio::time::Instant;
use uuid::Uuid;

/// Bytes all cached messages may take together.
const BUDGET: usize = 64 * 1024 * 1024;
/// The largest message kept.
const ENTRY_LIMIT: usize = 16 * 1024 * 1024;
/// Messages kept at most, whatever their size.
const ENTRIES: usize = 256;
/// How long a message stays after it was read from the server.
const TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// One message of one account generation. A new generation means the
/// account's settings changed, so its older entries are never served.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub account_id: Uuid,
    pub generation: i64,
    pub message: MessageRef,
}

struct Entry {
    fetched: Fetched,
    stored: Instant,
    serial: u64,
}

/// Evicts the least recently used entry first.
#[derive(Default)]
pub(super) struct Messages {
    entries: HashMap<Key, Entry>,
    order: VecDeque<(u64, Key)>,
    bytes: usize,
    serial: u64,
}

impl Messages {
    pub fn get(&mut self, key: &Key) -> Option<Fetched> {
        let entry = self.entries.get_mut(key)?;
        if entry.stored.elapsed() >= TTL {
            self.remove(key);
            return None;
        }
        self.serial += 1;
        entry.serial = self.serial;
        let fetched = entry.fetched.clone();
        self.order.push_back((self.serial, key.clone()));
        self.compact();
        Some(fetched)
    }

    /// Drops order slots left by entries that were read again or removed.
    fn compact(&mut self) {
        if self.order.len() > 2 * self.entries.len() + 16 {
            let entries = &self.entries;
            self.order
                .retain(|(serial, key)| entries.get(key).is_some_and(|e| e.serial == *serial));
        }
    }

    pub fn put(&mut self, key: Key, fetched: Fetched) {
        if fetched.raw.len() > ENTRY_LIMIT {
            return;
        }
        self.remove(&key);
        while self.bytes + fetched.raw.len() > BUDGET || self.entries.len() >= ENTRIES {
            let Some((serial, oldest)) = self.order.pop_front() else {
                break;
            };
            if self
                .entries
                .get(&oldest)
                .is_some_and(|e| e.serial == serial)
            {
                self.remove(&oldest);
            }
        }
        self.compact();
        self.serial += 1;
        self.bytes += fetched.raw.len();
        self.order.push_back((self.serial, key.clone()));
        self.entries.insert(
            key,
            Entry {
                fetched,
                stored: Instant::now(),
                serial: self.serial,
            },
        );
    }

    /// Drops every message of a removed account.
    pub fn forget(&mut self, account_id: Uuid) {
        self.entries.retain(|key, entry| {
            let keep = key.account_id != account_id;
            if !keep {
                self.bytes -= entry.fetched.raw.len();
            }
            keep
        });
        self.order.retain(|(_, key)| key.account_id != account_id);
    }

    pub fn remove(&mut self, key: &Key) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes -= entry.fetched.raw.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(account_id: Uuid, uid: u32) -> Key {
        Key {
            account_id,
            generation: 1,
            message: MessageRef {
                folder: "INBOX".into(),
                uid_validity: 7,
                uid,
            },
        }
    }
    fn fetched(len: usize) -> Fetched {
        Fetched {
            raw: vec![b'x'; len].into(),
            received_at: None,
        }
    }

    #[test]
    fn the_least_recently_read_message_leaves_first_when_the_budget_is_full() {
        let account = Uuid::new_v4();
        let mut cache = Messages::default();
        for uid in 1..=4 {
            cache.put(key(account, uid), fetched(15 * 1024 * 1024));
        }
        assert!(cache.get(&key(account, 1)).is_some());
        cache.put(key(account, 5), fetched(15 * 1024 * 1024));
        assert!(cache.get(&key(account, 2)).is_none(), "least recently used");
        assert!(
            [1, 3, 4, 5]
                .iter()
                .all(|&uid| cache.get(&key(account, uid)).is_some())
        );
        cache.put(key(account, 6), fetched(17 * 1024 * 1024));
        assert!(cache.get(&key(account, 6)).is_none(), "too large to keep");
    }

    #[tokio::test(start_paused = true)]
    async fn a_message_ages_out_and_a_forgotten_account_leaves() {
        let (kept, gone) = (Uuid::new_v4(), Uuid::new_v4());
        let mut cache = Messages::default();
        cache.put(key(kept, 1), fetched(10));
        cache.put(key(gone, 1), fetched(10));
        cache.forget(gone);
        assert!(cache.get(&key(gone, 1)).is_none());
        assert!(cache.get(&key(kept, 1)).is_some());
        let other_generation = Key {
            generation: 2,
            ..key(kept, 1)
        };
        assert!(cache.get(&other_generation).is_none());
        tokio::time::advance(Duration::from_secs(23 * 60 * 60)).await;
        assert!(cache.get(&key(kept, 1)).is_some(), "kept for most of a day");
        tokio::time::advance(Duration::from_secs(60 * 60)).await;
        assert!(cache.get(&key(kept, 1)).is_none());
    }
}
