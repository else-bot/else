pub(super) mod state;

use self::state::{ReplyState, TaskReply};
use super::conversation::ReplyLayout;
use super::reply::Author;
use super::{ElseApp, RowKey};
use crate::avatar::bot_avatar;
use chrono::Utc;
use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::time::Duration;
use uuid::Uuid;

impl ElseApp {
    pub(super) fn render_task_reply(
        &self,
        task: &TaskReply,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let conversation = view.conversation.as_ref()?;
        let turn = conversation.turn(task.turn)?;
        let bot = self.bots.iter().find(|bot| bot.id == view.bot_id);
        let theme = cx.theme().clone();
        let expanded = view.expanded_tasks.contains(&task.turn);
        let working = task.state == ReplyState::Working;
        let (status, error) = super::message_row::turn_label(turn);
        let title = match task.state {
            ReplyState::Complete => "Task complete".to_owned(),
            ReplyState::Working => task
                .summary
                .clone()
                .unwrap_or_else(|| "Working on your task".to_owned()),
            _ => status.clone(),
        };
        let elapsed = task.elapsed(Utc::now());
        let color = if error {
            theme.danger
        } else if working {
            theme.primary
        } else {
            theme.muted_foreground
        };
        let header = h_flex()
            .w_full()
            .items_center()
            .gap_3()
            .children(bot.map(|bot| {
                bot_avatar(
                    SharedString::from(format!("task-avatar-{}", task.turn)),
                    bot.avatar,
                    px(34.),
                )
                .when(working, |avatar| avatar.working())
            }))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .text_xs()
                            .text_color(color)
                            .child(if working {
                                "Working".to_owned()
                            } else {
                                bot.map_or_else(|| "Bot".to_owned(), |bot| bot.name.clone())
                            })
                            .children(elapsed.as_ref().filter(|_| !task.state.terminal()).map(
                                |elapsed| {
                                    div()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("· {elapsed}"))
                                },
                            )),
                    ),
            )
            .children((task.state == ReplyState::Complete).then(|| {
                h_flex()
                    .items_center()
                    .gap_1()
                    .text_xs()
                    .text_color(theme.success)
                    .child(Icon::new(IconName::Check).xsmall())
                    .child("Done")
            }));
        let show_reply =
            task.latest_kept || !matches!(task.state, ReplyState::Queued | ReplyState::Working);
        let mut card = v_flex()
            .relative()
            .w_full()
            .min_w_0()
            .gap_3()
            .p_3p5()
            .rounded(theme.radius_lg)
            .bg(theme.secondary.opacity(0.5))
            .child(header);
        if error {
            card = card.child(div().text_sm().text_color(theme.danger).child(status));
        }
        if show_reply {
            if let Some(id) = task.latest {
                card = card.child(
                    div()
                        .w_full()
                        .pt_3()
                        .border_t_1()
                        .border_color(theme.border.opacity(0.5))
                        .children(self.task_message(id, cx)),
                );
            } else if task.state == ReplyState::Complete {
                card = card.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Finished without a text reply."),
                );
            }
        }
        for &id in &task.kept {
            card = card.child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .p_3()
                    .rounded(theme.radius)
                    .bg(theme.primary.opacity(0.06))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Kept from this task"),
                    )
                    .children(self.task_message(id, cx)),
            );
        }
        let mut history = task.history.clone();
        if !show_reply && let Some(id) = task.latest {
            history.push(id);
        }
        if !history.is_empty() || (task.state.terminal() && elapsed.is_some()) {
            let label = if task.state.terminal() {
                elapsed.map_or_else(
                    || "Activity".to_owned(),
                    |elapsed| format!("Worked for {elapsed}"),
                )
            } else {
                format!(
                    "Activity · {} {}",
                    history.len(),
                    if history.len() == 1 {
                        "update"
                    } else {
                        "updates"
                    }
                )
            };
            let id = task.turn;
            card = card.child(
                h_flex()
                    .w_full()
                    .pt_2()
                    .border_t_1()
                    .border_color(theme.border.opacity(0.5))
                    .child(if history.is_empty() {
                        h_flex()
                            .items_center()
                            .gap_1p5()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(Icon::new(IconName::Clock).xsmall())
                            .child(label)
                            .into_any_element()
                    } else {
                        Button::new(SharedString::from(format!("task-history-{id}")))
                            .ghost()
                            .xsmall()
                            .focus_ring(false)
                            .cursor_pointer()
                            .icon(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .label(label)
                            .on_click(cx.listener(move |app, _, _, cx| {
                                if let Some(view) = app.view.as_mut()
                                    && !view.expanded_tasks.remove(&id)
                                {
                                    view.expanded_tasks.insert(id);
                                }
                                app.remeasure_row(RowKey::Task(id), cx);
                            }))
                            .into_any_element()
                    }),
            );
        }
        if expanded {
            for id in history {
                card = card.child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .pl_3()
                        .border_l_1()
                        .border_color(theme.border)
                        .children(conversation.item(id).and_then(|item| item.created_at).map(
                            |at| {
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(super::message_row::stamp(at))
                            },
                        ))
                        .children(self.task_message(id, cx)),
                );
            }
        }
        let animate = !cx.reduce_motion()
            && view
                .reply_transitions
                .get(&task.turn)
                .is_some_and(|at| at.elapsed() < Duration::from_millis(320));
        if animate {
            Some(
                card.with_animation(
                    SharedString::from(format!("task-settled-{}", task.turn)),
                    Animation::new(Duration::from_millis(280)).with_max_fps(60.),
                    |card, t| {
                        let ease = 1. - (1. - t).powi(3);
                        card.opacity(0.3 + 0.7 * ease).top(px(5. * (1. - ease)))
                    },
                )
                .into_any_element(),
            )
        } else {
            Some(card.into_any_element())
        }
    }

    fn task_message(&self, id: Uuid, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let conversation = view.conversation.as_ref()?;
        let item = conversation.item(id)?;
        let text = view.texts.get(&RowKey::Item(id))?;
        let bot = view.bot_id;
        let owner = cx.entity().downgrade();
        let mut body = div()
            .id(SharedString::from(format!("task-message-{id}")))
            .relative()
            .group("message-bubble")
            .w_full()
            .min_w_0()
            .child(div().w_full().pr(px(64.)).child(text.render()))
            .context_menu(move |menu, window, cx| {
                Self::message_menu(owner.clone(), bot, id, Author::Bot, menu, window, cx)
            });
        if conversation
            .latest_reply()
            .is_some_and(|reply| reply.id == id)
        {
            let owner = cx.entity().downgrade();
            let position = crate::api::ReplyPosition {
                item_seq: item.seq,
                text_bytes: text.rendered_bytes(cx).unwrap_or(0) as i64,
            };
            body = body.on_prepaint(move |bounds, window, cx| {
                let owner = owner.clone();
                window.defer(cx, move |window, cx| {
                    let _ = owner.update(cx, |app, cx| {
                        app.reply_laid_out(bot, ReplyLayout { position, bounds }, window, cx)
                    });
                });
            });
        }
        body = body.child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .invisible()
                .group_hover("message-bubble", |this| this.visible())
                .child(self.message_actions(bot, id, Author::Bot, cx)),
        );
        if let Some(reaction) = conversation.reaction_for(id) {
            body = body.child(div().pt_1().text_sm().child(reaction.mark()));
        }
        Some(body.into_any_element())
    }
}
