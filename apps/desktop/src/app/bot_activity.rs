use super::conversation::TranscriptRow;
use super::row_text::RowText;
use super::{ElseApp, RowKey};
use crate::avatar::bot_avatar;
use crate::model::{BotCreated, BotReplyStatus, Conversation, Item};
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Disclosure {
    Activity(Uuid),
    Message(Uuid),
    Prompt(Uuid),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BotActivity {
    pub(super) items: Vec<Uuid>,
    failed: Vec<Uuid>,
    stopped: Vec<Uuid>,
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
        let status = item.bot_reply.as_ref().map(|reply| reply.status);
        let activity = BotActivity {
            items: vec![item.id],
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
            && previous.routine.map(|routine| routine.turn)
                == row.routine.map(|routine| routine.turn)
            && let Some(group) = previous.activity.as_mut()
        {
            group.items.extend(activity.items);
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
    fn bot_disclosure_button(
        &self,
        disclosure: Disclosure,
        item: Uuid,
        label: String,
        cx: &mut Context<Self>,
    ) -> Button {
        let expanded = self
            .view
            .as_ref()
            .is_some_and(|view| view.bot_disclosures.contains(&disclosure));
        Button::new(SharedString::from(format!("bot-{disclosure:?}")))
            .ghost()
            .small()
            .focus_ring(false)
            .cursor_pointer()
            .accessibility_label(format!(
                "{} {label}",
                if expanded { "Collapse" } else { "Expand" }
            ))
            .icon(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .on_click(cx.listener(move |app, _, _, cx| {
                if let Some(view) = app.view.as_mut()
                    && !view.bot_disclosures.remove(&disclosure)
                {
                    view.bot_disclosures.insert(disclosure);
                }
                app.remeasure_row(RowKey::Item(item), cx);
            }))
    }

    pub(super) fn render_bot_activity(
        &mut self,
        row: &TranscriptRow,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let conversation = view.conversation.as_ref()?;
        let activity = row.activity.as_ref()?;
        let &anchor = activity.items.first()?;
        let expanded = view.bot_disclosures.contains(&Disclosure::Activity(anchor));
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
        let count = activity.items.len();
        let mut header = h_flex().w_full().items_center().gap_2();
        for (id, _) in participants.iter().take(3) {
            if let Some(bot) = self.bots.iter().find(|bot| bot.id == *id) {
                header = header.child(
                    bot_avatar(
                        SharedString::from(format!("activity-{anchor}-{id}")),
                        bot.avatar,
                        px(20.),
                    )
                    .still(),
                );
            }
        }
        header = header.child(
            v_flex()
                .flex_1()
                .min_w_0()
                .items_start()
                .gap_1()
                .child(div().w_full().truncate().text_sm().child(names.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "{count} bot {}",
                            if count == 1 { "message" } else { "messages" }
                        )),
                ),
        );
        let last = conversation.item(*activity.items.last()?)?;
        let status = if !activity.failed.is_empty() {
            format!("{} failed", activity.failed.len())
        } else if !activity.stopped.is_empty() {
            format!("{} stopped", activity.stopped.len())
        } else if last.bot_reply.is_some() {
            "Replied".to_owned()
        } else {
            "Task received".to_owned()
        };
        header = header.child(
            div()
                .text_xs()
                .text_color(if activity.failed.is_empty() {
                    theme.muted_foreground
                } else {
                    theme.danger
                })
                .child(status),
        );
        let mut card = v_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .p_3()
            .rounded(theme.radius_lg)
            .bg(theme.secondary.opacity(0.5))
            .child(
                self.bot_disclosure_button(
                    Disclosure::Activity(anchor),
                    anchor,
                    format!("bot activity with {names}"),
                    cx,
                )
                .w_full()
                .h_auto()
                .child(header),
            );
        for (ids, label, color) in [
            (&activity.failed, "Reply failed", theme.danger),
            (&activity.stopped, "Reply stopped", theme.muted_foreground),
        ] {
            for id in ids {
                let item = conversation.item(*id)?;
                let (_, name, _) = exchange_sender(item)?;
                let detail = item
                    .text
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("No reply text.");
                card = card.child(
                    v_flex()
                        .gap_1()
                        .text_xs()
                        .text_color(color)
                        .child(format!("{name} · {label}"))
                        .child(div().line_clamp(2).child(detail.to_owned())),
                );
            }
        }
        if expanded {
            for id in &activity.items {
                let mut message = row.clone();
                message.key = RowKey::Item(*id);
                message.activity = None;
                message.footer_stamp = None;
                card = card.children(self.render_row_content(&message, cx));
            }
        }
        Some(card.into_any_element())
    }

    pub(super) fn render_bot_message(
        &self,
        item: &Item,
        text: &RowText,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let view = self.view.as_ref()?;
        let (sender, name, kind) = exchange_sender(item)?;
        let recipient = self
            .bots
            .iter()
            .find(|bot| bot.id == view.bot_id)
            .map_or("Bot", |bot| bot.name.as_str());
        let expanded = view.bot_disclosures.contains(&Disclosure::Message(item.id));
        let theme = cx.theme().clone();
        let label = format!("{name} → {recipient} · {kind}");
        let mut header = h_flex().w_full().min_w_0().items_center().gap_1p5();
        if let Some(bot) = self.bots.iter().find(|bot| bot.id == sender) {
            header = header.child(
                bot_avatar(
                    SharedString::from(format!("exchange-{}", item.id)),
                    bot.avatar,
                    px(18.),
                )
                .still(),
            );
        }
        header = header.child(div().flex_1().min_w_0().truncate().child(label.clone()));
        let mut card = div()
            .w_full()
            .p_2()
            .rounded(theme.radius)
            .bg(theme.sidebar.opacity(0.5))
            .child(
                self.bot_disclosure_button(Disclosure::Message(item.id), item.id, label, cx)
                    .w_full()
                    .child(header),
            );
        let body = item
            .bot_task
            .as_ref()
            .map_or(item.text.as_str(), |task| task.prompt.as_str());
        if expanded {
            card = card.child(div().pt_2().child(
                if item.bot_reply.is_some() && body.trim().is_empty() {
                    div()
                        .text_sm()
                        .child("Finished without a reply.")
                        .into_any_element()
                } else {
                    text.render(cx)
                },
            ));
            if let Some(reply) = item.bot_reply.as_ref() {
                let prompt_open = view.bot_disclosures.contains(&Disclosure::Prompt(item.id));
                card = card.child(
                    h_flex().pt_2().child(
                        self.bot_disclosure_button(
                            Disclosure::Prompt(item.id),
                            item.id,
                            "original task".into(),
                            cx,
                        )
                        .xsmall()
                        .label("Original task"),
                    ),
                );
                if prompt_open {
                    card = card.child(
                        div()
                            .pt_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(reply.prompt.clone()),
                    );
                }
            }
        } else {
            let preview = body.split_whitespace().collect::<Vec<_>>().join(" ");
            // Clipped, not `line_clamp`: GPUI caches a clamped line under the
            // same key as the unclamped one, so the expanded reply would
            // reuse this preview's unwrapped lines.
            card = card.child(
                div()
                    .mt_1()
                    .text_sm()
                    .line_height(rems(1.25))
                    .max_h(rems(2.5))
                    .overflow_hidden()
                    .text_color(theme.muted_foreground)
                    .child(if preview.is_empty() {
                        "Finished without a reply.".to_owned()
                    } else {
                        preview
                    }),
            );
        }
        Some(card)
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
