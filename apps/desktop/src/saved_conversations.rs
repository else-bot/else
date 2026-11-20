//! The latest conversation page of each bot, saved per server as the API
//! sent it, so a bot not in memory draws at once while live data loads.

use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::model::Snapshot;

/// A page larger than this is not saved.
const MAX_PAGE_BYTES: u64 = 1024 * 1024;
/// How many bots' pages a server keeps; the least recently saved go first.
const KEPT_PAGES: usize = 32;

#[derive(Debug, Clone)]
pub struct SavedConversations {
    dir: PathBuf,
}

impl SavedConversations {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
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

    /// Saves `raw`, the API's response for `bot`'s latest page, readable
    /// only by the current user. A page over the size limit removes the
    /// older one instead, so a stale page never stands in for it.
    pub fn save(&self, bot: Uuid, raw: &str) -> Result<(), String> {
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

    /// Removes every page saved for a server.
    pub fn forget(dir: &Path) -> Result<(), String> {
        match std::fs::remove_dir_all(dir) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("Cannot remove {}: {err}", dir.display()))
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
        let saved = SavedConversations::new(dir.path().join("server.conversations"));
        let bot = Uuid::from_u128(1);
        assert!(saved.load(bot).is_none());
        saved.save(bot, &page("hello")).unwrap();
        let snapshot = saved.load(bot).expect("saved page loads");
        assert_eq!(snapshot.items[0].text, "hello");
        assert_eq!(mode(&saved.dir), 0o700);
        assert_eq!(mode(&saved.page(bot)), 0o600);
    }

    #[test]
    fn a_page_shared_with_other_users_is_not_shown() {
        let dir = tempfile::tempdir().unwrap();
        let saved = SavedConversations::new(dir.path().to_owned());
        let bot = Uuid::from_u128(1);
        saved.save(bot, &page("hello")).unwrap();
        std::fs::set_permissions(saved.page(bot), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(saved.load(bot).is_none());
    }

    #[test]
    fn an_oversized_page_replaces_the_saved_one_with_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let saved = SavedConversations::new(dir.path().to_owned());
        let bot = Uuid::from_u128(1);
        saved.save(bot, &page("hello")).unwrap();
        saved
            .save(bot, &page(&"x".repeat(MAX_PAGE_BYTES as usize)))
            .unwrap();
        assert!(saved.load(bot).is_none());
    }

    #[test]
    fn a_server_keeps_only_its_most_recently_saved_pages() {
        let dir = tempfile::tempdir().unwrap();
        let saved = SavedConversations::new(dir.path().to_owned());
        let oldest = Uuid::from_u128(1000);
        saved.save(oldest, &page("first")).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(saved.page(oldest))
            .unwrap()
            .set_modified(past)
            .unwrap();
        for n in 0..KEPT_PAGES as u128 {
            saved.save(Uuid::from_u128(n), &page("later")).unwrap();
        }
        assert!(saved.load(oldest).is_none(), "the oldest page is pruned");
        assert!(saved.load(Uuid::from_u128(0)).is_some());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 32);
    }

    #[test]
    fn forgetting_a_server_removes_its_pages() {
        let dir = tempfile::tempdir().unwrap();
        let pages = dir.path().join("server.conversations");
        let saved = SavedConversations::new(pages.clone());
        saved.save(Uuid::from_u128(1), &page("hello")).unwrap();
        SavedConversations::forget(&pages).unwrap();
        assert!(!pages.exists());
        SavedConversations::forget(&pages).unwrap();
    }
}
