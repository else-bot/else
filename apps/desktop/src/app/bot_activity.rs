use super::action_card::{Disclosure, EventEnd};
use super::conversation::{ReplyLayout, TranscriptRow};
use super::{ElseApp, RowKey};
use crate::model::{BotCreated, BotReplyStatus, Conversation, Item};
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BotActivity {
    /// The turn that holds this exchange. Separate turns are separate exchanges.
    turn: Uuid,
    pub(super) items: Vec<Uuid>,
    /// This bot's latest reply to each task it received, hidden from the transcript.
    pub(super) answers: Vec<Uuid>,
    failed: Vec<Uuid>,
    stopped: Vec<Uuid>,
    /// Its turn has a notice still waiting on the owner.
    needs_owner: bool,
}

impl BotActivity {
    pub(super) fn contains(&self, id: Uuid) -> bool {
        self.items.contains(&id) || self.answers.contains(&id)
    }
}

/// How the exchange `item` ended: a delegated bot's reply carries it, and a
/// received task takes it from the turn that ran it.
fn outcome(item: &Item, conversation: Option<&Conversation>) -> Option<BotReplyStatus> {
    if let Some(reply) = &item.bot_reply {
        return Some(reply.status);
    }
    item.bot_task.as_ref()?;
    match conversation?.turn(item.turn_id)?.status.as_str() {
        "failed" => Some(BotReplyStatus::Failed),
        "interrupted" => Some(BotReplyStatus::Cancelled),
        _ => None,
    }
}

fn waits_on_owner(conversation: Option<&Conversation>, turn: Uuid) -> bool {
    conversation.and_then(|c| c.turn(turn)).is_some_and(|turn| {
        turn.owner_notices
            .iter()
            .any(|n| n.status == crate::model::OwnerNoticeStatus::Pending)
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
        let Some(item) = item.filter(|item| item.bot_task.is_some() || item.bot_reply.is_some())
        else {
            grouped.push(row);
            continue;
        };
        let status = outcome(item, conversation);
        let activity = BotActivity {
            turn: item.turn_id,
            items: vec![item.id],
            answers: conversation
                .map(|c| {
                    c.task_answers(item)
                        .iter()
                        .map(|answer| answer.id)
                        .collect()
                })
                .unwrap_or_default(),
            failed: (status == Some(BotReplyStatus::Failed))
                .then_some(item.id)
                .into_iter()
                .collect(),
            stopped: (status == Some(BotReplyStatus::Cancelled))
                .then_some(item.id)
                .into_iter()
                .collect(),
            needs_owner: waits_on_owner(conversation, item.turn_id),
        };
        if !row.unread_start
            && let Some(previous) = grouped.last_mut()
            && let Some(group) = previous.activity.as_mut()
            && group.turn == activity.turn
        {
            group.items.extend(activity.items);
            group.answers.extend(activity.answers);
            group.failed.extend(activity.failed);
            group.stopped.extend(activity.stopped);
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

impl ElseApp {
    pub(super) fn render_bot_activity(
        &mut self,
        row: &TranscriptRow,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let conversation = view.conversation.as_ref()?;
        let activity = row.activity.as_ref()?;
        let &anchor = activity.items.first()?;
        let theme = cx.theme().clone();
        let mut participants = Vec::new();
        for id in &activity.items {
            let item = conversation.item(*id)?;
            let (id, name, _) = exchange_sender(item)?;
            if !participants.iter().any(|(known, _)| *known == id) {
                participants.push((id, name));
            }
        }
        if let Some(bot) = self.bots.iter().find(|bot| bot.id == view.bot_id)
            && !participants.iter().any(|(id, _)| *id == bot.id)
        {
            participants.push((bot.id, bot.name.as_str()));
        }
        let names = participants
            .iter()
            .map(|(_, name)| *name)
            .collect::<Vec<_>>()
            .join(" & ");
        let count = activity.items.len() + activity.answers.len();
        let last = conversation.item(*activity.items.last()?)?;
        let me = self.bots.iter().find(|bot| bot.id == view.bot_id);
        let answer = conversation.task_answers(last).pop();
        let (last_sender, last_text) = match (answer, me) {
            (Some(answer), Some(me)) => (me.name.as_str(), answer.text.as_str()),
            _ => (
                exchange_sender(last)?.1,
                last.bot_task
                    .as_ref()
                    .map_or(last.text.as_str(), |task| task.prompt.as_str()),
            ),
        };
        let last_text = last_text.split_whitespace().collect::<Vec<_>>().join(" ");
        let status = activity_status(activity, last, conversation);
        let messages = format!(
            "{count} {}",
            if count == 1 { "message" } else { "messages" }
        );
        // This conversation's bot is a party to every exchange in it, so the
        // row names only the others: "Task from Research".
        let others = participants
            .iter()
            .filter(|(id, _)| *id != view.bot_id)
            .map(|(_, name)| *name)
            .collect::<Vec<_>>();
        let object = if others.is_empty() {
            names.clone()
        } else {
            others.join(" & ")
        };
        let verb = if conversation.item(anchor)?.bot_task.is_some() {
            "Task from"
        } else {
            "Reply from"
        };
        let header = super::action_card::event_row(&theme)
            .child(self.event_faces(
                &format!("activity-{anchor}"),
                participants.iter().map(|(id, _)| *id),
            ))
            .child(super::action_card::event_verb(&theme, verb))
            .child(super::action_card::event_object(&theme, object))
            .child(super::action_card::event_end(
                &theme,
                EventEnd::Meta(format!("{} · {messages}", status.label()).into()),
                Disclosure::None,
            ));
        let title = names.clone();
        let mut card = v_flex().w_full().min_w_0().child(
            Button::new(SharedString::from(format!("bot-activity-{anchor}")))
                .ghost()
                .focus_ring(false)
                .cursor_pointer()
                .w_full()
                .h_auto()
                .p_0()
                .accessibility_label(format!(
                    "Open conversation between {names}, {messages}, {}. {last_sender}: {last_text}",
                    status.label()
                ))
                .child(header)
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.open_bot_exchange(anchor, title.clone(), window, cx);
                })),
        );
        let mut notes = v_flex().w_full().gap_2().px_3p5().pb_3();
        let mut noted = false;
        for (ids, label, color) in [
            (&activity.failed, "Reply failed", theme.danger),
            (&activity.stopped, "Reply stopped", theme.muted_foreground),
        ] {
            for id in ids {
                let item = conversation.item(*id)?;
                let (name, reason, text) = match (&item.bot_task, me) {
                    (Some(_), Some(me)) => (
                        me.name.as_str(),
                        conversation
                            .turn(item.turn_id)
                            .and_then(|turn| turn.error.as_ref()?.message.as_deref()),
                        "",
                    ),
                    _ => (
                        exchange_sender(item)?.1,
                        item.bot_reply.as_ref().and_then(|r| r.reason.as_deref()),
                        item.text.as_str(),
                    ),
                };
                let (reason, reply) = note_lines(reason, text);
                noted = true;
                notes = notes.child(
                    v_flex()
                        .gap_1()
                        .text_xs()
                        .child(div().text_color(color).child(format!("{name} · {label}")))
                        .when_some(reason, |note, reason| {
                            note.child(
                                div()
                                    .line_clamp(2)
                                    .text_color(theme.muted_foreground)
                                    .child(reason.to_owned()),
                            )
                        })
                        .when_some(reply, |note, reply| {
                            note.child(
                                div()
                                    .line_clamp(2)
                                    .text_color(theme.muted_foreground)
                                    .child(reply.to_owned()),
                            )
                        }),
                );
            }
        }
        if noted {
            card = card.child(notes);
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
            card = card.on_prepaint(move |bounds, window, cx| {
                window.defer(cx, move |window, cx| {
                    let _ = owner.update(cx, |app, cx| {
                        app.reply_laid_out(bot, ReplyLayout { position, bounds }, window, cx)
                    });
                });
            });
        }
        Some(card.into_any_element())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivityStatus {
    NeedsYou,
    Failed(usize),
    Stopped(usize),
    Replied,
    Received,
    Uncertain,
    Waiting,
    Working,
}

impl ActivityStatus {
    fn label(self) -> String {
        match self {
            Self::NeedsYou => "Needs you".into(),
            Self::Failed(count) => format!("{count} failed"),
            Self::Stopped(count) => format!("{count} stopped"),
            Self::Replied => "Replied".into(),
            Self::Received => "Task received".into(),
            Self::Uncertain => "Unknown result".into(),
            Self::Waiting => "Waiting".into(),
            Self::Working => "Working".into(),
        }
    }
}

fn activity_status(activity: &BotActivity, last: &Item, c: &Conversation) -> ActivityStatus {
    if activity.needs_owner {
        return ActivityStatus::NeedsYou;
    }
    if !activity.failed.is_empty() {
        return ActivityStatus::Failed(activity.failed.len());
    }
    if !activity.stopped.is_empty() {
        return ActivityStatus::Stopped(activity.stopped.len());
    }
    let task_status = last
        .bot_task
        .as_ref()
        .and_then(|_| c.turn(last.turn_id))
        .map(|turn| turn.status.as_str());
    if last.bot_reply.is_some() || task_status == Some("completed") {
        return ActivityStatus::Replied;
    }
    match task_status {
        None | Some("queued") => ActivityStatus::Received,
        Some("uncertain") => ActivityStatus::Uncertain,
        Some("waiting") => ActivityStatus::Waiting,
        _ => ActivityStatus::Working,
    }
}

fn note_lines<'a>(reason: Option<&'a str>, text: &'a str) -> (Option<&'a str>, Option<&'a str>) {
    let first = |text: &'a str| text.lines().map(str::trim).find(|line| !line.is_empty());
    let reason = reason.and_then(first);
    let reply = first(text).filter(|line| Some(*line) != reason);
    match (reason, reply) {
        (None, None) => (None, Some("No reply text.")),
        lines => lines,
    }
}

fn exchange_sender(item: &Item) -> Option<(Uuid, &str, &str)> {
    item.bot_task
        .as_ref()
        .map(|task| (task.sender_bot_id, task.sender_name.as_str(), "Task"))
        .or_else(|| {
            item.bot_reply.as_ref().map(|reply| {
                (
                    reply.sender_bot_id,
                    reply.sender_name.as_str(),
                    reply.status.label(),
                )
            })
        })
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
    use super::{ActivityStatus, BotActivity, activity_status, note_lines, waits_on_owner};
    use crate::model::Conversation;
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

    #[test]
    fn bot_activity_status_prefers_pending_owner_notices() {
        for (turn_status, notice, expected) in [
            ("completed", Some("pending"), ActivityStatus::NeedsYou),
            ("running", Some("pending"), ActivityStatus::NeedsYou),
            ("failed", Some("pending"), ActivityStatus::NeedsYou),
            ("completed", Some("replied"), ActivityStatus::Replied),
            ("completed", Some("dismissed"), ActivityStatus::Replied),
            ("waiting", None, ActivityStatus::Waiting),
            ("running", None, ActivityStatus::Working),
            ("queued", None, ActivityStatus::Received),
            ("uncertain", None, ActivityStatus::Uncertain),
        ] {
            let c = received_task(turn_status, notice);
            let item = c.item(Uuid::from_u128(1)).unwrap();
            let activity = BotActivity {
                turn: item.turn_id,
                items: vec![item.id],
                answers: vec![],
                failed: if turn_status == "failed" {
                    vec![item.id]
                } else {
                    vec![]
                },
                stopped: vec![],
                needs_owner: waits_on_owner(Some(&c), item.turn_id),
            };
            assert_eq!(activity_status(&activity, item, &c), expected);
        }
        assert_eq!(ActivityStatus::Waiting.label(), "Waiting");
        assert_eq!(ActivityStatus::NeedsYou.label(), "Needs you");
    }

    #[test]
    fn owner_notice_dismiss_response_does_not_overwrite_a_replied_event() {
        let mut c = received_task("completed", Some("replied"));
        let mut response = c.owner_notice(Uuid::from_u128(400)).unwrap().clone();
        response.status = crate::model::OwnerNoticeStatus::Dismissed;
        c.apply_owner_notice(response.clone());
        assert_eq!(
            c.owner_notice(response.id).unwrap().status,
            crate::model::OwnerNoticeStatus::Replied
        );
        let mut pending = received_task("completed", Some("pending"));
        pending.apply_owner_notice(response.clone());
        assert_eq!(
            pending.owner_notice(response.id).unwrap().status,
            crate::model::OwnerNoticeStatus::Dismissed
        );
    }

    #[test]
    fn a_failure_reason_stays_apart_from_the_progress_it_wrote() {
        assert_eq!(
            note_lines(
                Some("Stopped at the 30-minute attempt limit."),
                "Routine paused.\nNow the computer state."
            ),
            (
                Some("Stopped at the 30-minute attempt limit."),
                Some("Routine paused.")
            )
        );
        assert_eq!(
            note_lines(Some("no input"), "no input"),
            (Some("no input"), None),
            "a reply that only repeats the reason shows once"
        );
        assert_eq!(
            note_lines(None, "Half of the form"),
            (None, Some("Half of the form"))
        );
        assert_eq!(note_lines(None, " "), (None, Some("No reply text.")));
    }
}
