//! Sidebar layout (POC-031): the main bot pinned on top, then categories
//! with their bots, then the unassigned bots. Pure, so the order is tested
//! without a window.
//!
//! Categories are ordered as the server orders them: by the case-folded
//! name's code points, then ID. Bots within a group are ordered by recency:
//! the newest message first, a never-messaged bot by its creation time, then
//! the folded name and ID. A bot with no category, or with a category the
//! client does not know (older records, a category being deleted), is
//! unassigned. Empty categories are kept.

use std::cmp::Reverse;

use uuid::Uuid;

use crate::api::{Bot, Category};

/// The comparison key the server uses for category names.
pub fn fold(name: &str) -> String {
    name.to_uppercase().to_lowercase()
}

/// The bot's category if the client knows it; otherwise unassigned.
pub fn known_category(categories: &[Category], category: Option<Uuid>) -> Option<Uuid> {
    category.filter(|id| categories.iter().any(|c| c.id == *id))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Group<'a> {
    /// `None` is the unassigned group.
    pub category: Option<&'a Category>,
    pub bots: Vec<&'a Bot>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout<'a> {
    /// The main bot, above every group and in none of them.
    pub pinned: Option<&'a Bot>,
    pub groups: Vec<Group<'a>>,
}

impl<'a> Layout<'a> {
    pub fn new(categories: &'a [Category], bots: &'a [Bot], main: Option<Uuid>) -> Self {
        let pinned = bots.iter().find(|b| Some(b.id) == main);
        let others = || bots.iter().filter(move |b| Some(b.id) != main);
        let mut sorted: Vec<&Category> = categories.iter().collect();
        sorted.sort_by_key(|c| (fold(&c.name), c.id));
        let mut groups: Vec<Group> = sorted
            .into_iter()
            .map(|category| Group {
                category: Some(category),
                bots: others()
                    .filter(|b| known_category(categories, b.category_id) == Some(category.id))
                    .collect(),
            })
            .collect();
        groups.push(Group {
            category: None,
            bots: others()
                .filter(|b| known_category(categories, b.category_id).is_none())
                .collect(),
        });
        for group in &mut groups {
            group.bots.sort_by_key(|b| {
                (
                    Reverse(b.last_message_at.or(b.created_at)),
                    fold(&b.name),
                    b.id,
                )
            });
        }
        Self { pinned, groups }
    }

    pub fn with_preferences(mut self, preferences: &crate::preferences::Preferences) -> Self {
        self.groups
            .retain(|g| g.category.is_some() || !g.bots.is_empty());
        self.groups.sort_by_key(|g| {
            (
                g.category.is_none(),
                g.category
                    .and_then(|c| preferences.category_order.iter().position(|id| *id == c.id))
                    .unwrap_or(usize::MAX),
            )
        });
        self
    }

    pub fn visible_order(&self, collapsed: &std::collections::HashSet<Option<Uuid>>) -> Vec<Uuid> {
        let grouped = self.groups.iter().any(|group| group.category.is_some());
        self.pinned
            .into_iter()
            .chain(
                self.groups
                    .iter()
                    .filter(|g| !grouped || !collapsed.contains(&g.category.map(|c| c.id)))
                    .flat_map(|g| g.bots.iter().copied()),
            )
            .filter(|b| b.is_active())
            .map(|b| b.id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn bot(id: u128, name: &str, category: Option<u128>) -> Bot {
        Bot {
            id: Uuid::from_u128(id),
            name: name.into(),
            metadata_revision: None,
            avatar: Default::default(),
            conversation_id: None,
            selected_provider_id: None,
            activity: None,
            cleanup: None,
            category_id: category.map(Uuid::from_u128),
            created_at: None,
            last_message_at: None,
            read_state: None,
        }
    }

    #[test]
    fn saved_order_collapse_and_empty_unassigned_match_navigation() {
        let categories = vec![
            category(1, "Alpha"),
            category(2, "Beta"),
            category(3, "New"),
        ];
        let bots = vec![
            bot(10, "Main", None),
            bot(11, "A", Some(1)),
            bot(12, "B", Some(2)),
        ];
        let mut prefs = crate::preferences::Preferences::default();
        prefs.category_order = vec![Uuid::from_u128(2), Uuid::from_u128(999), Uuid::from_u128(1)];
        prefs.collapsed.insert(Some(Uuid::from_u128(2)));
        let layout =
            Layout::new(&categories, &bots, Some(Uuid::from_u128(10))).with_preferences(&prefs);
        assert_eq!(
            shape(&layout),
            vec![
                (Some("Beta"), vec!["B"]),
                (Some("Alpha"), vec!["A"]),
                (Some("New"), vec![])
            ]
        );
        assert_eq!(
            layout.visible_order(&prefs.collapsed),
            vec![Uuid::from_u128(10), Uuid::from_u128(11)]
        );
    }

    #[test]
    fn removing_last_category_keeps_unassigned_bots_visible() {
        let bots = vec![bot(10, "Main", None), bot(11, "Other", None)];
        let layout = Layout::new(&[], &bots, Some(Uuid::from_u128(10)));
        assert_eq!(
            layout.visible_order(&[None].into()),
            vec![Uuid::from_u128(10), Uuid::from_u128(11)]
        );
    }

    fn at(minute: u32) -> Option<DateTime<Utc>> {
        Some(
            format!("2026-11-04T10:{minute:02}:00Z")
                .parse()
                .expect("timestamp"),
        )
    }

    fn category(id: u128, name: &str) -> Category {
        Category {
            id: Uuid::from_u128(id),
            name: name.into(),
        }
    }

    fn shape<'a>(layout: &Layout<'a>) -> Vec<(Option<&'a str>, Vec<&'a str>)> {
        layout
            .groups
            .iter()
            .map(|g| {
                (
                    g.category.map(|c| c.name.as_str()),
                    g.bots.iter().map(|b| b.name.as_str()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn categories_then_unassigned_in_a_deterministic_order() {
        let categories = vec![
            category(2, "work"),
            category(1, "Family"),
            category(3, "Empty"),
        ];
        let bots = vec![
            bot(10, "zeta", Some(2)),
            bot(11, "Alpha", Some(2)),
            bot(12, "beta", None),
            bot(13, "Mom", Some(1)),
            // A category the client does not know: shown unassigned.
            bot(14, "Orphan", Some(99)),
            bot(15, "alpha", Some(2)),
        ];
        let layout = Layout::new(&categories, &bots, None);
        assert_eq!(layout.pinned, None);
        assert_eq!(
            shape(&layout),
            vec![
                (Some("Empty"), vec![]),
                (Some("Family"), vec!["Mom"]),
                (Some("work"), vec!["Alpha", "alpha", "zeta"]),
                (None, vec!["beta", "Orphan"]),
            ]
        );
        assert_eq!(
            layout.visible_order(&Default::default()),
            [13, 11, 15, 10, 12, 14].map(Uuid::from_u128).to_vec()
        );
    }

    #[test]
    fn without_categories_everything_is_unassigned() {
        let bots = vec![bot(1, "b", None), bot(2, "a", Some(7))];
        let layout = Layout::new(&[], &bots, None);
        assert_eq!(shape(&layout), vec![(None, vec!["a", "b"])]);
    }

    #[test]
    fn main_is_pinned_above_every_category_and_listed_once() {
        let categories = vec![category(1, "Work")];
        let mut main = bot(1, "Main", Some(1));
        main.last_message_at = at(1);
        let mut busy = bot(2, "Busy", Some(1));
        busy.last_message_at = at(30);
        let bots = vec![busy, main, bot(3, "Loose", None)];
        let layout = Layout::new(&categories, &bots, Some(Uuid::from_u128(1)));
        assert_eq!(layout.pinned.map(|b| b.name.as_str()), Some("Main"));
        assert_eq!(
            shape(&layout),
            vec![(Some("Work"), vec!["Busy"]), (None, vec!["Loose"])]
        );
        assert_eq!(
            layout.visible_order(&Default::default()),
            [1, 2, 3].map(Uuid::from_u128).to_vec()
        );
    }

    #[test]
    fn newest_message_first_then_creation_time_within_a_category() {
        let mut talked = bot(1, "Talked", None);
        talked.created_at = at(0);
        talked.last_message_at = at(20);
        let mut older = bot(2, "Older", None);
        older.created_at = at(0);
        older.last_message_at = at(10);
        let mut new = bot(3, "New", None);
        new.created_at = at(40);
        let mut stale = bot(4, "Stale", None);
        stale.created_at = at(5);
        let mut deleting = bot(5, "Deleting", None);
        deleting.last_message_at = at(50);
        deleting.cleanup = Some(crate::api::Cleanup {
            state: "pending".into(),
            error: None,
        });
        let bots = vec![stale, older, talked, new, deleting];
        let layout = Layout::new(&[], &bots, None);
        assert_eq!(
            shape(&layout),
            vec![(None, vec!["Deleting", "New", "Talked", "Older", "Stale"])]
        );
        assert_eq!(
            layout.visible_order(&Default::default()),
            [3, 1, 2, 4].map(Uuid::from_u128).to_vec(),
            "navigation skips the bot being deleted"
        );
    }
}
