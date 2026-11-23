//! What the bot list shows for each bot, decided from the server's bot
//! record alone so the list stays a plain function of `GET /v1/bots`.

use chrono::{DateTime, Datelike as _, Local, Utc};
use std::collections::HashSet;

use else_client::layout::Layout;
use else_wire::bots::{Avatar, Bot, Category, Phase, ReplyPosition, SidebarStatus};
use uuid::Uuid;

/// The line under a bot's name, most urgent first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// The bot waits on the owner: an answer, an approval, a key.
    NeedsYou(String),
    Working,
    Problem(String),
    /// The newest reply's preview.
    Preview(String),
    Empty,
}

/// Where the owner is in reading a bot's chat, whether or not the list
/// shows it (a muted bot never looks unread).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// Unread; marking it read goes through its newest reply, when it has one.
    Unread {
        newest: Option<ReplyPosition>,
    },
    Read,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotRow {
    pub id: Uuid,
    pub name: String,
    pub avatar: Avatar,
    pub status: Status,
    pub unread: bool,
    pub reading: Reading,
    pub muted: bool,
    pub last_message_at: Option<DateTime<Utc>>,
}

impl BotRow {
    pub fn of(bot: &Bot) -> Self {
        let status = match &bot.sidebar_status {
            Some(
                SidebarStatus::WaitingForAnswer { preview, .. }
                | SidebarStatus::NeedsApproval { preview, .. }
                | SidebarStatus::NeedsTakeover { preview, .. }
                | SidebarStatus::NeedsCredentials { preview, .. }
                | SidebarStatus::NeedsYou { preview, .. },
            ) => Status::NeedsYou(one_line(preview)),
            Some(SidebarStatus::Failed) => Status::Problem("The last reply failed".into()),
            Some(SidebarStatus::Uncertain) => {
                Status::Problem("The last reply's outcome is unknown".into())
            }
            _ if bot.activity.as_ref().is_some_and(|a| {
                matches!(a.phase, Phase::Queued | Phase::Working | Phase::Cancelling)
            }) =>
            {
                Status::Working
            }
            _ => match bot.read_state.as_ref().and_then(|r| r.latest.as_ref()) {
                Some(latest) if !latest.preview.trim().is_empty() => {
                    Status::Preview(latest.plain_preview())
                }
                _ => Status::Empty,
            },
        };
        let reading = match &bot.read_state {
            Some(read) if read.unread => Reading::Unread {
                newest: read.latest.as_ref().map(|latest| latest.position),
            },
            _ => Reading::Read,
        };
        let unread = reading != Reading::Read && !bot.silenced();
        Self {
            id: bot.id,
            name: bot.name.clone(),
            avatar: bot.avatar,
            status,
            unread,
            reading,
            muted: bot.muted,
            last_message_at: bot.last_message_at,
        }
    }

    pub fn needs_you(&self) -> bool {
        matches!(self.status, Status::NeedsYou(_))
    }

    pub fn subtitle(&self) -> String {
        match &self.status {
            Status::NeedsYou(text) => {
                if text.is_empty() {
                    "Waiting for you".into()
                } else {
                    text.clone()
                }
            }
            Status::Working => "Working…".into(),
            Status::Problem(text) | Status::Preview(text) => text.clone(),
            Status::Empty => "No messages yet".into(),
        }
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `incoming` may replace `shown`: not when it is an older copy of
/// the bot's metadata, such as a slow answer landing after a live update.
pub fn is_newer(shown: &Bot, incoming: &Bot) -> bool {
    match (shown.metadata_revision, incoming.metadata_revision) {
        (Some(shown), Some(incoming)) => incoming >= shown,
        _ => true,
    }
}

/// One line of the bot list: a section's header or a bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A category's header; `None` is the unassigned bots, the waiting
    /// section has no category and its own key.
    Header {
        key: String,
        category: Option<Uuid>,
        title: String,
        count: usize,
        collapsible: bool,
    },
    /// A bot, keyed by its section, since a waiting bot also shows in its
    /// category.
    Bot { key: String, row: BotRow },
}

impl Line {
    pub fn key(&self) -> &str {
        match self {
            Self::Header { key, .. } | Self::Bot { key, .. } => key,
        }
    }
}

/// The bot list as the desktop's sidebar lays it out (else_client::layout):
/// the main bot, the bots waiting on the owner oldest request first, then
/// each category in the owner's order and the unassigned bots. Without
/// categories the bots follow the waiting ones without headers. A collapsed
/// category keeps its header.
pub fn lines(
    bots: &[Bot],
    categories: &[Category],
    main: Option<Uuid>,
    collapsed: &HashSet<Option<Uuid>>,
) -> Vec<Line> {
    let layout = Layout::new(categories, bots, main).without_empty_unassigned();
    let grouped = !categories.is_empty();
    let bot = |section: &str, bot: &Bot| Line::Bot {
        key: format!("{section}-{}", bot.id),
        row: BotRow::of(bot),
    };
    let mut lines: Vec<Line> = layout
        .pinned
        .filter(|b| b.is_active())
        .map(|b| bot("main", b))
        .into_iter()
        .collect();
    let waiting: Vec<&Bot> = layout
        .waiting
        .iter()
        .copied()
        .filter(|b| b.is_active())
        .collect();
    if !waiting.is_empty() && grouped {
        lines.push(Line::Header {
            key: "waiting".into(),
            category: None,
            title: "Waiting on you".into(),
            count: waiting.len(),
            collapsible: false,
        });
    }
    lines.extend(waiting.iter().map(|b| bot("waiting", b)));
    for group in &layout.groups {
        let active: Vec<&Bot> = group
            .bots
            .iter()
            .copied()
            .filter(|b| b.is_active())
            .collect();
        let category = group.category.map(|c| c.id);
        // Unassigned shows only with a bot in it; a deleted bot's record
        // does not count.
        if category.is_none() && active.is_empty() {
            continue;
        }
        let section = category.map_or_else(|| "unassigned".to_owned(), |id| id.to_string());
        if grouped {
            lines.push(Line::Header {
                key: format!("header-{section}"),
                category,
                title: group
                    .category
                    .map_or_else(|| "Unassigned".to_owned(), |c| c.name.clone()),
                count: active.len(),
                collapsible: true,
            });
            if collapsed.contains(&category) {
                continue;
            }
        } else {
            // Ungrouped, a waiting bot is listed once, above the rest.
            lines.extend(
                active
                    .iter()
                    .filter(|b| !waiting.iter().any(|w| w.id == b.id))
                    .map(|b| bot(&section, b)),
            );
            continue;
        }
        lines.extend(active.iter().map(|b| bot(&section, b)));
    }
    lines
}

/// A message time as iOS lists show it: the time today, "Yesterday", the
/// weekday within a week, else the date.
pub fn time_label(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let at = at.with_timezone(&Local);
    let now = now.with_timezone(&Local);
    let days = now
        .date_naive()
        .signed_duration_since(at.date_naive())
        .num_days();
    match days {
        ..=0 => at.format("%H:%M").to_string(),
        1 => "Yesterday".into(),
        2..=6 => at.format("%A").to_string(),
        _ if at.year() == now.year() => at.format("%-d %b").to_string(),
        _ => at.format("%-d/%-m/%y").to_string(),
    }
}

/// The active bots whose name holds `query`, ignoring case, in the list's
/// order (main first, then waiting and newest), without category headers.
pub fn found(bots: &[Bot], main: Option<Uuid>, query: &str) -> Vec<Line> {
    let query = query.trim().to_lowercase();
    lines(bots, &[], main, &HashSet::new())
        .into_iter()
        .filter(|line| match line {
            Line::Bot { row, .. } => row.name.to_lowercase().contains(&query),
            Line::Header { .. } => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bot(name: &str, extra: serde_json::Value) -> Bot {
        let mut value = json!({
            "id": Uuid::new_v4(), "name": name, "avatar": { "shape": "star", "color": "green" },
        });
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value(value).unwrap()
    }

    fn names(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|line| match line {
                Line::Header { title, .. } => format!("# {title}"),
                Line::Bot { row, .. } => row.name.clone(),
            })
            .collect()
    }

    #[test]
    fn a_search_finds_bots_by_name_ignoring_case_and_categories() {
        let bots = [
            bot("Scout", json!({ "category_id": Uuid::from_u128(1) })),
            bot("Ledger", json!({})),
            bot("Scoutmaster", json!({})),
        ];
        assert_eq!(
            names(&found(&bots, None, " scout ")),
            ["Scout", "Scoutmaster"]
        );
        assert!(found(&bots, None, "nobody").is_empty());
    }

    #[test]
    fn a_waiting_bot_shows_its_ask_and_sorts_first() {
        let bots = [
            bot(
                "Research",
                json!({ "last_message_at": "2026-11-22T10:00:00Z" }),
            ),
            bot(
                "Inbox",
                json!({
                    "last_message_at": "2026-11-20T10:00:00Z",
                    "sidebar_status": { "kind": "needs_approval", "preview": "Send the\nreply?" },
                }),
            ),
        ];
        let lines = lines(&bots, &[], None, &HashSet::new());
        assert_eq!(names(&lines), ["Inbox", "Research"]);
        let Line::Bot { row, .. } = &lines[0] else {
            panic!("a bot first")
        };
        assert_eq!(row.subtitle(), "Send the reply?");
    }

    #[test]
    fn categories_group_the_list_and_a_collapsed_one_keeps_its_header() {
        let growth: Category = serde_json::from_value(json!({
            "id": "00000000-0000-0000-0000-0000000000c1", "name": "Growth", "position": 0,
            "created_at": "2026-11-01T00:00:00Z", "updated_at": "2026-11-01T00:00:00Z",
        }))
        .unwrap();
        let bots = [
            bot("Main", json!({})),
            bot("Scout", json!({ "category_id": growth.id })),
            bot(
                "Ledger",
                json!({ "sidebar_status": { "kind": "needs_you", "preview": "Pick one" } }),
            ),
        ];
        let main = Some(bots[0].id);
        let open = lines(&bots, std::slice::from_ref(&growth), main, &HashSet::new());
        assert_eq!(
            names(&open),
            [
                "Main",
                "# Waiting on you",
                "Ledger",
                "# Growth",
                "Scout",
                "# Unassigned",
                "Ledger"
            ]
        );
        let folded = lines(
            &bots,
            std::slice::from_ref(&growth),
            main,
            &HashSet::from([Some(growth.id)]),
        );
        assert_eq!(
            names(&folded),
            [
                "Main",
                "# Waiting on you",
                "Ledger",
                "# Growth",
                "# Unassigned",
                "Ledger"
            ]
        );
    }

    #[test]
    fn an_older_copy_of_a_bot_never_replaces_a_newer_one() {
        let renamed = bot("Renamed", json!({ "metadata_revision": 5 }));
        let late = Bot {
            name: "Old name".into(),
            metadata_revision: Some(4),
            ..renamed.clone()
        };
        assert!(!is_newer(&renamed, &late));
        assert!(is_newer(&late, &renamed));
    }

    #[test]
    fn a_working_bot_says_so_over_its_preview() {
        let row = BotRow::of(&bot(
            "Code",
            json!({
                "activity": { "phase": "working", "active_turn_id": null, "queued_count": 0 },
                "read_state": {
                    "latest": { "turn_id": Uuid::nil(), "item_id": Uuid::nil(),
                        "position": { "item_seq": 1, "text_bytes": 2 }, "preview": "Done." },
                    "read_through": { "item_seq": 0, "text_bytes": 0 },
                    "marked_unread": false, "unread": true,
                },
            }),
        ));
        assert_eq!(row.subtitle(), "Working…");
        assert!(row.unread);
        // Marking it read goes through the newest reply.
        assert_eq!(
            row.reading,
            Reading::Unread {
                newest: Some(ReplyPosition {
                    item_seq: 1,
                    text_bytes: 2
                })
            }
        );
    }

    #[test]
    fn a_muted_bot_is_never_unread() {
        let row = BotRow::of(&bot(
            "Quiet",
            json!({
                "muted": true,
                "read_state": {
                    "latest": null, "read_through": { "item_seq": 0, "text_bytes": 0 },
                    "marked_unread": false, "unread": true,
                },
            }),
        ));
        assert!(!row.unread);
    }

    #[test]
    fn times_read_like_ios_lists() {
        let now: DateTime<Utc> = Local::now().with_timezone(&Utc);
        assert_eq!(
            time_label(now, now),
            now.with_timezone(&Local).format("%H:%M").to_string()
        );
        assert_eq!(
            time_label(now - chrono::Duration::days(1), now),
            "Yesterday"
        );
    }
}
