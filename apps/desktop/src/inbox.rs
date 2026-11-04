//! What the owner has read, and which new replies deserve a system
//! notification. Pure state, owned by the messaging view; the server keeps
//! the durable read position and this keeps only what is in flight.
//!
//! Reads go to the server one request per bot at a time. Later wishes are
//! merged into the next request instead of queued: positions only move
//! forward, and of two explicit marks the later one wins.

use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::api::{Bot, ReadCommand, ReplyPosition};

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
struct ReadSync {
    /// The furthest this client has seen.
    through: ReplyPosition,
    /// The furthest the server confirmed.
    confirmed: ReplyPosition,
    /// An explicit mark the server has not confirmed yet.
    mark: Option<Mark>,
    mark_unsent: bool,
    inflight: Option<ReadCommand>,
}

impl ReadSync {
    fn set_mark(&mut self, mark: Mark) {
        self.mark = Some(mark);
        self.mark_unsent = true;
    }

    fn next(&mut self) -> Option<ReadCommand> {
        if self.inflight.is_some() {
            return None;
        }
        let command = match self.mark.filter(|_| self.mark_unsent) {
            Some(Mark::Unread) => ReadCommand::MarkUnread,
            Some(Mark::Read) => ReadCommand::MarkRead {
                position: (self.through > ReplyPosition::default()).then_some(self.through),
            },
            None if self.through > self.confirmed => ReadCommand::Through {
                position: self.through,
            },
            None => return None,
        };
        self.mark_unsent = false;
        self.inflight = Some(command);
        Some(command)
    }

    fn finished(&mut self, outcome: Outcome) {
        let Some(command) = self.inflight.take() else {
            return;
        };
        let position = match command {
            ReadCommand::Through { position } => Some(position),
            ReadCommand::MarkRead { position } => position,
            ReadCommand::MarkUnread => None,
        };
        let explicit = !matches!(command, ReadCommand::Through { .. });
        match outcome {
            Outcome::Retry => {
                if explicit && self.mark.is_some() {
                    self.mark_unsent = true;
                }
            }
            Outcome::Done | Outcome::Refused => {
                if let Some(position) = position {
                    self.confirmed = self.confirmed.max(position);
                }
                if explicit && !self.mark_unsent {
                    self.mark = None;
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    pub bot: Uuid,
    pub title: String,
    pub body: String,
}

#[derive(Debug, Default)]
pub struct Inbox {
    reads: HashMap<Uuid, ReadSync>,
    /// The bot the owner explicitly opened. Its explicit unread mark is
    /// cleared once its replies are seen; any other is kept.
    opened: Option<Uuid>,
    /// Whether the first bot list has been seen: what it shows is restored,
    /// never announced.
    baseline: bool,
    /// The latest reply position seen per bot.
    heads: HashMap<Uuid, ReplyPosition>,
    /// Bots with new replies awaiting a notification decision.
    due: HashSet<Uuid>,
    /// The turn last announced per bot, and whether its notification is up.
    alerted: HashMap<Uuid, (Uuid, bool)>,
}

impl Inbox {
    pub fn is_unread(&self, bot: &Bot) -> bool {
        unread(&self.reads, bot)
    }

    fn is_marked(&self, bot: &Bot) -> bool {
        match self.reads.get(&bot.id).and_then(|s| s.mark) {
            Some(mark) => mark == Mark::Unread,
            None => bot.read_state.as_ref().is_some_and(|s| s.marked_unread),
        }
    }

    /// The owner opened this bot: seeing its replies now clears its mark.
    pub fn open(&mut self, bot: Uuid) {
        self.opened = Some(bot);
    }

    /// The owner's explicit choice. Marking the open bot unread keeps it
    /// unread until it is opened again.
    pub fn mark(&mut self, bot: &Bot, mark: Mark) {
        if mark == Mark::Unread && self.opened == Some(bot.id) {
            self.opened = None;
        }
        let sync = self.reads.entry(bot.id).or_default();
        if mark == Mark::Read
            && let Some(latest) = bot.read_state.as_ref().and_then(|s| s.latest.as_ref())
        {
            sync.through = sync.through.max(latest.position);
        }
        sync.set_mark(mark);
    }

    /// The owner saw the bot's replies up to `position`. Returns whether
    /// that changed anything to send.
    pub fn saw(&mut self, bot: &Bot, position: ReplyPosition) -> bool {
        let clear = self.opened == Some(bot.id) && self.is_marked(bot);
        let known = self.seen_through(bot);
        let sync = self.reads.entry(bot.id).or_default();
        let mut changed = false;
        if position > known {
            sync.through = position;
            changed = true;
        }
        if clear {
            sync.set_mark(Mark::Read);
            changed = true;
        }
        changed
    }

    /// The furthest seen here or saved on the server.
    pub fn seen_through(&self, bot: &Bot) -> ReplyPosition {
        let local = self
            .reads
            .get(&bot.id)
            .map(|s| s.through)
            .unwrap_or_default();
        bot.read_state
            .as_ref()
            .map_or(local, |s| s.read_through.max(local))
    }

    /// The request to send for `bot` now, if any and none is in flight.
    pub fn next_command(&mut self, bot: Uuid) -> Option<ReadCommand> {
        self.reads.get_mut(&bot)?.next()
    }

    pub fn finished(&mut self, bot: Uuid, outcome: Outcome) {
        if let Some(sync) = self.reads.get_mut(&bot) {
            sync.finished(outcome);
        }
    }

    /// Note new replies in a bot list. Returns whether any await a
    /// notification decision.
    pub fn observe(&mut self, bots: &[Bot]) -> bool {
        let first = !self.baseline;
        self.baseline = true;
        let live: HashSet<Uuid> = bots
            .iter()
            .filter(|b| b.is_active())
            .map(|b| b.id)
            .collect();
        self.heads.retain(|id, _| live.contains(id));
        self.due.retain(|id| live.contains(id));
        self.reads
            .retain(|id, sync| live.contains(id) || sync.inflight.is_some());
        for bot in bots.iter().filter(|b| b.is_active()) {
            let Some(latest) = bot.read_state.as_ref().and_then(|s| s.latest.as_ref()) else {
                continue;
            };
            let before = self.heads.insert(bot.id, latest.position);
            let announced = self
                .alerted
                .get(&bot.id)
                .is_some_and(|(turn, _)| *turn == latest.turn_id);
            if !first && before.is_none_or(|p| latest.position > p) && !announced {
                self.due.insert(bot.id);
            }
        }
        !self.due.is_empty()
    }

    /// Decide the due notifications. `viewing` says whether the owner is
    /// looking at that bot's conversation right now.
    pub fn alerts(&mut self, bots: &[Bot], viewing: impl Fn(Uuid) -> bool) -> Vec<Alert> {
        let mut alerts = Vec::new();
        for id in std::mem::take(&mut self.due) {
            let Some(bot) = bots.iter().find(|b| b.id == id && b.is_active()) else {
                continue;
            };
            let Some(latest) = bot.read_state.as_ref().and_then(|s| s.latest.as_ref()) else {
                continue;
            };
            if !self.is_unread(bot) || viewing(id) {
                continue;
            }
            self.alerted.insert(id, (latest.turn_id, true));
            alerts.push(Alert {
                bot: id,
                title: bot.name.clone(),
                body: latest.preview.clone(),
            });
        }
        alerts.sort_by_key(|a| a.bot);
        alerts
    }

    /// Notifications to take down: their bot was read, deleted, or is gone.
    pub fn settle(&mut self, bots: &[Bot]) -> Vec<Uuid> {
        let mut gone = Vec::new();
        for (id, (_, shown)) in &mut self.alerted {
            let unread = bots
                .iter()
                .find(|b| b.id == *id && b.is_active())
                .is_some_and(|b| unread(&self.reads, b));
            if *shown && !unread {
                *shown = false;
                gone.push(*id);
            }
        }
        self.alerted
            .retain(|id, _| bots.iter().any(|b| b.id == *id && b.is_active()));
        gone.sort();
        gone
    }

    /// Bots whose notification is still up.
    pub fn shown(&self) -> impl Iterator<Item = Uuid> + '_ {
        self.alerted
            .iter()
            .filter(|(_, (_, shown))| *shown)
            .map(|(id, _)| *id)
    }
}

fn unread(reads: &HashMap<Uuid, ReadSync>, bot: &Bot) -> bool {
    let sync = reads.get(&bot.id);
    let mark = sync.and_then(|s| s.mark);
    let Some(state) = bot.read_state.as_ref().filter(|_| bot.is_active()) else {
        return mark == Some(Mark::Unread);
    };
    let marked = mark.map_or(state.marked_unread, |m| m == Mark::Unread);
    let through = state
        .read_through
        .max(sync.map(|s| s.through).unwrap_or_default());
    marked || state.latest.as_ref().is_some_and(|l| l.position > through)
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

    /// Bot `id` whose latest reply belongs to `reply` and is at `latest`,
    /// read through `read`.
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

    #[test]
    fn a_streamed_reply_is_announced_once() {
        let mut inbox = Inbox::default();
        inbox.observe(&[bot(1, None, at(0, 0))]);
        let started = bot(1, Some((1, at(2, 7))), at(0, 0));
        assert!(inbox.observe(std::slice::from_ref(&started)));
        let alerts = inbox.alerts(&[started], nobody);
        assert_eq!(
            alerts,
            [Alert {
                bot: Uuid::from_u128(1),
                title: "Bot 1".into(),
                body: "Reply 1".into(),
            }]
        );
        for bytes in [14, 21, 80] {
            let more = bot(1, Some((1, at(2, bytes))), at(0, 0));
            assert!(
                !inbox.observe(std::slice::from_ref(&more)),
                "one alert per reply"
            );
            assert!(inbox.alerts(&[more], nobody).is_empty());
        }
        let second_message = bot(1, Some((1, at(3, 5))), at(0, 0));
        assert!(!inbox.observe(&[second_message]), "same turn");
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
        // The server's list still predates the mark.
        let read = bot(1, Some((1, at(3, 40))), at(3, 40));
        assert!(inbox.is_unread(&read));
        inbox.finished(id, Outcome::Done);
        assert!(inbox.is_unread(&read), "the read landed first");
        // The open conversation keeps being seen, and more text arrives.
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
    fn tags_name_their_connection_and_bot() {
        let (scope, bot) = (Uuid::from_u128(7), Uuid::from_u128(8));
        assert_eq!(parse_tag(&tag(scope, bot)), Some((scope, bot)));
        for foreign in ["", "else-bot:", "else-bot:x:y", "other:1:2"] {
            assert_eq!(parse_tag(foreign), None, "{foreign}");
        }
    }
}
