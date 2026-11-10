//! What the owner has read, and which new replies deserve a system
//! notification. Pure state, owned by the messaging view; the server keeps
//! the durable read position and this keeps only what is in flight.
//!
//! Reads go to the server one request per bot at a time. Later wishes are
//! merged into the next request instead of queued: positions only move
//! forward, and of two explicit marks the later one wins.

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
            let Some(latest) = bot
                .read_state
                .as_ref()
                .and_then(|state| state.latest.as_ref())
            else {
                continue;
            };
            let attention = self.bots.entry(bot.id).or_default();
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
                body: latest.preview.clone(),
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
                .filter(|alert| alert.shown)
                .map(|_| *id)
        })
    }
}

/// The bot has nothing left to run, or waits on the owner. Replies before
/// then are progress updates and stay silent; a turn continuing after its
/// delegated tasks is queued, so it is not idle.
fn handed_back(bot: &Bot) -> bool {
    bot.activity.as_ref().is_none_or(|a| a.phase == "idle")
        || matches!(
            bot.sidebar_status,
            Some(
                SidebarStatus::WaitingForAnswer { .. }
                    | SidebarStatus::NeedsApproval { .. }
                    | SidebarStatus::NeedsCredentials { .. }
            )
        )
}

fn unread(sync: Option<&ReadSync>, bot: &Bot) -> bool {
    if bot.muted {
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

    fn busy(mut bot: Bot, phase: &str) -> Bot {
        bot.activity = Some(crate::api::Activity {
            phase: phase.into(),
            active_turn_id: (phase == "working").then(|| turn(1)),
            queued_count: u32::from(phase == "queued"),
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
        inbox.observe(&[busy(bot(1, None, at(0, 0)), "working")]);
        for (position, phase) in [
            (at(2, 7), "working"),
            (at(3, 12), "working"),
            (at(3, 12), "queued"),
        ] {
            let update = busy(bot(1, Some((1, position)), at(0, 0)), phase);
            assert!(!inbox.observe(std::slice::from_ref(&update)), "{phase}");
            assert!(inbox.alerts(&[update], nobody).is_empty(), "{phase}");
        }
        let done = finished(bot(1, Some((1, at(3, 12))), at(0, 0)), "Final");
        assert!(
            inbox.observe(std::slice::from_ref(&done)),
            "the bot went idle"
        );
        assert_eq!(
            inbox.alerts(std::slice::from_ref(&done), nobody),
            [Alert {
                bot: Uuid::from_u128(1),
                title: "Bot 1".into(),
                body: "Final".into(),
            }]
        );
        assert!(!inbox.observe(&[done]), "announced once");
    }

    #[test]
    fn a_question_is_announced_and_so_is_the_answer_it_led_to() {
        let mut inbox = Inbox::default();
        inbox.observe(&[busy(bot(1, None, at(0, 0)), "working")]);
        let mut asking = busy(bot(1, Some((1, at(2, 7))), at(0, 0)), "working");
        asking.sidebar_status = Some(crate::api::SidebarStatus::WaitingForAnswer {
            preview: "Which one?".into(),
        });
        assert!(inbox.observe(std::slice::from_ref(&asking)));
        assert_eq!(inbox.alerts(&[asking], nobody).len(), 1);
        let resumed = busy(bot(1, Some((1, at(4, 9))), at(0, 0)), "working");
        assert!(!inbox.observe(std::slice::from_ref(&resumed)));
        let done = bot(1, Some((1, at(4, 9))), at(0, 0));
        assert!(
            inbox.observe(std::slice::from_ref(&done)),
            "same turn, new reply"
        );
        assert_eq!(inbox.alerts(&[done], nobody).len(), 1);
    }

    #[test]
    fn a_turn_running_at_startup_is_announced_when_it_finishes() {
        let mut inbox = Inbox::default();
        assert!(!inbox.observe(&[busy(bot(1, Some((1, at(2, 7))), at(0, 0)), "working")]));
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
