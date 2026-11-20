//! What the owner has read, and which new replies deserve a system
//! notification. Pure state, owned by the messaging view; the server keeps
//! the durable read position and this keeps only what is in flight.
//!
//! Reads go to the server one request per bot at a time. Later wishes are
//! merged into the next request instead of queued: positions only move
//! forward, and of two explicit marks the later one wins.

use crate::model::Phase;
use std::collections::HashMap;

use uuid::Uuid;

use crate::api::{Bot, ReadCommand, ReplyPosition, SidebarStatus};

/// The owner's explicit choice from the bot menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Read,
    Unread,
}

/// How a read request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Try again later; nothing is lost.
    Retry,
    /// The server refused it for good.
    Refused,
}

#[derive(Debug, Default)]
struct ReadIntent {
    position: Option<ReplyPosition>,
    mark: Option<Mark>,
}

impl ReadIntent {
    fn advance(&mut self, position: ReplyPosition) {
        self.position = Some(
            self.position
                .map_or(position, |pending| pending.max(position)),
        );
    }

    fn retry(&mut self, command: ReadCommand) {
        if let Some(position) = command_position(command) {
            self.advance(position);
        }
        self.mark = self.mark.or_else(|| command_mark(command));
    }
}

#[derive(Debug, Default)]
struct ReadSync {
    observed: ReplyPosition,
    confirmed: ReplyPosition,
    pending: ReadIntent,
    inflight: Option<ReadCommand>,
}

impl ReadSync {
    fn observe(&mut self, position: ReplyPosition, server: ReplyPosition) -> bool {
        if position <= self.observed {
            return false;
        }
        self.observed = position;
        if position <= self.confirmed.max(server) {
            return false;
        }
        self.pending.advance(position);
        true
    }

    fn mark(&self) -> Option<Mark> {
        self.pending
            .mark
            .or_else(|| self.inflight.and_then(command_mark))
    }

    fn optimistic_position(&self) -> ReplyPosition {
        self.confirmed
            .max(self.pending.position.unwrap_or_default())
            .max(self.inflight.and_then(command_position).unwrap_or_default())
    }

    fn next(&mut self) -> Option<ReadCommand> {
        if self.inflight.is_some() {
            return None;
        }
        let command = match self.pending.mark.take() {
            Some(Mark::Unread) => ReadCommand::MarkUnread,
            Some(Mark::Read) => ReadCommand::MarkRead {
                position: self.pending.position.take(),
            },
            None => ReadCommand::Through {
                position: self.pending.position.take()?,
            },
        };
        self.inflight = Some(command);
        Some(command)
    }

    fn finished(&mut self, outcome: Outcome) {
        let Some(command) = self.inflight.take() else {
            return;
        };
        match outcome {
            Outcome::Done => {
                if let Some(position) = command_position(command) {
                    self.confirmed = self.confirmed.max(position);
                }
            }
            Outcome::Retry => self.pending.retry(command),
            Outcome::Refused => {}
        }
    }
}

fn command_position(command: ReadCommand) -> Option<ReplyPosition> {
    match command {
        ReadCommand::Through { position } => Some(position),
        ReadCommand::MarkRead { position } => position,
        ReadCommand::MarkUnread => None,
    }
}

fn command_mark(command: ReadCommand) -> Option<Mark> {
    match command {
        ReadCommand::Through { .. } => None,
        ReadCommand::MarkRead { .. } => Some(Mark::Read),
        ReadCommand::MarkUnread => Some(Mark::Unread),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub bot: Uuid,
    pub title: String,
    pub body: String,
}

#[derive(Debug)]
struct Announcement {
    position: ReplyPosition,
    shown: bool,
}

#[derive(Debug, Default)]
struct BotAttention {
    read: ReadSync,
    head: Option<ReplyPosition>,
    due: bool,
    announcement: Option<Announcement>,
    ask: AskAlert,
}

/// A task of the bot that ended needing the owner (`NeedsYou`). It is
/// announced once, whether or not the bot's reply is unread, and taken down
/// when the owner settles it.
#[derive(Debug, Default)]
struct AskAlert {
    /// The ask last seen open. A named one survives another card showing in
    /// the sidebar for a while, so the same ask is not announced again after.
    seen: Option<AskKey>,
    due: bool,
    shown: bool,
}

/// The bot's open ask. A server that names it keys it by its ID, which a
/// rewrite of its text keeps; older ones by its time and text.
fn open_ask(bot: &Bot) -> Option<AskKey> {
    match &bot.sidebar_status {
        Some(SidebarStatus::NeedsYou {
            notice_id: Some(id),
            ..
        }) => Some(AskKey::Notice(*id)),
        Some(SidebarStatus::NeedsYou { preview, since, .. }) => {
            Some(AskKey::Legacy(format!("{since:?} {preview}")))
        }
        _ => None,
    }
}

/// Whether the sidebar shows a card it ranks above an ask, so an open ask
/// may be behind it.
fn masks_an_ask(bot: &Bot) -> bool {
    matches!(
        bot.sidebar_status,
        Some(
            SidebarStatus::WaitingForAnswer { .. }
                | SidebarStatus::NeedsApproval { .. }
                | SidebarStatus::NeedsTakeover { .. }
                | SidebarStatus::NeedsCredentials { .. }
        )
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AskKey {
    Notice(Uuid),
    Legacy(String),
}

#[derive(Debug, Default)]
pub struct Inbox {
    bots: HashMap<Uuid, BotAttention>,
    opening: Option<Uuid>,
    baseline: bool,
}

impl Inbox {
    pub fn is_unread(&self, bot: &Bot) -> bool {
        unread(self.bots.get(&bot.id).map(|attention| &attention.read), bot)
    }

    fn is_marked(&self, bot: &Bot) -> bool {
        match self
            .bots
            .get(&bot.id)
            .and_then(|attention| attention.read.mark())
        {
            Some(mark) => mark == Mark::Unread,
            None => bot.read_state.as_ref().is_some_and(|s| s.marked_unread),
        }
    }

    /// The owner opened this bot: seeing its replies now clears its mark.
    pub fn open(&mut self, bot: Uuid) {
        self.opening = Some(bot);
    }

    /// The owner's explicit choice. Marking the open bot unread keeps it
    /// unread until it is opened again.
    pub fn mark(&mut self, bot: &Bot, mark: Mark) {
        if mark == Mark::Unread && self.opening == Some(bot.id) {
            self.opening = None;
        }
        let sync = &mut self.bots.entry(bot.id).or_default().read;
        if mark == Mark::Read
            && let Some(latest) = bot.read_state.as_ref().and_then(|s| s.latest.as_ref())
        {
            sync.observed = sync.observed.max(latest.position);
            sync.pending.advance(latest.position);
        }
        sync.pending.mark = Some(mark);
    }

    /// The owner saw the bot's replies up to `position`. Returns whether
    /// that changed anything to send.
    pub fn saw(&mut self, bot: &Bot, position: ReplyPosition) -> bool {
        let opened = self.opening.take_if(|id| *id == bot.id).is_some();
        let clear = opened && self.is_marked(bot);
        let server = bot
            .read_state
            .as_ref()
            .map(|state| state.read_through)
            .unwrap_or_default();
        let sync = &mut self.bots.entry(bot.id).or_default().read;
        let mut changed = sync.observe(position, server);
        if clear {
            sync.pending.mark = Some(Mark::Read);
            changed = true;
        }
        changed
    }

    /// The furthest seen here or saved on the server.
    pub fn seen_through(&self, bot: &Bot) -> ReplyPosition {
        let local = self
            .bots
            .get(&bot.id)
            .map(|attention| attention.read.optimistic_position())
            .unwrap_or_default();
        bot.read_state
            .as_ref()
            .map_or(local, |s| s.read_through.max(local))
    }

    /// The request to send for `bot` now, if any and none is in flight.
    pub fn next_command(&mut self, bot: Uuid) -> Option<ReadCommand> {
        self.bots.get_mut(&bot)?.read.next()
    }

    pub fn finished(&mut self, bot: Uuid, outcome: Outcome) {
        if let Some(attention) = self.bots.get_mut(&bot) {
            attention.read.finished(outcome);
        }
    }

    /// Note new replies in a bot list, and on the first list the replies of
    /// turns still running. Returns whether any await a notification
    /// decision now that their bot handed back.
    pub fn observe(&mut self, bots: &[Bot]) -> bool {
        let first = !self.baseline;
        self.baseline = true;
        let mut due = false;
        for bot in bots.iter().filter(|bot| bot.is_active()) {
            let attention = self.bots.entry(bot.id).or_default();
            match open_ask(bot) {
                Some(ask) if attention.ask.seen.as_ref() != Some(&ask) => {
                    attention.ask.due = !first;
                    attention.ask.seen = Some(ask);
                }
                Some(_) => {}
                // Without an ID, the same text could be a new ask later, so
                // the key goes once the ask is settled: no card the sidebar
                // ranks above it could be hiding it.
                None => {
                    if matches!(attention.ask.seen, Some(AskKey::Legacy(_))) && !masks_an_ask(bot) {
                        attention.ask.seen = None;
                    }
                }
            }
            due |= attention.ask.due;
            let Some(latest) = bot
                .read_state
                .as_ref()
                .and_then(|state| state.latest.as_ref())
            else {
                continue;
            };
            let before = attention.head.replace(latest.position);
            let announced = attention
                .announcement
                .as_ref()
                .is_some_and(|alert| alert.position >= latest.position);
            let news = if first {
                !handed_back(bot)
            } else {
                before.is_none_or(|position| latest.position > position)
            };
            attention.due |= news && !announced;
            due |= attention.due && handed_back(bot);
        }
        due
    }

    /// Decide the due notifications. `viewing` says whether the owner is
    /// looking at that bot's conversation right now.
    pub fn alerts(&mut self, bots: &[Bot], viewing: impl Fn(Uuid) -> bool) -> Vec<Alert> {
        let mut alerts = Vec::new();
        for bot in bots.iter().filter(|bot| bot.is_active()) {
            let Some(attention) = self.bots.get_mut(&bot.id) else {
                continue;
            };
            if std::mem::take(&mut attention.ask.due)
                && let Some(SidebarStatus::NeedsYou { preview, .. }) = &bot.sidebar_status
                && !viewing(bot.id)
            {
                attention.ask.shown = true;
                alerts.push(Alert {
                    bot: bot.id,
                    title: format!("{} needs you", bot.name),
                    body: preview.clone(),
                });
            }
            let Some(latest) = bot
                .read_state
                .as_ref()
                .and_then(|state| state.latest.as_ref())
            else {
                continue;
            };
            if !attention.due || !handed_back(bot) {
                continue;
            }
            attention.due = false;
            // The reply shares the ask's notification, so its prose would
            // replace what the owner has to act on.
            if matches!(bot.sidebar_status, Some(SidebarStatus::NeedsYou { .. })) {
                continue;
            }
            if !unread(Some(&attention.read), bot) || viewing(bot.id) {
                continue;
            }
            attention.announcement = Some(Announcement {
                position: latest.position,
                shown: true,
            });
            alerts.push(Alert {
                bot: bot.id,
                title: bot.name.clone(),
                body: latest.plain_preview(),
            });
        }
        alerts.sort_by_key(|a| a.bot);
        alerts
    }

    /// Notifications to take down: their bot was read, deleted, or is gone.
    pub fn settle(&mut self, bots: &[Bot]) -> Vec<Uuid> {
        let live: HashMap<_, _> = bots
            .iter()
            .filter(|bot| bot.is_active())
            .map(|bot| (bot.id, bot))
            .collect();
        let mut gone = Vec::new();
        self.bots.retain(|id, attention| {
            let unread = live
                .get(id)
                .is_some_and(|bot| unread(Some(&attention.read), bot));
            if let Some(alert) = &mut attention.announcement
                && alert.shown
                && !unread
            {
                alert.shown = false;
                gone.push(*id);
            }
            let asking = live.get(id).is_some_and(|bot| open_ask(bot).is_some());
            if attention.ask.shown && !asking {
                attention.ask.shown = false;
                if !gone.contains(id) {
                    gone.push(*id);
                }
            }
            live.contains_key(id) || attention.read.inflight.is_some()
        });
        self.opening = self.opening.filter(|id| live.contains_key(id));
        gone.sort();
        gone
    }

    pub fn shown(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.bots.iter().filter_map(|(id, attention)| {
            attention
                .announcement
                .as_ref()
                .is_some_and(|alert| alert.shown)
                .then_some(*id)
                .or(attention.ask.shown.then_some(*id))
        })
    }
}

/// The bot has nothing left to run, or its turn is parked on the owner.
/// Replies before then are progress updates and stay silent; a turn
/// continuing after its delegated tasks is queued, so it is not idle. A
/// takeover request leaves its turn running, so its reply waits for idle.
fn handed_back(bot: &Bot) -> bool {
    bot.activity.as_ref().is_none_or(|a| a.phase == Phase::Idle)
        || matches!(
            bot.sidebar_status,
            Some(
                SidebarStatus::WaitingForAnswer { .. }
                    | SidebarStatus::NeedsApproval { .. }
                    | SidebarStatus::NeedsCredentials { .. }
            )
        )
}

/// Unread by the server's saved read state alone, for a server this
/// desktop has no live inbox for.
pub fn saved_unread(bot: &Bot) -> bool {
    unread(None, bot)
}

fn unread(sync: Option<&ReadSync>, bot: &Bot) -> bool {
    if bot.silenced() {
        return false;
    }
    let mark = sync.and_then(ReadSync::mark);
    let Some(state) = bot.read_state.as_ref().filter(|_| bot.is_active()) else {
        return mark == Some(Mark::Unread);
    };
    let marked = mark.map_or(state.marked_unread, |mark| mark == Mark::Unread);
    let through = state
        .read_through
        .max(sync.map(ReadSync::optimistic_position).unwrap_or_default());
    marked
        || state
            .latest
            .as_ref()
            .is_some_and(|latest| latest.position > through)
}

/// A notification's tag: the connection it came from and its bot, so a
/// response from another connection or for a deleted bot does nothing.
pub fn tag(scope: Uuid, bot: Uuid) -> String {
    format!("else-bot:{scope}:{bot}")
}

pub fn parse_tag(tag: &str) -> Option<(Uuid, Uuid)> {
    let rest = tag.strip_prefix("else-bot:")?;
    let (scope, bot) = rest.split_once(':')?;
    Some((scope.parse().ok()?, bot.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{LatestReply, ReadState};

    fn at(item_seq: i64, text_bytes: i64) -> ReplyPosition {
        ReplyPosition {
            item_seq,
            text_bytes,
        }
    }

    fn turn(n: u128) -> Uuid {
        Uuid::from_u128(1000 + n)
    }

    fn bot(id: u128, reply: Option<(u128, ReplyPosition)>, read: ReplyPosition) -> Bot {
        serde_json::from_value(serde_json::json!({
            "id": Uuid::from_u128(id),
            "name": format!("Bot {id}"),
        }))
        .map(|mut bot: Bot| {
            bot.read_state = Some(ReadState {
                latest: reply.map(|(n, position)| LatestReply {
                    turn_id: turn(n),
                    item_id: Uuid::from_u128(2000 + n),
                    position,
                    preview: format!("Reply {n}"),
                }),
                read_through: read,
                marked_unread: false,
                unread_replies: None,
            });
            bot
        })
        .unwrap()
    }

    fn flagged(mut bot: Bot) -> Bot {
        bot.read_state.as_mut().unwrap().marked_unread = true;
        bot
    }

    fn nobody(_: Uuid) -> bool {
        false
    }

    #[test]
    fn the_first_list_restores_unread_without_announcing_it() {
        let mut inbox = Inbox::default();
        let waiting = bot(1, Some((1, at(4, 30))), at(2, 10));
        assert!(!inbox.observe(std::slice::from_ref(&waiting)));
        assert!(
            inbox
                .alerts(std::slice::from_ref(&waiting), nobody)
                .is_empty()
        );
        assert!(inbox.is_unread(&waiting));
        assert!(!inbox.observe(&[waiting]), "a repeated list is no news");
    }

    fn busy(mut bot: Bot, phase: Phase) -> Bot {
        bot.activity = Some(crate::api::Activity {
            phase,
            active_turn_id: (phase == Phase::Working).then(|| turn(1)),
            queued_count: u32::from(phase == Phase::Queued),
            waiting_on: None,
        });
        bot
    }

    fn finished(mut bot: Bot, preview: &str) -> Bot {
        bot.read_state
            .as_mut()
            .unwrap()
            .latest
            .as_mut()
            .unwrap()
            .preview = preview.into();
        bot
    }

    #[test]
    fn progress_updates_wait_for_the_bot_to_finish() {
        let mut inbox = Inbox::default();
        inbox.observe(&[busy(bot(1, None, at(0, 0)), Phase::Working)]);
        for (position, phase) in [
            (at(2, 7), Phase::Working),
            (at(3, 12), Phase::Working),
            (at(3, 12), Phase::Queued),
        ] {
            let update = busy(bot(1, Some((1, position)), at(0, 0)), phase);
            assert!(!inbox.observe(std::slice::from_ref(&update)), "{phase:?}");
            assert!(inbox.alerts(&[update], nobody).is_empty(), "{phase:?}");
        }
        let done = finished(
            bot(1, Some((1, at(3, 12))), at(0, 0)),
            "Final is live on\nproduction",
        );
        assert!(
            inbox.observe(std::slice::from_ref(&done)),
            "the bot went idle"
        );
        assert_eq!(
            inbox.alerts(std::slice::from_ref(&done), nobody),
            [Alert {
                bot: Uuid::from_u128(1),
                title: "Bot 1".into(),
                body: "Final is live on production".into(),
            }]
        );
        assert!(!inbox.observe(&[done]), "announced once");
    }

    #[test]
    fn a_question_is_announced_and_so_is_the_answer_it_led_to() {
        let mut inbox = Inbox::default();
        inbox.observe(&[busy(bot(1, None, at(0, 0)), Phase::Working)]);
        let mut asking = busy(bot(1, Some((1, at(2, 7))), at(0, 0)), Phase::Working);
        asking.sidebar_status = Some(crate::api::SidebarStatus::WaitingForAnswer {
            preview: "Which one?".into(),
            since: None,
        });
        assert!(inbox.observe(std::slice::from_ref(&asking)));
        assert_eq!(inbox.alerts(&[asking], nobody).len(), 1);
        let resumed = busy(bot(1, Some((1, at(4, 9))), at(0, 0)), Phase::Working);
        assert!(!inbox.observe(std::slice::from_ref(&resumed)));
        let done = bot(1, Some((1, at(4, 9))), at(0, 0));
        assert!(
            inbox.observe(std::slice::from_ref(&done)),
            "same turn, new reply"
        );
        assert_eq!(inbox.alerts(&[done], nobody).len(), 1);
    }

    #[test]
    fn a_pending_owner_notice_is_announced_once_and_keeps_progress_replies_quiet() {
        let mut inbox = Inbox::default();
        inbox.observe(&[busy(bot(1, None, at(0, 0)), Phase::Working)]);
        let mut working = busy(bot(1, Some((1, at(2, 7))), at(0, 0)), Phase::Working);
        working.sidebar_status = Some(SidebarStatus::NeedsYou {
            preview: "Choose another bot".into(),
            since: None,
            notice_id: None,
        });
        assert!(inbox.observe(std::slice::from_ref(&working)));
        let alerts = inbox.alerts(std::slice::from_ref(&working), nobody);
        assert_eq!(
            alerts,
            [Alert {
                bot: working.id,
                title: format!("{} needs you", working.name),
                body: "Choose another bot".into(),
            }],
            "the ask, not the progress reply"
        );
        let mut later = busy(bot(1, Some((1, at(3, 7))), at(0, 0)), Phase::Working);
        later.sidebar_status = working.sidebar_status.clone();
        inbox.observe(std::slice::from_ref(&later));
        assert!(
            inbox
                .alerts(std::slice::from_ref(&later), nobody)
                .is_empty(),
            "the same ask is not announced again"
        );

        let mut handled = later;
        handled.sidebar_status = None;
        assert_eq!(inbox.settle(std::slice::from_ref(&handled)), [handled.id]);
    }

    #[test]
    fn an_ask_is_not_announced_while_its_chat_is_open_or_on_the_first_list() {
        let asking = |since: u32| {
            let mut bot = bot(1, None, at(0, 0));
            bot.sidebar_status = Some(SidebarStatus::NeedsYou {
                preview: "Approve the Romow DM".into(),
                since: chrono::DateTime::from_timestamp(i64::from(since), 0),
                notice_id: None,
            });
            bot
        };
        let mut inbox = Inbox::default();
        assert!(
            !inbox.observe(&[asking(1)]),
            "an ask open at startup stays quiet"
        );
        assert!(inbox.observe(&[asking(2)]), "a new ask is due");
        assert!(inbox.alerts(&[asking(2)], |_| true).is_empty());
        assert!(inbox.shown().next().is_none());
    }

    #[test]
    fn an_ask_keeps_its_notification_over_the_reply_and_is_announced_once() {
        let with = |status: SidebarStatus| {
            let mut bot = bot(1, Some((1, at(2, 9))), at(0, 0));
            bot.sidebar_status = Some(status);
            bot
        };
        let first = Uuid::from_u128(1);
        let ask_of = |id: Uuid, preview: &str| SidebarStatus::NeedsYou {
            preview: preview.into(),
            since: chrono::DateTime::from_timestamp(5, 0),
            notice_id: Some(id),
        };
        let ask = |preview: &str| ask_of(first, preview);
        let mut inbox = Inbox::default();
        inbox.observe(&[bot(1, None, at(0, 0))]);
        let asking = with(ask("Approve the Romow DM"));
        assert!(inbox.observe(std::slice::from_ref(&asking)));
        let alerts = inbox.alerts(std::slice::from_ref(&asking), nobody);
        assert_eq!(alerts.len(), 1, "{alerts:?}");
        assert_eq!(alerts[0].title, format!("{} needs you", asking.name));
        for _ in 0..2 {
            inbox.observe(std::slice::from_ref(&asking));
            assert!(
                inbox
                    .alerts(std::slice::from_ref(&asking), nobody)
                    .is_empty(),
                "the reply's prose does not replace the ask"
            );
        }

        let rewritten = with(ask("Approve the Romow DM, now with the fee"));
        inbox.observe(std::slice::from_ref(&rewritten));
        assert!(
            inbox
                .alerts(std::slice::from_ref(&rewritten), nobody)
                .is_empty()
        );
        let masked = with(SidebarStatus::NeedsApproval {
            preview: "Run the migration".into(),
            since: None,
        });
        inbox.observe(std::slice::from_ref(&masked));
        inbox.alerts(std::slice::from_ref(&masked), nobody);
        inbox.observe(std::slice::from_ref(&rewritten));
        assert!(
            inbox
                .alerts(std::slice::from_ref(&rewritten), nobody)
                .is_empty(),
            "the same ask showing again after another card is not new"
        );

        let same_time = with(ask_of(Uuid::from_u128(2), "Approve the Romow DM"));
        assert!(inbox.observe(std::slice::from_ref(&same_time)));
        assert_eq!(
            inbox.alerts(std::slice::from_ref(&same_time), nobody).len(),
            1,
            "another ask raised in the same instant is new"
        );
    }

    #[test]
    fn an_older_server_announces_the_same_text_again_once_its_ask_settled() {
        let legacy = |asking: bool| {
            let mut bot = bot(1, None, at(0, 0));
            bot.sidebar_status = asking.then(|| SidebarStatus::NeedsYou {
                preview: "Approve the Romow DM".into(),
                since: None,
                notice_id: None,
            });
            bot
        };
        let mut inbox = Inbox::default();
        inbox.observe(&[legacy(false)]);
        assert!(inbox.observe(&[legacy(true)]));
        assert_eq!(inbox.alerts(&[legacy(true)], nobody).len(), 1);
        inbox.observe(&[legacy(false)]);
        assert!(inbox.observe(&[legacy(true)]));
        assert_eq!(inbox.alerts(&[legacy(true)], nobody).len(), 1);

        let mut masked = legacy(false);
        masked.sidebar_status = Some(SidebarStatus::NeedsApproval {
            preview: "Run the migration".into(),
            since: None,
        });
        inbox.observe(std::slice::from_ref(&masked));
        inbox.alerts(std::slice::from_ref(&masked), nobody);
        inbox.observe(&[legacy(true)]);
        assert!(
            inbox.alerts(&[legacy(true)], nobody).is_empty(),
            "a card above the ask does not make it new"
        );
    }

    #[test]
    fn a_turn_running_at_startup_is_announced_when_it_finishes() {
        let mut inbox = Inbox::default();
        assert!(!inbox.observe(&[busy(bot(1, Some((1, at(2, 7))), at(0, 0)), Phase::Working)]));
        let done = bot(1, Some((1, at(2, 7))), at(0, 0));
        assert!(inbox.observe(std::slice::from_ref(&done)));
        assert_eq!(inbox.alerts(&[done], nobody).len(), 1);
    }

    #[test]
    fn a_new_turn_is_announced_while_an_older_reply_is_still_unread() {
        let mut inbox = Inbox::default();
        inbox.observe(&[bot(1, None, at(0, 0))]);
        let first = bot(1, Some((1, at(2, 9))), at(0, 0));
        inbox.observe(std::slice::from_ref(&first));
        assert_eq!(inbox.alerts(&[first], nobody).len(), 1);
        let next = bot(1, Some((2, at(4, 9))), at(0, 0));
        assert!(inbox.observe(std::slice::from_ref(&next)));
        assert_eq!(inbox.alerts(&[next], nobody).len(), 1);
    }

    #[test]
    fn stale_lists_and_read_replies_are_not_announced() {
        let mut inbox = Inbox::default();
        inbox.observe(&[bot(1, Some((1, at(4, 9))), at(4, 9))]);
        assert!(
            !inbox.observe(&[bot(1, Some((1, at(3, 9))), at(0, 0))]),
            "older"
        );
        let read_elsewhere = bot(1, Some((2, at(6, 9))), at(6, 9));
        assert!(inbox.observe(std::slice::from_ref(&read_elsewhere)));
        assert!(inbox.alerts(&[read_elsewhere], nobody).is_empty());
    }

    #[test]
    fn the_viewed_conversation_is_not_announced_but_others_are() {
        let mut inbox = Inbox::default();
        inbox.observe(&[bot(1, None, at(0, 0)), bot(2, None, at(0, 0))]);
        let bots = [
            bot(1, Some((1, at(2, 9))), at(0, 0)),
            bot(2, Some((2, at(2, 9))), at(0, 0)),
        ];
        inbox.observe(&bots);
        let alerts = inbox.alerts(&bots, |id| id == Uuid::from_u128(1));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].bot, Uuid::from_u128(2));
    }

    #[test]
    fn muted_bots_stay_read_and_silent_and_muting_closes_their_notification() {
        let mut inbox = Inbox::default();
        inbox.observe(&[bot(1, None, at(0, 0)), bot(2, None, at(0, 0))]);
        let mut quiet = flagged(bot(1, Some((1, at(2, 9))), at(0, 0)));
        quiet.muted = true;
        let loud = bot(2, Some((2, at(2, 9))), at(0, 0));
        let bots = vec![quiet.clone(), loud.clone()];
        inbox.observe(&bots);
        assert!(
            !inbox.is_unread(&quiet),
            "neither new replies nor a mark show"
        );
        let alerts = inbox.alerts(&bots, nobody);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].bot, loud.id);

        let mut muted = loud;
        muted.muted = true;
        assert_eq!(inbox.settle(&[quiet, muted.clone()]), [muted.id]);
        muted.muted = false;
        assert!(inbox.is_unread(&muted), "unmuting shows what is unread");
    }

    #[test]
    fn a_muted_bot_waiting_on_the_owner_is_announced_until_the_request_closes() {
        let mut inbox = Inbox::default();
        inbox.observe(&[busy(bot(1, None, at(0, 0)), Phase::Working)]);
        let takeover = || {
            Some(SidebarStatus::NeedsTakeover {
                preview: "Inspect the open X conversation.".into(),
                since: None,
            })
        };
        let mut streaming = busy(bot(1, Some((1, at(2, 4))), at(0, 0)), Phase::Working);
        streaming.muted = true;
        streaming.sidebar_status = takeover();
        assert!(
            !inbox.observe(std::slice::from_ref(&streaming)),
            "a takeover request leaves the turn running; its reply is still streaming"
        );
        let mut waiting = bot(1, Some((1, at(2, 9))), at(0, 0));
        waiting.muted = true;
        waiting.sidebar_status = takeover();
        assert!(inbox.observe(std::slice::from_ref(&waiting)));
        assert!(inbox.is_unread(&waiting));
        assert_eq!(
            inbox.alerts(std::slice::from_ref(&waiting), nobody).len(),
            1
        );

        let mut settled = waiting;
        settled.sidebar_status = None;
        assert!(!inbox.is_unread(&settled), "mute applies again");
        assert_eq!(inbox.settle(&[settled.clone()]), [settled.id]);
    }

    #[test]
    fn notifications_close_when_read_or_deleted() {
        let mut inbox = Inbox::default();
        inbox.observe(&[bot(1, None, at(0, 0)), bot(2, None, at(0, 0))]);
        let bots = vec![
            bot(1, Some((1, at(2, 9))), at(0, 0)),
            bot(2, Some((2, at(2, 9))), at(0, 0)),
        ];
        inbox.observe(&bots);
        assert_eq!(inbox.alerts(&bots, nobody).len(), 2);
        assert!(inbox.settle(&bots).is_empty(), "still unread");

        assert!(inbox.saw(&bots[0], at(2, 9)));
        assert_eq!(inbox.settle(&bots), [Uuid::from_u128(1)]);
        assert_eq!(
            inbox.settle(std::slice::from_ref(&bots[0])),
            [Uuid::from_u128(2)],
            "deleted"
        );
        assert_eq!(inbox.shown().count(), 0);
    }

    #[test]
    fn reads_merge_while_one_request_is_in_flight() {
        let mut inbox = Inbox::default();
        let id = Uuid::from_u128(1);
        let b = bot(1, Some((1, at(3, 40))), at(0, 0));
        assert!(inbox.saw(&b, at(3, 10)));
        assert_eq!(
            inbox.next_command(id),
            Some(ReadCommand::Through {
                position: at(3, 10)
            })
        );
        assert!(inbox.saw(&b, at(3, 20)));
        assert!(inbox.saw(&b, at(3, 40)));
        assert!(!inbox.saw(&b, at(3, 30)), "never backwards");
        assert_eq!(inbox.next_command(id), None, "one request at a time");
        inbox.finished(id, Outcome::Done);
        assert_eq!(
            inbox.next_command(id),
            Some(ReadCommand::Through {
                position: at(3, 40)
            })
        );
        inbox.finished(id, Outcome::Done);
        assert_eq!(inbox.next_command(id), None);
        assert!(!inbox.is_unread(&b));
    }

    #[test]
    fn a_late_read_never_overtakes_marking_unread() {
        let mut inbox = Inbox::default();
        let id = Uuid::from_u128(1);
        inbox.open(id);
        let b = bot(1, Some((1, at(3, 40))), at(0, 0));
        inbox.saw(&b, at(3, 40));
        assert!(matches!(
            inbox.next_command(id),
            Some(ReadCommand::Through { .. })
        ));
        inbox.mark(&b, Mark::Unread);
        let read = bot(1, Some((1, at(3, 40))), at(3, 40));
        assert!(inbox.is_unread(&read));
        inbox.finished(id, Outcome::Done);
        assert!(inbox.is_unread(&read), "the read landed first");
        inbox.saw(&read, at(3, 50));
        assert_eq!(inbox.next_command(id), Some(ReadCommand::MarkUnread));
        inbox.finished(id, Outcome::Done);
        assert_eq!(
            inbox.next_command(id),
            Some(ReadCommand::Through {
                position: at(3, 50)
            }),
            "seeing a held bot never clears its mark"
        );
        inbox.finished(id, Outcome::Done);
        assert!(inbox.is_unread(&flagged(bot(1, Some((1, at(3, 50))), at(3, 50)))));
    }

    #[test]
    fn opening_a_marked_bot_again_clears_the_mark_once_its_replies_are_seen() {
        let mut inbox = Inbox::default();
        let id = Uuid::from_u128(1);
        let b = flagged(bot(1, Some((1, at(3, 40))), at(3, 40)));
        assert!(!inbox.saw(&b, at(3, 40)), "selected, not opened");
        assert!(inbox.is_unread(&b));
        inbox.open(id);
        assert!(inbox.is_unread(&b), "opening alone is not reading");
        assert!(inbox.saw(&b, at(3, 40)));
        assert!(!inbox.is_unread(&b));
        assert!(matches!(
            inbox.next_command(id),
            Some(ReadCommand::MarkRead { .. })
        ));
    }

    #[test]
    fn a_retried_mark_is_sent_again_and_an_empty_bot_can_be_marked_read() {
        let mut inbox = Inbox::default();
        let id = Uuid::from_u128(1);
        let empty = bot(1, None, at(0, 0));
        inbox.mark(&empty, Mark::Unread);
        assert!(inbox.is_unread(&empty));
        assert_eq!(inbox.next_command(id), Some(ReadCommand::MarkUnread));
        inbox.finished(id, Outcome::Retry);
        assert_eq!(inbox.next_command(id), Some(ReadCommand::MarkUnread));
        inbox.finished(id, Outcome::Done);
        inbox.mark(&flagged(empty.clone()), Mark::Read);
        assert_eq!(
            inbox.next_command(id),
            Some(ReadCommand::MarkRead { position: None })
        );
        assert!(!inbox.is_unread(&flagged(empty)));
    }

    #[test]
    fn marking_read_uses_the_latest_known_reply() {
        let mut inbox = Inbox::default();
        let id = Uuid::from_u128(1);
        let b = bot(1, Some((1, at(5, 12))), at(1, 1));
        inbox.mark(&b, Mark::Read);
        assert_eq!(
            inbox.next_command(id),
            Some(ReadCommand::MarkRead {
                position: Some(at(5, 12))
            })
        );
    }

    #[test]
    fn servers_without_read_state_show_nothing_unread() {
        let mut inbox = Inbox::default();
        let mut old = bot(1, None, at(0, 0));
        old.read_state = None;
        inbox.observe(std::slice::from_ref(&old));
        assert!(!inbox.observe(std::slice::from_ref(&old)));
        assert!(!inbox.is_unread(&old));
    }

    #[test]
    fn a_rejected_read_restores_unread_without_retrying_the_same_text() {
        let mut inbox = Inbox::default();
        let waiting = bot(1, Some((1, at(3, 40))), at(0, 0));
        assert!(inbox.saw(&waiting, at(3, 40)));
        assert!(inbox.next_command(waiting.id).is_some());
        assert!(!inbox.is_unread(&waiting));
        inbox.finished(waiting.id, Outcome::Refused);
        assert!(inbox.is_unread(&waiting));
        assert!(!inbox.saw(&waiting, at(3, 40)));
        assert_eq!(inbox.next_command(waiting.id), None);

        assert!(inbox.saw(&waiting, at(3, 50)));
        assert_eq!(
            inbox.next_command(waiting.id),
            Some(ReadCommand::Through {
                position: at(3, 50)
            })
        );
    }

    #[test]
    fn a_rejected_manual_read_restores_the_server_mark() {
        let mut inbox = Inbox::default();
        let waiting = flagged(bot(1, Some((1, at(3, 40))), at(0, 0)));
        inbox.mark(&waiting, Mark::Read);
        assert!(inbox.next_command(waiting.id).is_some());
        assert!(!inbox.is_unread(&waiting));
        inbox.finished(waiting.id, Outcome::Refused);
        assert!(inbox.is_unread(&waiting));
        assert_eq!(inbox.seen_through(&waiting), at(0, 0));
        assert_eq!(inbox.next_command(waiting.id), None);
    }

    #[test]
    fn a_rejected_read_keeps_newer_pending_progress() {
        let mut inbox = Inbox::default();
        let waiting = bot(1, Some((1, at(3, 40))), at(0, 0));
        inbox.saw(&waiting, at(3, 20));
        inbox.next_command(waiting.id).unwrap();
        inbox.saw(&waiting, at(3, 40));
        inbox.finished(waiting.id, Outcome::Refused);
        assert_eq!(
            inbox.next_command(waiting.id),
            Some(ReadCommand::Through {
                position: at(3, 40)
            })
        );
    }

    #[test]
    fn seeing_an_open_chat_does_not_erase_a_later_manual_mark() {
        let mut inbox = Inbox::default();
        let read = bot(1, Some((1, at(3, 40))), at(3, 40));
        inbox.observe(std::slice::from_ref(&read));
        inbox.open(read.id);
        assert!(!inbox.saw(&read, at(3, 40)));

        let marked_elsewhere = flagged(read);
        inbox.observe(std::slice::from_ref(&marked_elsewhere));
        assert!(!inbox.saw(&marked_elsewhere, at(3, 40)));
        assert_eq!(inbox.next_command(marked_elsewhere.id), None);
        assert!(inbox.is_unread(&marked_elsewhere));

        inbox.open(marked_elsewhere.id);
        assert!(inbox.saw(&marked_elsewhere, at(3, 40)));
        assert!(matches!(
            inbox.next_command(marked_elsewhere.id),
            Some(ReadCommand::MarkRead { .. })
        ));
    }

    #[test]
    fn tags_name_their_connection_and_bot() {
        let (scope, bot) = (Uuid::from_u128(7), Uuid::from_u128(8));
        assert_eq!(parse_tag(&tag(scope, bot)), Some((scope, bot)));
        for foreign in ["", "else-bot:", "else-bot:x:y", "other:1:2"] {
            assert_eq!(parse_tag(foreign), None, "{foreign}");
        }
    }
}
