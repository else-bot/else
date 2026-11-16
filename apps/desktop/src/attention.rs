//! What one server's bots want from the owner, for its rail badges: unread
//! replies from bots that are not muted, and the requests waiting on the
//! owner, muted bots included.

use crate::api::{Bot, SidebarStatus};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attention {
    pub unread: u32,
    pub unread_bots: u32,
    /// In the server's bot order.
    pub requests: Vec<Request>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub bot: String,
    pub ask: Ask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    Answer,
    Approval,
    Takeover,
    Credentials,
    Owner,
}

impl Ask {
    fn of(status: &SidebarStatus) -> Option<Self> {
        Some(match status {
            SidebarStatus::WaitingForAnswer { .. } => Self::Answer,
            SidebarStatus::NeedsApproval { .. } => Self::Approval,
            SidebarStatus::NeedsTakeover { .. } => Self::Takeover,
            SidebarStatus::NeedsCredentials { .. } => Self::Credentials,
            SidebarStatus::NeedsYou { .. } => Self::Owner,
            SidebarStatus::Failed
            | SidebarStatus::Stopped
            | SidebarStatus::Uncertain
            | SidebarStatus::Unknown => return None,
        })
    }

    pub fn phrase(self) -> &'static str {
        match self {
            Self::Answer => "asked a question",
            Self::Approval => "needs approval",
            Self::Takeover => "needs you to take over",
            Self::Credentials => "needs credentials",
            Self::Owner => "needs you",
        }
    }
}

impl Attention {
    /// `unread` says whether a bot has unread replies; servers without a
    /// reply count count each unread bot once.
    pub fn of<'a>(bots: impl IntoIterator<Item = &'a Bot>, unread: impl Fn(&Bot) -> bool) -> Self {
        let mut attention = Self::default();
        for bot in bots.into_iter().filter(|bot| bot.is_active()) {
            if !bot.muted && unread(bot) {
                let replies = bot
                    .read_state
                    .as_ref()
                    .and_then(|state| state.unread_replies)
                    .unwrap_or(1)
                    .max(1);
                attention.unread = attention.unread.saturating_add(replies);
                attention.unread_bots += 1;
            }
            if let Some(ask) = bot.sidebar_status.as_ref().and_then(Ask::of) {
                attention.requests.push(Request {
                    bot: bot.name.clone(),
                    ask,
                });
            }
        }
        attention
    }

    pub fn is_empty(&self) -> bool {
        self.unread == 0 && self.requests.is_empty()
    }

    /// The badge text for `count`, capped so it fits on the icon.
    pub fn badge(count: usize) -> String {
        if count > 99 {
            "99+".into()
        } else {
            count.to_string()
        }
    }

    pub fn unread_line(&self) -> Option<String> {
        (self.unread > 0).then(|| match self.unread_bots {
            1 => format!("{} unread from 1 bot", self.unread),
            bots => format!("{} unread from {bots} bots", self.unread),
        })
    }

    pub fn waiting_line(&self) -> Option<String> {
        (!self.requests.is_empty()).then(|| format!("{} waiting on you", self.requests.len()))
    }

    /// Both lines in one, for screen readers.
    pub fn summary(&self) -> Option<String> {
        match (self.unread_line(), self.waiting_line()) {
            (Some(unread), Some(waiting)) => Some(format!("{unread}. {waiting}")),
            (one, other) => one.or(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ReadState, ReplyPosition};
    use uuid::Uuid;

    fn bot(name: &str, replies: Option<u32>, status: Option<SidebarStatus>) -> Bot {
        let mut bot: Bot = serde_json::from_value(serde_json::json!({
            "id": Uuid::new_v4(),
            "name": name,
        }))
        .unwrap();
        bot.read_state = Some(ReadState {
            latest: None,
            read_through: ReplyPosition::default(),
            marked_unread: replies.is_some(),
            unread_replies: replies,
        });
        bot.sidebar_status = status;
        bot
    }

    fn approval() -> Option<SidebarStatus> {
        Some(SidebarStatus::NeedsApproval {
            preview: String::new(),
            since: None,
        })
    }

    fn marked(bot: &Bot) -> bool {
        bot.read_state.as_ref().is_some_and(|s| s.marked_unread)
    }

    #[test]
    fn muted_bots_leave_the_unread_count_but_still_wait_on_the_owner() {
        let mut muted = bot("Muted", Some(7), approval());
        muted.muted = true;
        let bots = [
            bot("Writer", Some(3), None),
            bot("Reader", None, None),
            muted,
            bot("Old server", Some(0), None),
        ];
        let attention = Attention::of(&bots, marked);
        assert_eq!(attention.unread, 4, "an unread bot with no count is one");
        assert_eq!(attention.unread_bots, 2);
        assert_eq!(
            attention.requests,
            [Request {
                bot: "Muted".into(),
                ask: Ask::Approval
            }]
        );
        assert_eq!(
            attention.summary().as_deref(),
            Some("4 unread from 2 bots. 1 waiting on you")
        );
    }

    #[test]
    fn a_quiet_server_has_nothing_to_show() {
        let failed = bot("Failed", None, Some(SidebarStatus::Failed));
        let attention = Attention::of(&[failed], marked);
        assert!(attention.is_empty());
        assert_eq!(attention.summary(), None);
        assert_eq!(Attention::badge(100), "99+");
        assert_eq!(Attention::badge(99), "99");
    }
}
