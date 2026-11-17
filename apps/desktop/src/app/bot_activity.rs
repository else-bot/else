//! Bot-to-bot exchanges read inline. Each task is a block: a header naming
//! who replied to whom, the reply as an excerpt, and the owner's asks. The
//! header folds the task text, which is long and machine-written. A block
//! that needs nothing from the owner sits on one line until opened.

use super::action_card::{Disclosure, EVENT_INDENT, EventEnd};
use super::conversation::{ReplyLayout, Reveal, Spot, TranscriptRow};
use super::{ElseApp, RowKey};
use crate::controls::{Control as _, Scale, Tier};
use crate::model::{
    BotCreated, BotReplyStatus, Conversation, ExchangeAsk, Item, OwnerNeed, OwnerNoticeStatus,
};
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Theme, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

/// The most of a reply an exchange shows before "Show more".
const EXCERPT_LINES: usize = 8;
const EXCERPT_CHARS: usize = 600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BotActivity {
    /// The turn that holds this exchange. Separate turns are separate exchanges.
    turn: Uuid,
    pub(super) items: Vec<Uuid>,
    /// This bot's replies to the tasks it received, shown inside their block.
    pub(super) answers: Vec<Uuid>,
}

impl BotActivity {
    pub(super) fn contains(&self, id: Uuid) -> bool {
        self.items.contains(&id) || self.answers.contains(&id)
    }
}

/// How the exchange `item` ended: a delegated bot's reply carries it, and a
/// received task takes it from the turn that ran it.
fn outcome(item: &Item, conversation: &Conversation) -> Option<BotReplyStatus> {
    if let Some(reply) = &item.bot_reply {
        return Some(reply.status);
    }
    item.bot_task.as_ref()?;
    match conversation.turn(item.turn_id)?.status.as_str() {
        "failed" => Some(BotReplyStatus::Failed),
        "interrupted" => Some(BotReplyStatus::Cancelled),
        _ => None,
    }
}

/// The owner notices an exchange carries, each in its latest state, oldest
/// first. A received task has none here: its card is its own row.
fn exchange_asks<'a>(items: impl IntoIterator<Item = &'a Item>) -> Vec<&'a ExchangeAsk> {
    let mut asks: Vec<&ExchangeAsk> = Vec::new();
    for ask in items
        .into_iter()
        .filter_map(|item| item.bot_reply.as_ref())
        .flat_map(|reply| &reply.asks)
    {
        match asks
            .iter_mut()
            .find(|known| known.notice.id == ask.notice.id)
        {
            Some(known) => *known = ask,
            None => asks.push(ask),
        }
    }
    asks
}

fn has_pending_ask(item: &Item) -> bool {
    exchange_asks([item])
        .iter()
        .any(|ask| ask.notice.status == OwnerNoticeStatus::Pending)
}

/// What the task behind `item` declared it needs from the owner.
fn declared_need(item: &Item) -> Option<&OwnerNeed> {
    item.bot_reply
        .as_ref()
        .map(|reply| &reply.declared)
        .or_else(|| item.bot_task.as_ref().map(|task| &task.declared))?
        .0
        .as_ref()
}

fn task_title(item: &Item) -> Option<&str> {
    item.bot_reply
        .as_ref()
        .and_then(|reply| reply.title.as_deref())
        .or_else(|| item.bot_task.as_ref()?.title.as_deref())
        .filter(|title| !title.trim().is_empty())
}

fn waits_on_owner(conversation: &Conversation, turn: Uuid) -> bool {
    conversation.turn(turn).is_some_and(|turn| {
        turn.owner_notices
            .iter()
            .any(|n| n.status == OwnerNoticeStatus::Pending)
    })
}

/// `text` cut to what an exchange shows before "Show more", or `None` when
/// it all fits. The cut falls at the end of a paragraph or list item where
/// one fits, so the excerpt is real markdown and holds no hidden text.
pub(super) fn excerpt(text: &str) -> Option<String> {
    let text = text.trim_end();
    let filled = |lines: usize, chars: usize| lines > EXCERPT_LINES || chars > EXCERPT_CHARS;
    let all_lines = text.lines().filter(|line| !line.trim().is_empty()).count();
    if !filled(all_lines, text.chars().count()) {
        return None;
    }
    let (mut lines, mut chars, mut fenced) = (0, 0, false);
    let mut block_end = None;
    let mut line_end = None;
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let blank = body.trim().is_empty();
        if filled(lines + usize::from(!blank), chars + body.chars().count()) {
            break;
        }
        lines += usize::from(!blank);
        chars += body.chars().count();
        if body.trim_start().starts_with("```") {
            fenced = !fenced;
        }
        let end = start + body.len();
        start += line.len();
        if blank {
            continue;
        }
        line_end = Some((end, fenced));
        let next = text[start..].lines().next();
        if !fenced && next.is_none_or(|next| next.trim().is_empty() || list_item(next)) {
            block_end = Some(end);
        }
    }
    Some(match (block_end, line_end) {
        (Some(end), _) => text[..end].to_owned(),
        (None, Some((end, true))) => format!("{}\n```", &text[..end]),
        (None, Some((end, false))) => format!("{}…", &text[..end]),
        // The first line alone is over the budget: cut it between words.
        (None, None) => {
            let limit = text
                .char_indices()
                .nth(EXCERPT_CHARS)
                .map_or(text.len(), |(at, _)| at);
            let cut = text[..limit]
                .rfind(char::is_whitespace)
                .filter(|&at| at > 0)
                .unwrap_or(limit);
            format!("{}…", text[..cut].trim_end())
        }
    })
}

fn list_item(line: &str) -> bool {
    let line = line.trim_start();
    line.starts_with("- ")
        || line.starts_with("* ")
        || line.starts_with("+ ")
        || line.split_once(". ").is_some_and(|(number, _)| {
            !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())
        })
}

pub(super) fn group_activity(
    rows: Vec<TranscriptRow>,
    conversation: Option<&Conversation>,
) -> Vec<TranscriptRow> {
    let mut grouped: Vec<TranscriptRow> = Vec::new();
    for mut row in rows {
        let item = match row.key {
            RowKey::Item(id) => conversation.and_then(|c| c.item(id)),
            _ => None,
        };
        let (Some(item), Some(conversation)) = (
            item.filter(|item| item.bot_task.is_some() || item.bot_reply.is_some()),
            conversation,
        ) else {
            grouped.push(row);
            continue;
        };
        let activity = BotActivity {
            turn: item.turn_id,
            items: vec![item.id],
            answers: conversation
                .task_answers(item)
                .iter()
                .map(|answer| answer.id)
                .collect(),
        };
        if !row.unread_start
            && let Some(previous) = grouped.last_mut()
            && let Some(group) = previous.activity.as_mut()
            && group.turn == activity.turn
        {
            group.items.extend(activity.items);
            for answer in activity.answers {
                if !group.answers.contains(&answer) {
                    group.answers.push(answer);
                }
            }
            previous.footer_stamp = row.footer_stamp;
            if let (Some(previous), Some(current)) = (previous.routine.as_mut(), row.routine) {
                previous.last = current.last;
            }
        } else {
            row.activity = Some(activity);
            grouped.push(row);
        }
    }
    grouped
}

/// Which end of an exchange this chat's bot is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// It received the task, so its own messages are the reply.
    Received,
    /// It sent the task, and the reply is another bot's words.
    Delegated,
}

/// One task of an exchange row, as the owner reads it.
struct Block<'a> {
    item: &'a Item,
    side: Side,
    /// The other bot: the task's sender, or the bot that replied.
    peer: (Uuid, &'a str),
    status: ActivityStatus,
    /// The reply: this bot's answers, or the delegated bot's reply item.
    replies: Vec<&'a Item>,
}

impl<'a> Block<'a> {
    fn of(item: &'a Item, c: &'a Conversation) -> Option<Self> {
        let status = block_status(item, c);
        if let Some(task) = &item.bot_task {
            // Two receipts in one turn share its answers; the first keeps them.
            let first = c
                .task_receipt(item.turn_id)
                .is_some_and(|r| r.id == item.id);
            return Some(Self {
                item,
                side: Side::Received,
                peer: (task.sender_bot_id, task.sender_name.as_str()),
                status,
                replies: if first {
                    c.task_answers(item)
                } else {
                    Vec::new()
                },
            });
        }
        let reply = item.bot_reply.as_ref()?;
        Some(Self {
            item,
            side: Side::Delegated,
            peer: (reply.sender_bot_id, reply.sender_name.as_str()),
            status,
            replies: vec![item],
        })
    }

    /// Whether the block shows its reply until the owner folds it: it needs
    /// the owner, went wrong, or is this bot's own finished reply.
    fn opens(&self) -> bool {
        match self.status {
            ActivityStatus::NeedsYou | ActivityStatus::Failed | ActivityStatus::Stopped => true,
            ActivityStatus::Fyi | ActivityStatus::Replied => self.side == Side::Received,
            _ => false,
        }
    }

    fn task(&self) -> &'a str {
        match (&self.item.bot_task, &self.item.bot_reply) {
            (Some(task), _) => task.prompt.as_str(),
            (None, Some(reply)) => reply.prompt.as_str(),
            (None, None) => "",
        }
    }

    fn owner_line(&self) -> Option<&'a str> {
        match declared_need(self.item)? {
            OwnerNeed::Fyi(line) => Some(line.as_str()),
            _ => None,
        }
    }

    /// The reply's text on one line: the line the bot wrote for the owner
    /// when it wrote one, else the start of its reply.
    fn preview(&self) -> String {
        if let Some(line) = self.owner_line() {
            return format!("\u{201c}{line}\u{201d}");
        }
        let text = self
            .replies
            .iter()
            .map(|reply| reply.text.as_str())
            .find(|text| !text.trim().is_empty());
        match (text, self.status.settled()) {
            (Some(text), _) => super::reply::flatten(text),
            (None, true) => "No reply text.".to_owned(),
            (None, false) => "No reply yet.".to_owned(),
        }
    }

    /// Why the task failed or stopped, apart from what the bot wrote.
    fn reason(&self, c: &'a Conversation) -> Option<&'a str> {
        if !matches!(
            self.status,
            ActivityStatus::Failed | ActivityStatus::Stopped
        ) {
            return None;
        }
        let reason = match self.side {
            Side::Received => c
                .turn(self.item.turn_id)?
                .error
                .as_ref()?
                .message
                .as_deref(),
            Side::Delegated => self.item.bot_reply.as_ref()?.reason.as_deref(),
        };
        reason?.lines().map(str::trim).find(|line| !line.is_empty())
    }
}

fn block_status(item: &Item, c: &Conversation) -> ActivityStatus {
    // A received task's own asks sit in the turn that ran it.
    let received = item.bot_task.is_some();
    if has_pending_ask(item) || (received && waits_on_owner(c, item.turn_id)) {
        return ActivityStatus::NeedsYou;
    }
    match outcome(item, c) {
        Some(BotReplyStatus::Failed) => return ActivityStatus::Failed,
        Some(BotReplyStatus::Cancelled) => return ActivityStatus::Stopped,
        _ => {}
    }
    let task_status = received
        .then(|| c.turn(item.turn_id))
        .flatten()
        .map(|turn| turn.status.as_str());
    if item.bot_reply.is_some() || task_status == Some("completed") {
        return match declared_need(item) {
            Some(OwnerNeed::Fyi(_)) => ActivityStatus::Fyi,
            Some(OwnerNeed::Nothing) => ActivityStatus::NothingForYou,
            // A settled or superseded ask, or nothing declared.
            _ => ActivityStatus::Replied,
        };
    }
    match task_status {
        None | Some("queued") => ActivityStatus::Received,
        Some("uncertain") => ActivityStatus::Uncertain,
        Some("waiting") => ActivityStatus::Waiting,
        _ => ActivityStatus::Working,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivityStatus {
    NeedsYou,
    Failed,
    Stopped,
    /// Replied, and the task declared one line for the owner.
    Fyi,
    /// Replied, and the task declared it needs nothing from the owner.
    NothingForYou,
    Replied,
    Received,
    Uncertain,
    Waiting,
    Working,
}

impl ActivityStatus {
    fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
            Self::Fyi => "FYI",
            Self::NothingForYou => "Nothing for you",
            Self::Replied => "Replied",
            Self::Received => "Task received",
            Self::Uncertain => "Unknown result",
            Self::Waiting => "Waiting",
            Self::Working => "Working",
        }
    }

    /// Whether the task is over, so a missing reply will not arrive.
    fn settled(self) -> bool {
        !matches!(
            self,
            Self::Received | Self::Uncertain | Self::Waiting | Self::Working
        )
    }

    fn end(self, theme: &Theme) -> EventEnd {
        let (color, dot) = match self {
            Self::NeedsYou => (theme.primary, true),
            Self::Failed => (theme.danger, false),
            _ => (super::message_row::meta_color(theme), false),
        };
        EventEnd::Status {
            label: self.label().into(),
            color,
            dot,
        }
    }
}

impl ElseApp {
    fn exchange_blocks<'a>(&'a self, activity: &BotActivity) -> Vec<Block<'a>> {
        let Some(c) = self.view.as_ref().and_then(|v| v.conversation.as_ref()) else {
            return Vec::new();
        };
        activity
            .items
            .iter()
            .filter_map(|id| Block::of(c.item(*id)?, c))
            .collect()
    }

    fn exchange_open(&self, block: &Block) -> bool {
        block.opens()
            || self
                .view
                .as_ref()
                .is_some_and(|view| view.opened_exchanges.contains(&block.item.id))
    }

    /// Whether every block of `activity` sits on one line, so the row
    /// spaces like the other one-line activity rows.
    pub(super) fn exchange_is_line(&self, activity: &BotActivity) -> bool {
        self.exchange_blocks(activity)
            .iter()
            .all(|block| !self.exchange_open(block))
    }

    /// Applies a fold change to the exchange holding `item`. Its row's
    /// height changes, and so does the spacing of the row below.
    fn refold_exchange(&mut self, item: Uuid, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_rows(window, cx);
        if let Some(view) = self.view.as_ref()
            && let Some(ix) = view.row_index(RowKey::Item(item))
        {
            view.list.remeasure_items(ix..(ix + 2).min(view.rows.len()));
        }
        cx.notify();
    }

    /// Opens the exchange holding `item` with its task showing, and scrolls
    /// to it.
    pub(super) fn show_exchange_task(
        &mut self,
        item: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        view.opened_exchanges.insert(item);
        view.expanded_events.insert(item);
        self.refold_exchange(item, window, cx);
        let Some(view) = self.view.as_ref() else {
            return;
        };
        let Some(ix) = view.row_index(RowKey::Item(item)) else {
            return;
        };
        view.list.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: px(0.),
        });
        let row = view.rows[ix].key;
        self.start_reveal(Reveal::new(row, Spot::Row), cx);
    }

    pub(super) fn render_bot_activity(
        &mut self,
        row: &TranscriptRow,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let conversation = view.conversation.as_ref()?;
        let activity = row.activity.as_ref()?;
        let me = self
            .bots
            .iter()
            .find(|bot| bot.id == view.bot_id)
            .map_or((view.bot_id, "This bot"), |bot| (bot.id, bot.name.as_str()));
        let blocks = self.exchange_blocks(activity);
        if blocks.is_empty() {
            return None;
        }
        let mut column = v_flex().w_full().min_w_0().gap_1();
        for block in &blocks {
            column = column.child(if self.exchange_open(block) {
                self.render_exchange_block(block, me, conversation, cx)
            } else {
                self.render_exchange_line(block, me, cx)
            });
        }
        if let Some(latest) = conversation
            .latest_reply()
            .filter(|reply| activity.answers.contains(&reply.id))
        {
            let owner = cx.entity().downgrade();
            let bot = view.bot_id;
            let position = crate::api::ReplyPosition {
                item_seq: latest.seq,
                text_bytes: latest.text.len() as i64,
            };
            column = column.on_prepaint(move |bounds, window, cx| {
                window.defer(cx, move |window, cx| {
                    let _ = owner.update(cx, |app, cx| {
                        app.reply_laid_out(bot, ReplyLayout { position, bounds }, window, cx)
                    });
                });
            });
        }
        Some(column.into_any_element())
    }

    /// Who speaks in a block, and how its header names the exchange:
    /// "Replying to Scout" over this bot's reply, "Reply from Marketing"
    /// over another bot's.
    fn exchange_voice<'a>(block: &Block<'a>, me: (Uuid, &'a str)) -> ((Uuid, &'a str), String) {
        match block.side {
            Side::Received => (me, format!("Replying to {}", block.peer.1)),
            Side::Delegated => (block.peer, format!("Reply from {}", block.peer.1)),
        }
    }

    fn exchange_label(block: &Block, me: (Uuid, &str)) -> String {
        let to = match block.side {
            Side::Received => block.peer.1,
            Side::Delegated => me.1,
        };
        let speaker = Self::exchange_voice(block, me).0.1;
        let title = task_title(block.item).map_or_else(String::new, |t| format!(", {t}"));
        format!(
            "{speaker} replying to {to}{title}, {}",
            block.status.label()
        )
    }

    fn render_exchange_line(
        &self,
        block: &Block,
        me: (Uuid, &str),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let id = block.item.id;
        let ((speaker, _), verb) = Self::exchange_voice(block, me);
        let preview = block.preview();
        let row = super::action_card::event_row(&theme)
            .child(self.event_faces(&format!("exchange-{id}"), [speaker]))
            .child(super::action_card::event_verb(&theme, verb))
            .children(task_title(block.item).map(|title| {
                div()
                    .flex_none()
                    .max_w(relative(0.4))
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(title.to_owned())
            }))
            .child(div().flex_1().min_w_0().truncate().child(preview.clone()))
            .child(super::action_card::event_end(
                &theme,
                block.status.end(&theme),
                Disclosure::Collapsed,
            ));
        super::action_card::event_button(SharedString::from(format!("exchange-line-{id}")))
            .accessibility_label(format!(
                "{}. Show reply. {preview}",
                Self::exchange_label(block, me)
            ))
            .child(row)
            .on_click(cx.listener(move |app, _, window, cx| {
                let Some(view) = app.view.as_mut() else {
                    return;
                };
                view.opened_exchanges.insert(id);
                let replies: Vec<Uuid> = view
                    .rows
                    .iter()
                    .filter_map(|row| row.activity.as_ref())
                    .find(|activity| activity.items.contains(&id))
                    .map(|activity| {
                        let mut ids = activity.answers.clone();
                        ids.push(id);
                        ids
                    })
                    .unwrap_or_default();
                // Opened by hand, the reply shows whole: the owner asked for it.
                view.full_replies.extend(replies);
                app.refold_exchange(id, window, cx);
            }))
            .into_any_element()
    }

    fn render_exchange_block(
        &self,
        block: &Block,
        me: (Uuid, &str),
        conversation: &Conversation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let id = block.item.id;
        let ((speaker, speaker_name), verb) = Self::exchange_voice(block, me);
        let task_open = view.expanded_events.contains(&id);
        let header = super::action_card::event_row(&theme)
            .child(self.event_faces(&format!("exchange-{id}"), [speaker]))
            .child(super::action_card::event_verb(&theme, verb))
            .child(super::action_card::event_object(
                &theme,
                task_title(block.item).unwrap_or_default().to_owned(),
            ))
            .child(super::action_card::event_end(
                &theme,
                block.status.end(&theme),
                if task_open {
                    Disclosure::Expanded
                } else {
                    Disclosure::Collapsed
                },
            ));
        let header =
            super::action_card::event_button(SharedString::from(format!("exchange-task-{id}")))
                .accessibility_label(format!(
                    "{}. {} task",
                    Self::exchange_label(block, me),
                    if task_open { "Hide" } else { "Show" }
                ))
                .child(header)
                .on_click(cx.listener(move |app, _, window, cx| {
                    if let Some(view) = app.view.as_mut()
                        && !view.expanded_events.remove(&id)
                    {
                        view.expanded_events.insert(id);
                    }
                    app.refold_exchange(id, window, cx);
                }));
        let mut body = v_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .pl(px(EVENT_INDENT))
            .pb_2();
        if task_open {
            let task = block.task();
            let from = match block.side {
                Side::Received => format!("Task from {}", block.peer.1),
                Side::Delegated => format!("Task {} sent to {}", me.1, block.peer.1),
            };
            body =
                body.child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .pl_3()
                        .border_l_1()
                        .border_color(theme.border)
                        .text_color(theme.muted_foreground)
                        .child(div().text_xs().child(from))
                        .child(div().text_sm().whitespace_normal().child(
                            if task.trim().is_empty() {
                                "The task text is not available.".to_owned()
                            } else {
                                task.to_owned()
                            },
                        )),
                );
        }
        if let Some(reason) = block.reason(conversation) {
            body = body.child(
                div()
                    .text_sm()
                    .whitespace_normal()
                    .text_color(if block.status == ActivityStatus::Failed {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .child(reason.to_owned()),
            );
        }
        let first_text = block
            .replies
            .iter()
            .map(|reply| reply.text.trim_start())
            .find(|text| !text.is_empty());
        if let Some(line) = block
            .owner_line()
            .filter(|line| first_text.is_none_or(|text| !text.starts_with(line.trim())))
        {
            body = body.child(
                div()
                    .text_sm()
                    .whitespace_normal()
                    .font_weight(FontWeight::MEDIUM)
                    .child(format!("For you: \u{201c}{line}\u{201d}")),
            );
        }
        let mut reply = v_flex().w_full().min_w_0().gap_2();
        let mut shown = false;
        for item in &block.replies {
            if let Some(text) = view.texts.get(&RowKey::Item(item.id)) {
                shown |= !item.text.trim().is_empty() || !item.parts.is_empty();
                reply = reply.child(div().w_full().min_w_0().child(text.render(cx)));
            }
        }
        let reply = if shown {
            reply.into_any_element()
        } else {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(if block.status.settled() {
                    "No reply text."
                } else {
                    "No reply yet."
                })
                .into_any_element()
        };
        // Another bot's words in this chat read as a quote.
        body = body.child(match block.side {
            Side::Received => reply,
            Side::Delegated => div()
                .w_full()
                .min_w_0()
                .pl_3()
                .border_l_2()
                .border_color(theme.border)
                .child(reply)
                .into_any_element(),
        });
        let reply_ids: Vec<Uuid> = block.replies.iter().map(|reply| reply.id).collect();
        let cut = block
            .replies
            .iter()
            .any(|reply| excerpt(&reply.text).is_some());
        let full = reply_ids.iter().all(|id| view.full_replies.contains(id));
        let by_hand = !block.opens();
        if cut || by_hand {
            let less = by_hand || full;
            body = body.child(
                Button::new(SharedString::from(format!("exchange-more-{id}")))
                    .control(Tier::Ghost, Scale::Compact, false, cx)
                    .self_start()
                    // Its label lines up with the reply above.
                    .ml(-Scale::Compact.padding())
                    .accessibility_label(if less {
                        format!("Show less of the reply from {speaker_name}")
                    } else {
                        format!("Show the full reply from {speaker_name}")
                    })
                    .label(if less { "Show less" } else { "Show more" })
                    .on_click(cx.listener(move |app, _, window, cx| {
                        let Some(view) = app.view.as_mut() else {
                            return;
                        };
                        if by_hand {
                            view.opened_exchanges.remove(&id);
                        }
                        if less {
                            for reply in &reply_ids {
                                view.full_replies.remove(reply);
                            }
                        } else {
                            view.full_replies.extend(reply_ids.iter().copied());
                        }
                        app.refold_exchange(id, window, cx);
                    })),
            );
        }
        for ask in exchange_asks([block.item]) {
            let via = if block.peer.0 == ask.notice.bot_id {
                String::new()
            } else {
                format!("Via {}", block.peer.1)
            };
            body = body.child(self.ask_card(&ask.notice, &ask.bot_name, via, None, cx));
        }
        v_flex()
            .w_full()
            .min_w_0()
            .child(header)
            .child(body)
            .into_any_element()
    }
}

impl ElseApp {
    pub(super) fn render_bot_created(
        &self,
        item: Uuid,
        created: &BotCreated,
        stamp: Option<String>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let bot = |id: Uuid| self.bots.iter().find(|bot| bot.id == id);
        let creator = bot(created.creator_bot_id);
        let child = bot(created.bot_id);
        let creator_name = creator.map_or(created.creator_name.clone(), |b| b.name.clone());
        let name = child.map_or(created.name.clone(), |b| b.name.clone());
        let expanded = self
            .view
            .as_ref()
            .is_some_and(|view| view.expanded_events.contains(&item));
        let details = super::action_card::event_action(
            SharedString::from(format!("created-details-{item}")),
            "Details",
            false,
            cx,
        )
        .accessibility_label("Show created bot details")
        .on_click(cx.listener(move |app, _, _, cx| {
            if let Some(view) = app.view.as_mut()
                && !view.expanded_events.remove(&item)
            {
                view.expanded_events.insert(item);
            }
            app.remeasure_row(RowKey::Item(item), cx);
        }));
        let open = child.map(|bot| {
            let id = bot.id;
            super::action_card::event_action(
                SharedString::from(format!("open-created-{item}")),
                "Open chat",
                false,
                cx,
            )
            .accessibility_label(format!("Open chat with {name}"))
            .on_click(cx.listener(move |app, _, window, cx| app.open_bot(id, window, cx)))
        });
        let row = super::action_card::event_row(&theme)
            .child(self.event_faces(&format!("created-{item}"), child.map(|bot| bot.id)))
            .child(super::action_card::event_verb(&theme, "Created bot"))
            .child(super::action_card::event_object(&theme, name.clone()))
            .child(super::action_card::event_end(
                &theme,
                EventEnd::Actions(std::iter::once(details).chain(open).collect()),
                Disclosure::None,
            ));
        v_flex()
            .w_full()
            .min_w_0()
            .child(row)
            .when(expanded, |column| {
                column.child(
                    v_flex()
                        .gap_2()
                        .p_4()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("Created by {creator_name}")),
                        )
                        .child(
                            div()
                                .text_sm()
                                .whitespace_normal()
                                .child(created.purpose.clone()),
                        )
                        .children(stamp.map(|at| {
                            div().text_xs().text_color(theme.muted_foreground).child(at)
                        })),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{ActivityStatus, Block, Side, block_status, excerpt, exchange_asks, task_title};
    use crate::model::{Conversation, OwnerNoticeStatus};
    use serde_json::json;
    use uuid::Uuid;

    fn received_task(status: &str, notice: Option<&str>) -> Conversation {
        let mut turn = json!({
            "id": Uuid::from_u128(10), "request_id": Uuid::from_u128(110), "turn_no": 1,
            "provider_id": "codex", "status": status, "effect_uncertain": false
        });
        if let Some(status) = notice {
            turn["owner_notices"] = json!([{
                "id": Uuid::from_u128(400), "bot_id": Uuid::from_u128(80),
                "turn_id": Uuid::from_u128(10), "kind": "decision", "summary": "Pick a bot",
                "tried": null, "status": status, "created_at": "2026-11-01T00:00:01Z", "settled_at": null
            }]);
        }
        Conversation::from_snapshot(serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": [{"id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(10),
                "kind": "user", "text": "Task received", "bot_task": {
                    "sender_bot_id": Uuid::from_u128(90), "sender_name": "Sentry", "prompt": "Triage"
                }}], "turns": [turn],
            "activity": {"phase": "idle", "active_turn_id": null, "queued_count": 0}
        })).unwrap()).unwrap()
    }

    fn status_of(c: &Conversation) -> ActivityStatus {
        block_status(c.item(Uuid::from_u128(1)).unwrap(), c)
    }

    #[test]
    fn a_received_task_prefers_pending_owner_notices() {
        for (turn_status, notice, expected) in [
            ("completed", Some("pending"), ActivityStatus::NeedsYou),
            ("running", Some("pending"), ActivityStatus::NeedsYou),
            ("failed", Some("pending"), ActivityStatus::NeedsYou),
            ("failed", None, ActivityStatus::Failed),
            ("interrupted", None, ActivityStatus::Stopped),
            ("completed", Some("replied"), ActivityStatus::Replied),
            ("completed", Some("dismissed"), ActivityStatus::Replied),
            ("waiting", None, ActivityStatus::Waiting),
            ("running", None, ActivityStatus::Working),
            ("queued", None, ActivityStatus::Received),
            ("uncertain", None, ActivityStatus::Uncertain),
        ] {
            let c = received_task(turn_status, notice);
            assert_eq!(status_of(&c), expected, "{turn_status} {notice:?}");
        }
    }

    #[test]
    fn owner_notice_dismiss_response_does_not_overwrite_a_replied_event() {
        let mut c = received_task("completed", Some("replied"));
        let mut response = c.owner_notice(Uuid::from_u128(400)).unwrap().clone();
        response.status = OwnerNoticeStatus::Dismissed;
        c.apply_owner_notice(response.clone());
        assert_eq!(
            c.owner_notice(response.id).unwrap().status,
            OwnerNoticeStatus::Replied
        );
        let mut pending = received_task("completed", Some("pending"));
        pending.apply_owner_notice(response.clone());
        assert_eq!(
            pending.owner_notice(response.id).unwrap().status,
            OwnerNoticeStatus::Dismissed
        );
    }

    #[test]
    fn a_received_task_shows_its_latest_answer_and_why_it_stopped() {
        let c = Conversation::from_snapshot(serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": [
                {"id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(10),
                    "kind": "user", "text": "Task received", "bot_task": {
                        "sender_bot_id": Uuid::from_u128(90), "sender_name": "Marketing",
                        "prompt": "Draft", "title": "Draft the reply step"
                    }},
                {"id": Uuid::from_u128(2), "seq": 2, "turn_id": Uuid::from_u128(10),
                    "kind": "assistant", "text": "Checking the cards."},
                {"id": Uuid::from_u128(3), "seq": 3, "turn_id": Uuid::from_u128(10),
                    "kind": "assistant", "text": "I'll add the reply step behind a dry-run lock."}
            ],
            "turns": [{
                "id": Uuid::from_u128(10), "request_id": Uuid::from_u128(110), "turn_no": 1,
                "provider_id": "grok", "status": "interrupted", "effect_uncertain": false,
                "error": {"code": "superseded",
                    "message": "Marketing updated the task. It continues in a new attempt.\nDetails."}
            }],
            "activity": {"phase": "idle", "active_turn_id": null, "queued_count": 0}
        })).unwrap()).unwrap();
        let block = Block::of(c.item(Uuid::from_u128(1)).unwrap(), &c).unwrap();
        assert_eq!(block.side, Side::Received);
        assert_eq!(block.peer.1, "Marketing");
        assert_eq!(block.status, ActivityStatus::Stopped);
        assert!(block.opens(), "a stopped task shows what it wrote");
        assert_eq!(block.task(), "Draft");
        assert_eq!(
            block.replies.iter().map(|r| r.id).collect::<Vec<_>>(),
            [Uuid::from_u128(3)]
        );
        assert_eq!(
            block.reason(&c),
            Some("Marketing updated the task. It continues in a new attempt.")
        );
        assert_eq!(
            block.preview(),
            "I'll add the reply step behind a dry-run lock."
        );
    }

    fn ask(id: u128, status: &str) -> serde_json::Value {
        json!({"id": Uuid::from_u128(id), "bot_id": Uuid::from_u128(90), "bot_name": "Marketing",
            "turn_id": Uuid::from_u128(11), "kind": "decision",
            "summary": "Approve the Romow DM before I send it.", "tried": null, "status": status,
            "created_at": "2026-11-01T00:00:01Z", "settled_at": null})
    }

    /// The delegating chat after Marketing replied to its task.
    fn replied(extra: serde_json::Value) -> Conversation {
        let mut reply = json!({"sender_bot_id": Uuid::from_u128(90), "sender_name": "Marketing",
            "prompt": "Context: run 8f3. Sort the DMs.", "status": "completed"});
        reply
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        Conversation::from_snapshot(serde_json::from_value(json!({
            "conversation_id": Uuid::from_u128(70), "bot_id": Uuid::from_u128(80),
            "event_cursor": "11111111-1111-1111-1111-111111111111:00000000-0000-0000-0000-000000000050:1",
            "items": [{"id": Uuid::from_u128(1), "seq": 1, "turn_id": Uuid::from_u128(10),
                "kind": "status", "text": "MIRAY is the only item that needs a decision.",
                "bot_reply": reply}],
            "turns": [{"id": Uuid::from_u128(10), "request_id": Uuid::from_u128(110), "turn_no": 1,
                "provider_id": "codex", "status": "completed", "effect_uncertain": false,
                "owner_notices": [{
                    "id": Uuid::from_u128(500), "bot_id": Uuid::from_u128(80),
                    "turn_id": Uuid::from_u128(10), "kind": "decision", "summary": "Mine",
                    "tried": null, "status": "pending", "created_at": "2026-11-01T00:00:01Z",
                    "settled_at": null}]}],
            "activity": {"phase": "idle", "active_turn_id": null, "queued_count": 0}
        })).unwrap()).unwrap()
    }

    #[test]
    fn a_reply_shows_what_its_task_declared_and_never_guesses() {
        for (extra, expected, opens) in [
            (json!({}), ActivityStatus::Replied, false),
            (
                json!({"owner_need": "nothing"}),
                ActivityStatus::NothingForYou,
                false,
            ),
            (
                json!({"owner_need": "fyi", "owner_line": "Declined MIRAY myself."}),
                ActivityStatus::Fyi,
                false,
            ),
            (json!({"owner_need": "fyi"}), ActivityStatus::Replied, false),
            (
                json!({"owner_need": "someday"}),
                ActivityStatus::Replied,
                false,
            ),
            (
                json!({"owner_need": "decision", "asks": [ask(400, "pending")]}),
                ActivityStatus::NeedsYou,
                true,
            ),
            (
                json!({"owner_need": "decision", "asks": [ask(400, "dismissed")]}),
                ActivityStatus::Replied,
                false,
            ),
            (json!({"status": "failed"}), ActivityStatus::Failed, true),
        ] {
            // The delegating turn's own pending ask is its own card: it does
            // not make another bot's reply need the owner.
            let c = replied(extra.clone());
            assert_eq!(status_of(&c), expected, "{extra}");
            let block = Block::of(c.item(Uuid::from_u128(1)).unwrap(), &c).unwrap();
            assert_eq!(block.side, Side::Delegated);
            assert_eq!(block.opens(), opens, "{extra}");
        }
    }

    #[test]
    fn a_folded_reply_previews_the_line_written_for_the_owner() {
        let c = replied(json!({"owner_need": "fyi", "owner_line": "Declined MIRAY myself."}));
        let block = Block::of(c.item(Uuid::from_u128(1)).unwrap(), &c).unwrap();
        assert_eq!(block.preview(), "\u{201c}Declined MIRAY myself.\u{201d}");
        let c = replied(json!({"owner_need": "nothing"}));
        let block = Block::of(c.item(Uuid::from_u128(1)).unwrap(), &c).unwrap();
        assert_eq!(
            block.preview(),
            "MIRAY is the only item that needs a decision."
        );
        assert_eq!(block.task(), "Context: run 8f3. Sort the DMs.");
    }

    #[test]
    fn an_exchange_shows_each_ask_once_in_its_latest_state() {
        let first = replied(json!({"title": "Sort the overnight DMs",
            "asks": [ask(400, "pending"), ask(401, "pending")]}));
        let later = replied(json!({"asks": [ask(400, "replied")], "title": " "}));
        let a = first.item(Uuid::from_u128(1)).unwrap();
        let b = later.item(Uuid::from_u128(1)).unwrap();
        assert_eq!(task_title(a), Some("Sort the overnight DMs"));
        assert_eq!(task_title(b), None, "a blank title is no title");
        let asks: Vec<_> = exchange_asks([a, b])
            .into_iter()
            .map(|ask| (ask.notice.id, ask.notice.status))
            .collect();
        assert_eq!(
            asks,
            [
                (Uuid::from_u128(400), OwnerNoticeStatus::Replied),
                (Uuid::from_u128(401), OwnerNoticeStatus::Pending),
            ]
        );
    }

    #[test]
    fn a_short_reply_shows_whole() {
        assert_eq!(excerpt("Done. 14 rows read."), None);
        let eight = (1..=8)
            .map(|n| format!("Line {n}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        assert_eq!(excerpt(&eight), None, "blank lines do not count");
    }

    #[test]
    fn a_long_reply_is_cut_after_a_whole_paragraph_or_list_item() {
        let reply =
            "Summary first.\n\n- one\n- two\n- three\n- four\n- five\n- six\n- seven\n- eight";
        assert_eq!(
            excerpt(reply).as_deref(),
            Some("Summary first.\n\n- one\n- two\n- three\n- four\n- five\n- six\n- seven")
        );
        let paragraph = format!(
            "{}\n\n{}",
            "word ".repeat(80).trim(),
            "word ".repeat(80).trim()
        );
        assert_eq!(
            excerpt(&paragraph).as_deref(),
            Some("word ".repeat(80).trim()),
            "the second paragraph would pass 600 characters"
        );
    }

    #[test]
    fn a_long_block_is_cut_at_a_line_or_a_word_and_says_so() {
        let lines = (1..=12)
            .map(|n| format!("row {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let cut = excerpt(&lines).unwrap();
        assert!(cut.ends_with("row 8…"), "{cut}");
        let code = format!("```\n{}\n```", lines);
        let cut = excerpt(&code).unwrap();
        assert!(cut.starts_with("```\nrow 1"), "{cut}");
        assert!(
            cut.ends_with("row 7\n```"),
            "a cut code block stays closed: {cut}"
        );
        let long = "word ".repeat(200);
        let cut = excerpt(&long).unwrap();
        assert!(cut.ends_with("word…"), "{cut}");
        assert!(cut.chars().count() <= 601);
    }
}
