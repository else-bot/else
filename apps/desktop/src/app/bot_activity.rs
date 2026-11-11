use super::conversation::{ReplyLayout, TranscriptRow};
use super::{ElseApp, RowKey};
use crate::avatar::bot_avatar;
use crate::model::{BotCreated, BotReplyStatus, Conversation, Item};
use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
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
        let preview = if last_text.is_empty() {
            format!("{last_sender} finished without a reply.")
        } else {
            format!("{last_sender}: {last_text}")
        };
        let task_status = last
            .bot_task
            .as_ref()
            .and_then(|_| conversation.turn(last.turn_id))
            .map(|turn| turn.status.as_str());
        let status = if !activity.failed.is_empty() {
            format!("{} failed", activity.failed.len())
        } else if !activity.stopped.is_empty() {
            format!("{} stopped", activity.stopped.len())
        } else if last.bot_reply.is_some() || task_status == Some("completed") {
            "Replied".to_owned()
        } else if task_status.is_none_or(|status| status == "queued") {
            "Task received".to_owned()
        } else if task_status == Some("uncertain") {
            "Unknown result".to_owned()
        } else {
            "Working".to_owned()
        };
        let messages = format!(
            "{count} {}",
            if count == 1 { "message" } else { "messages" }
        );
        let mut avatars = h_flex().flex_none().items_center().gap_1();
        for (id, _) in participants.iter().take(3) {
            if let Some(bot) = self.bots.iter().find(|bot| bot.id == *id) {
                avatars = avatars.child(
                    bot_avatar(
                        SharedString::from(format!("activity-{anchor}-{id}")),
                        bot.avatar,
                        px(20.),
                    )
                    .still(),
                );
            }
        }
        let header = h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .gap_3()
            .child(avatars.mt(px(1.)))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .text_left()
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(names.clone()),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(if activity.failed.is_empty() {
                                        theme.muted_foreground
                                    } else {
                                        theme.danger
                                    })
                                    .child(format!("{status} · {messages}")),
                            )
                            .child(
                                Icon::new(IconName::ChevronRight)
                                    .xsmall()
                                    .flex_none()
                                    .text_color(theme.muted_foreground),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(preview),
                    ),
            );
        let title = names.clone();
        let mut card = v_flex()
            .w_full()
            .min_w_0()
            .rounded(theme.radius_lg)
            .bg(theme.secondary.opacity(0.5))
            .child(
                Button::new(SharedString::from(format!("bot-activity-{anchor}")))
                    .ghost()
                    .focus_ring(false)
                    .cursor_pointer()
                    .w_full()
                    .h_auto()
                    .px_3p5()
                    .py_2p5()
                    .rounded(theme.radius_lg)
                    .accessibility_label(format!(
                        "Open conversation between {names}, {messages}, {status}"
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
                let (name, text) = match (&item.bot_task, me) {
                    (Some(_), Some(me)) => (
                        me.name.as_str(),
                        conversation
                            .turn(item.turn_id)
                            .and_then(|turn| turn.error.as_ref()?.message.as_deref())
                            .unwrap_or_default(),
                    ),
                    _ => (exchange_sender(item)?.1, item.text.as_str()),
                };
                let detail = text
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("No reply text.");
                noted = true;
                notes = notes.child(
                    v_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(color)
                        .child(format!("{name} · {label}"))
                        .child(div().line_clamp(2).child(detail.to_owned())),
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
        let identity = h_flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap_2p5()
            .children(child.map(|b| {
                bot_avatar(
                    SharedString::from(format!("created-{item}")),
                    b.avatar,
                    px(32.),
                )
                .still()
            }))
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(child.is_none(), |name| {
                                name.text_color(theme.muted_foreground)
                            })
                            .child(name.clone()),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if child.is_some() {
                            "New bot"
                        } else {
                            "Deleted bot"
                        },
                    )),
            );
        let open = child.map(|b| {
            let id = b.id;
            Button::new(SharedString::from(format!("open-created-{item}")))
                .ghost()
                .small()
                .focus_ring(false)
                .cursor_pointer()
                .label("Open chat")
                .accessibility_label(format!("Open chat with {name}"))
                .on_click(cx.listener(move |app, _, window, cx| app.open_bot(id, window, cx)))
        });
        let card = v_flex()
            .id(SharedString::from(format!("message-{item}")))
            .w_full()
            .gap_3()
            .p_3p5()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border.opacity(0.6))
            .bg(theme.sidebar.opacity(0.6))
            .child(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .children(creator.map(|b| {
                        bot_avatar(
                            SharedString::from(format!("creator-{item}")),
                            b.avatar,
                            px(16.),
                        )
                        .still()
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.foreground)
                            .child(creator_name),
                    )
                    .child(div().flex_none().child("created a bot")),
            )
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .items_center()
                    .gap_3()
                    .child(identity)
                    .children(open),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border.opacity(0.6))
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.muted_foreground)
                            .child("Purpose"),
                    )
                    .child(div().w_full().text_sm().child(created.purpose.clone())),
            );
        v_flex()
            .w_full()
            .gap_1()
            .child(
                div()
                    .w_full()
                    .pr(super::message_row::BOT_GUTTER)
                    .child(card),
            )
            .children(stamp.map(|at| div().text_xs().text_color(theme.muted_foreground).child(at)))
            .into_any_element()
    }
}
