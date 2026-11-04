//! Sidebar grouping (POC-031): categories with their bots, then the
//! unassigned bots. Pure, so the order is tested without a window.
//!
//! Categories are ordered as the server orders them: by the case-folded
//! name's code points, then ID. Bots within a group are ordered the same
//! way by name. A bot with no category, or with a category the client does
//! not know (older records, a category being deleted), is unassigned.
//! Empty categories are kept.

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

pub fn groups<'a>(categories: &'a [Category], bots: &'a [Bot]) -> Vec<Group<'a>> {
    let mut sorted: Vec<&Category> = categories.iter().collect();
    sorted.sort_by_key(|c| (fold(&c.name), c.id));
    let mut out: Vec<Group> = sorted
        .into_iter()
        .map(|category| Group {
            category: Some(category),
            bots: bots
                .iter()
                .filter(|b| known_category(categories, b.category_id) == Some(category.id))
                .collect(),
        })
        .collect();
    out.push(Group {
        category: None,
        bots: bots
            .iter()
            .filter(|b| known_category(categories, b.category_id).is_none())
            .collect(),
    });
    for group in &mut out {
        group.bots.sort_by_key(|b| (fold(&b.name), b.id));
    }
    out
}

/// The active bots in display order (keyboard navigation follows it).
pub fn order(categories: &[Category], bots: &[Bot]) -> Vec<Uuid> {
    groups(categories, bots)
        .into_iter()
        .flat_map(|g| g.bots.into_iter())
        .filter(|b| b.is_active())
        .map(|b| b.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(id: u128, name: &str, category: Option<u128>) -> Bot {
        Bot {
            id: Uuid::from_u128(id),
            name: name.into(),
            conversation_id: None,
            selected_provider_id: None,
            activity: None,
            cleanup: None,
            category_id: category.map(Uuid::from_u128),
        }
    }

    fn category(id: u128, name: &str) -> Category {
        Category {
            id: Uuid::from_u128(id),
            name: name.into(),
        }
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
        let groups = groups(&categories, &bots);
        let shape: Vec<(Option<&str>, Vec<&str>)> = groups
            .iter()
            .map(|g| {
                (
                    g.category.map(|c| c.name.as_str()),
                    g.bots.iter().map(|b| b.name.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (Some("Empty"), vec![]),
                (Some("Family"), vec!["Mom"]),
                // Same folded name: the ID breaks the tie.
                (Some("work"), vec!["Alpha", "alpha", "zeta"]),
                (None, vec!["beta", "Orphan"]),
            ]
        );
        assert_eq!(
            order(&categories, &bots),
            [13, 11, 15, 10, 12, 14].map(Uuid::from_u128).to_vec()
        );
    }

    #[test]
    fn without_categories_everything_is_unassigned() {
        let bots = vec![bot(1, "b", None), bot(2, "a", Some(7))];
        let groups = groups(&[], &bots);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0]
                .bots
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
}
