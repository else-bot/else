use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    #[serde(skip)]
    path: Option<PathBuf>,
    pub sidebar_width: f32,
    pub details_width: f32,
    pub category_order: Vec<Uuid>,
    pub collapsed: HashSet<Option<Uuid>>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            path: None,
            sidebar_width: 240.,
            details_width: 336.,
            category_order: Vec::new(),
            collapsed: HashSet::new(),
        }
    }
}

impl Preferences {
    pub fn load(path: PathBuf) -> Self {
        let mut saved: Self = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        saved.path = Some(path);
        saved.sidebar_width = saved.sidebar_width.clamp(180., 420.);
        saved.details_width = saved.details_width.clamp(280., 560.);
        saved
    }

    pub fn load_for_server(store: &crate::config::ProfileStore, id: Uuid, legacy: bool) -> Self {
        let path = store.server_preferences_path(id);
        let missing = matches!(
            std::fs::symlink_metadata(&path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound
        );
        if legacy && missing {
            let mut saved = Self::load(store.preferences_path());
            saved.path = Some(path);
            let _ = saved.save();
            saved
        } else {
            Self::load(path)
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = self
            .path
            .as_ref()
            .ok_or("The layout has no profile storage path.")?;
        let staged = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
        let bytes =
            serde_json::to_vec(self).map_err(|err| format!("Could not serialize layout: {err}"))?;
        let result = std::fs::write(&staged, bytes)
            .and_then(|_| std::fs::rename(&staged, path))
            .map_err(|err| format!("Could not save layout: {err}"));
        if result.is_err() {
            let _ = std::fs::remove_file(staged);
        }
        result
    }

    pub fn reorder(&mut self, ids: Vec<Uuid>, dragged: Uuid, target: Uuid) {
        let (Some(from), Some(to)) = (
            ids.iter().position(|id| *id == dragged),
            ids.iter().position(|id| *id == target),
        ) else {
            return;
        };
        self.category_order = ids;
        self.category_order.remove(from);
        self.category_order.insert(to, dragged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_stay_beside_their_resolved_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let first =
            crate::config::ProfileStore::at(dir.path().join("first.json")).preferences_path();
        let second =
            crate::config::ProfileStore::at(dir.path().join("second.json")).preferences_path();
        let mut a = Preferences::load(first.clone());
        a.sidebar_width = 310.;
        a.save().unwrap();
        let mut b = Preferences::load(second.clone());
        assert_eq!(b.sidebar_width, 240.);
        b.sidebar_width = 390.;
        b.save().unwrap();
        assert_eq!(Preferences::load(first).sidebar_width, 310.);
        assert_eq!(Preferences::load(second).sidebar_width, 390.);
    }

    #[test]
    fn legacy_layout_is_copied_only_to_its_original_server() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::config::ProfileStore::at(dir.path().join("desktop.json"));
        let mut legacy = Preferences::load(store.preferences_path());
        let category = Uuid::new_v4();
        legacy.sidebar_width = 310.;
        legacy.category_order = vec![category];
        legacy.collapsed.insert(Some(category));
        legacy.save().unwrap();
        let original = std::fs::read(store.preferences_path()).unwrap();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let migrated = Preferences::load_for_server(&store, first, true);
        assert_eq!(migrated.sidebar_width, 310.);
        assert_eq!(migrated.category_order, [category]);
        assert!(migrated.collapsed.contains(&Some(category)));
        assert_eq!(std::fs::read(store.preferences_path()).unwrap(), original);
        legacy.category_order.clear();
        legacy.save().unwrap();
        assert_eq!(
            Preferences::load_for_server(&store, first, true).category_order,
            [category]
        );
        let mut unrelated = Preferences::load_for_server(&store, second, false);
        assert!(unrelated.category_order.is_empty());
        assert!(unrelated.collapsed.is_empty());
        unrelated.category_order = vec![Uuid::new_v4()];
        unrelated.save().unwrap();
        assert_eq!(
            Preferences::load_for_server(&store, first, true).category_order,
            [category]
        );
        assert_eq!(
            Preferences::load_for_server(&store, second, false).category_order,
            unrelated.category_order
        );
    }

    #[test]
    fn category_moves_reach_both_ends_and_preferences_round_trip() {
        let ids: Vec<_> = (1..=3).map(Uuid::from_u128).collect();
        let mut prefs = Preferences::default();
        prefs.reorder(ids.clone(), ids[0], ids[2]);
        assert_eq!(prefs.category_order, vec![ids[1], ids[2], ids[0]]);
        prefs.reorder(prefs.category_order.clone(), ids[0], ids[1]);
        assert_eq!(prefs.category_order, ids);
        prefs.collapsed.extend([None, Some(ids[1])]);
        prefs.sidebar_width = 315.;
        prefs.details_width = 410.;
        let loaded: Preferences =
            serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert_eq!(loaded.category_order, prefs.category_order);
        assert_eq!(loaded.collapsed, prefs.collapsed);
        assert_eq!((loaded.sidebar_width, loaded.details_width), (315., 410.));
    }
}
