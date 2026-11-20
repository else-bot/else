//! The latest conversation page of each bot, saved per server as the API
//! sent it, so a bot not in memory draws at once while live data loads.

use std::collections::HashMap;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use uuid::Uuid;

use crate::model::Snapshot;

/// A page larger than this is not saved.
const MAX_PAGE_BYTES: u64 = 1024 * 1024;
/// How many bots' pages a server keeps; the least recently saved go first.
const KEPT_PAGES: usize = 32;

/// One server connection's pages. Every session of a server shares one
/// writer, so saves land in the order their pages were fetched and none
/// lands once the server is forgotten.
#[derive(Debug, Clone)]
pub struct SavedConversations {
    dir: PathBuf,
    writer: Arc<Mutex<Writer>>,
}

#[derive(Debug, Default)]
struct Writer {
    forgotten: bool,
    next: u64,
    /// The ticket of the page last saved for each bot.
    saved: HashMap<Uuid, u64>,
}

fn writer(server: &Path) -> Arc<Mutex<Writer>> {
    static WRITERS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<Writer>>>>> =
        LazyLock::new(Default::default);
    let mut writers = WRITERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    writers.entry(server.to_owned()).or_default().clone()
}

fn lock(writer: &Mutex<Writer>) -> std::sync::MutexGuard<'_, Writer> {
    writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A fetched page's place in fetch order.
#[derive(Debug, Clone, Copy)]
pub struct Ticket(u64);

impl SavedConversations {
    /// Pages for the server saved under `server`, reached at `origin`; a
    /// connection edited to another address starts with none.
    pub fn new(server: &Path, origin: &str) -> Self {
        let address: String = origin
            .bytes()
            .map(|b| match b {
                b if b.is_ascii_alphanumeric() => char::from(b).to_string(),
                b => format!("_{b:02x}"),
            })
            .collect();
        Self {
            dir: server.join(address),
            writer: writer(server),
        }
    }

    fn page(&self, bot: Uuid) -> PathBuf {
        self.dir.join(format!("{bot}.json"))
    }

    /// The page saved for `bot`, if one is saved, private and readable.
    pub fn load(&self, bot: Uuid) -> Option<Snapshot> {
        let text = crate::config::read_private_limited(&self.page(bot), MAX_PAGE_BYTES)
            .ok()
            .flatten()?;
        serde_json::from_str(&text).ok()
    }

    /// Takes a place in fetch order for a page just fetched.
    pub fn ticket(&self) -> Ticket {
        let mut writer = lock(&self.writer);
        writer.next += 1;
        Ticket(writer.next)
    }

    /// Saves `raw`, the API's response for `bot`'s latest page, readable
    /// only by the current user, unless a page fetched later is already
    /// saved. A page over the size limit removes the older one instead, so
    /// a stale page never stands in for it.
    pub fn save(&self, bot: Uuid, raw: &str, ticket: Ticket) -> Result<(), String> {
        let mut writer = lock(&self.writer);
        if writer.forgotten || writer.saved.get(&bot).is_some_and(|&last| last > ticket.0) {
            return Ok(());
        }
        writer.saved.insert(bot, ticket.0);
        let path = self.page(bot);
        if raw.len() as u64 > MAX_PAGE_BYTES {
            return remove(&path);
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|err| format!("Cannot create {}: {err}", self.dir.display()))?;
        crate::config::replace_private(&path, raw.as_bytes())
            .map_err(|err| format!("Cannot save {}: {err}", path.display()))?;
        self.prune()
    }

    /// Removes the least recently saved pages beyond [`KEPT_PAGES`].
    fn prune(&self) -> Result<(), String> {
        let entries = std::fs::read_dir(&self.dir)
            .map_err(|err| format!("Cannot read {}: {err}", self.dir.display()))?;
        let mut pages: Vec<_> = entries
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
            .collect();
        pages.sort_by_key(|(saved, _)| std::cmp::Reverse(*saved));
        for (_, path) in pages.into_iter().skip(KEPT_PAGES) {
            remove(&path)?;
        }
        Ok(())
    }

    /// Removes every page saved under `server`, at any address, and
    /// refuses later saves from its sessions.
    pub fn forget(server: &Path) -> Result<(), String> {
        let writer = writer(server);
        let mut writer = lock(&writer);
        writer.forgotten = true;
        match std::fs::remove_dir_all(server) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("Cannot remove {}: {err}", server.display()))
            }
            _ => Ok(()),
        }
    }
}

fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("Cannot remove {}: {err}", path.display()))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    const ORIGIN: &str = "http://127.0.0.1:7879";

    fn page(text: &str) -> String {
        serde_json::json!({
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:7",
            "items": [{
                "id": Uuid::from_u128(0xa1), "seq": 1, "turn_id": Uuid::from_u128(0xb1),
                "kind": "assistant", "text": text, "provider_id": "codex"
            }],
            "turns": [],
            "activity": { "phase": "idle", "active_turn_id": null, "queued_count": 0 }
        })
        .to_string()
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_saved_page_loads_back_readable_only_by_its_owner() {
        let dir = tempfile::tempdir().unwrap();
        let saved = SavedConversations::new(&dir.path().join("server.conversations"), ORIGIN);
        let bot = Uuid::from_u128(1);
        assert!(saved.load(bot).is_none());
        saved.save(bot, &page("hello"), saved.ticket()).unwrap();
        let snapshot = saved.load(bot).expect("saved page loads");
        assert_eq!(snapshot.items[0].text, "hello");
        assert_eq!(mode(&saved.dir), 0o700);
        assert_eq!(mode(&saved.page(bot)), 0o600);
    }

    #[test]
    fn a_page_shared_with_other_users_is_not_shown() {
        let dir = tempfile::tempdir().unwrap();
        let saved = SavedConversations::new(dir.path(), ORIGIN);
        let bot = Uuid::from_u128(1);
        saved.save(bot, &page("hello"), saved.ticket()).unwrap();
        std::fs::set_permissions(saved.page(bot), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(saved.load(bot).is_none());
    }

    #[test]
    fn an_oversized_page_replaces_the_saved_one_with_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let saved = SavedConversations::new(dir.path(), ORIGIN);
        let bot = Uuid::from_u128(1);
        saved.save(bot, &page("hello"), saved.ticket()).unwrap();
        saved
            .save(
                bot,
                &page(&"x".repeat(MAX_PAGE_BYTES as usize)),
                saved.ticket(),
            )
            .unwrap();
        assert!(saved.load(bot).is_none());
    }

    #[test]
    fn a_server_keeps_only_its_most_recently_saved_pages() {
        let dir = tempfile::tempdir().unwrap();
        let saved = SavedConversations::new(dir.path(), ORIGIN);
        let oldest = Uuid::from_u128(1000);
        saved.save(oldest, &page("first"), saved.ticket()).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(saved.page(oldest))
            .unwrap()
            .set_modified(past)
            .unwrap();
        for n in 0..KEPT_PAGES as u128 {
            saved
                .save(Uuid::from_u128(n), &page("later"), saved.ticket())
                .unwrap();
        }
        assert!(saved.load(oldest).is_none(), "the oldest page is pruned");
        assert!(saved.load(Uuid::from_u128(0)).is_some());
        assert_eq!(std::fs::read_dir(&saved.dir).unwrap().count(), 32);
    }

    #[test]
    fn a_page_fetched_earlier_does_not_replace_a_later_one() {
        let dir = tempfile::tempdir().unwrap();
        let saved = SavedConversations::new(dir.path(), ORIGIN);
        let bot = Uuid::from_u128(1);
        let (earlier, later) = (saved.ticket(), saved.ticket());
        saved.save(bot, &page("later"), later).unwrap();
        saved.save(bot, &page("earlier"), earlier).unwrap();
        assert_eq!(saved.load(bot).unwrap().items[0].text, "later");
    }

    #[test]
    fn forgetting_a_server_removes_its_pages_and_refuses_later_saves() {
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("server.conversations");
        let saved = SavedConversations::new(&server, ORIGIN);
        let elsewhere = SavedConversations::new(&server, "https://other.example");
        let ticket = saved.ticket();
        saved
            .save(Uuid::from_u128(1), &page("hello"), saved.ticket())
            .unwrap();
        elsewhere
            .save(Uuid::from_u128(2), &page("hello"), elsewhere.ticket())
            .unwrap();
        SavedConversations::forget(&server).unwrap();
        assert!(!server.exists(), "every address's pages are gone");
        saved
            .save(Uuid::from_u128(3), &page("late"), ticket)
            .unwrap();
        assert!(
            !server.exists(),
            "a save queued before removal lands nowhere"
        );
    }

    #[test]
    fn another_address_for_the_server_starts_without_pages() {
        let dir = tempfile::tempdir().unwrap();
        let bot = Uuid::from_u128(1);
        let saved = SavedConversations::new(dir.path(), ORIGIN);
        saved.save(bot, &page("hello"), saved.ticket()).unwrap();
        let moved = SavedConversations::new(dir.path(), "https://other.example");
        assert!(moved.load(bot).is_none());
        let lookalike = SavedConversations::new(dir.path(), "http://127.0.0.1_7879");
        assert!(
            lookalike.load(bot).is_none(),
            "distinct origins never share pages"
        );
    }
}
