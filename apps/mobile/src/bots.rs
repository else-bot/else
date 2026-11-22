//! What the bot list shows for each bot, decided from the server's bot
//! record alone so the list stays a plain function of `GET /v1/bots`.

use chrono::{DateTime, Datelike as _, Local, Utc};
use else_wire::bots::{AvatarColor, Bot, Phase, SidebarStatus};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotRow {
    pub id: Uuid,
    pub name: String,
    pub initial: String,
    pub color: u32,
    pub status: Status,
    pub unread: bool,
    pub muted: bool,
    pub last_message_at: Option<DateTime<Utc>>,
}

pub fn avatar_rgb(color: AvatarColor) -> u32 {
    match color {
        AvatarColor::White => 0xa1a1a6,
        AvatarColor::Brown => 0xa4693f,
        AvatarColor::Red => 0xee3b47,
        AvatarColor::Orange => 0xf6631d,
        AvatarColor::Amber => 0xf59e0b,
        AvatarColor::Yellow => 0xe0b021,
        AvatarColor::Green => 0x22c162,
        AvatarColor::Teal => 0x17b3a3,
        AvatarColor::Blue => 0x2b88fb,
        AvatarColor::Purple => 0x8f5cf6,
        AvatarColor::Pink => 0xec4b9a,
        AvatarColor::Gray => 0x7d7d84,
    }
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
        let unread = bot.read_state.as_ref().is_some_and(|r| r.unread) && !bot.silenced();
        Self {
            id: bot.id,
            name: bot.name.clone(),
            initial: bot
                .name
                .chars()
                .find(|c| c.is_alphanumeric())
                .map(|c| c.to_uppercase().collect())
                .unwrap_or_else(|| "?".into()),
            color: avatar_rgb(bot.avatar.color),
            status,
            unread,
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

/// The bots in list order: those waiting on the owner first, then by their
/// newest message, never-used bots last by name.
pub fn rows(bots: &[Bot]) -> Vec<BotRow> {
    let mut rows: Vec<BotRow> = bots
        .iter()
        .filter(|b| b.is_active())
        .map(BotRow::of)
        .collect();
    rows.sort_by(|a, b| {
        b.needs_you()
            .cmp(&a.needs_you())
            .then(b.last_message_at.cmp(&a.last_message_at))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    rows
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
        let rows = rows(&bots);
        assert_eq!(rows[0].name, "Inbox");
        assert_eq!(rows[0].subtitle(), "Send the reply?");
        assert_eq!(rows[1].name, "Research");
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
        assert_eq!(row.initial, "C");
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
