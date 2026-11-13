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
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Size, h_flex, v_flex};
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
        let complete = task.state == ReplyState::Complete;
        let (status, error) = super::message_row::turn_label(turn);
        let title = bot.map_or_else(|| "Task".to_owned(), |bot| bot.name.clone());
        let summary = match task.state {
            ReplyState::Complete => "Task complete".to_owned(),
            ReplyState::Working => task
                .summary
                .clone()
                .unwrap_or_else(|| "Working on your task".to_owned()),
            _ => status,
        };
        let elapsed = task.elapsed(Utc::now());
        // Parked on other bots' tasks: their faces show who has the work.
        let delegates: Vec<Uuid> = if task.state == ReplyState::Waiting && !error {
            turn.open_delegations()
                .iter()
                .map(|d| d.target_bot_id)
                .collect()
        } else {
            Vec::new()
        };
        let delegate_faces = self.face_stack(
            &format!("task-delegates-{}", task.turn),
            delegates.iter().copied(),
            px(16.),
        );
        let delegate_badge = self.face_stack(
            &format!("task-badge-{}", task.turn),
            delegates.first().copied(),
            px(18.),
        );
        let handed_off = delegate_faces.is_some();
        let color = if error {
            theme.danger
        } else if working {
            theme.primary
        } else if complete {
            theme.success
        } else {
            theme.muted_foreground
        };
        let show_reply =
            task.latest_kept || !matches!(task.state, ReplyState::Queued | ReplyState::Working);
        let mut history = task.history.clone();
        if !show_reply && let Some(id) = task.latest {
            history.push(id);
        }
        let expandable = !history.is_empty();
        let header_padding = if complete { px(10.) } else { px(14.) };
        let header = if complete {
            h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_1p5()
                .text_xs()
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .text_color(color)
                        .child(Icon::new(IconName::Check).xsmall().flex_none())
                        .child(summary.clone()),
                )
                .children(elapsed.map(|elapsed| {
                    div()
                        .text_color(theme.muted_foreground)
                        .child(format!("· {elapsed}"))
                }))
                .child(div().flex_1())
                .children(expandable.then(|| {
                    h_flex()
                        .items_center()
                        .gap_1()
                        .text_color(theme.muted_foreground)
                        .child(format!(
                            "{} {}",
                            history.len(),
                            if history.len() == 1 {
                                "update"
                            } else {
                                "updates"
                            }
                        ))
                        .child(
                            Icon::new(if expanded {
                                IconName::ChevronDown
                            } else {
                                IconName::ChevronRight
                            })
                            .xsmall()
                            .flex_none(),
                        )
                }))
        } else {
            let label = if error {
                "Needs attention"
            } else {
                task.state.label()
            };
            let pill = (error || !matches!(task.state, ReplyState::Working | ReplyState::Waiting))
                .then_some(label);
            let detail = (!error && summary != label).then(|| summary.clone());
            let updates = expandable.then(|| {
                let count = history.len();
                format!("{count} {}", if count == 1 { "update" } else { "updates" })
            });
            h_flex()
                .w_full()
                .min_w_0()
                .items_center()
                .gap_4()
                .children(bot.map(|bot| {
                    let avatar = bot_avatar(
                        SharedString::from(format!("task-avatar-{}", task.turn)),
                        bot.avatar,
                        px(38.),
                    )
                    .when(working, |avatar| avatar.working());
                    h_flex()
                        .flex_none()
                        .size(px(56.))
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .relative()
                                .child(
                                    div()
                                        .when(handed_off, |face| face.opacity(0.55))
                                        .child(avatar),
                                )
                                .children(delegate_badge.map(|badge| {
                                    div()
                                        .absolute()
                                        .right(px(-4.))
                                        .bottom(px(-4.))
                                        .rounded_full()
                                        .border_2()
                                        .border_color(theme.background)
                                        .child(badge)
                                })),
                        )
                }))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap(px(3.))
                        .text_left()
                        .child(
                            h_flex()
                                .min_w_0()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .min_w_0()
                                        .text_sm()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .truncate()
                                        .child(title.clone()),
                                )
                                .children(pill.map(|label| {
                                    div()
                                        .flex_none()
                                        .px_2()
                                        .rounded_full()
                                        .border_1()
                                        .border_color(color.opacity(0.3))
                                        .bg(color.opacity(0.08))
                                        .text_xs()
                                        .text_color(color)
                                        .child(label)
                                })),
                        )
                        .children(detail.map(|detail| {
                            h_flex()
                                .min_w_0()
                                .gap_1p5()
                                .items_center()
                                .text_sm()
                                .text_color(if working {
                                    theme.foreground
                                } else {
                                    theme.muted_foreground
                                })
                                .children(delegate_faces)
                                .child(div().min_w_0().truncate().child(detail))
                        }))
                        .when(working, |column| {
                            column.child(work_sweep(task.turn, color, cx.reduce_motion()))
                        }),
                )
                .child(
                    v_flex()
                        .flex_none()
                        .items_end()
                        .gap_1p5()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(h_flex().items_center().gap_2().children(elapsed).children(
                            expandable.then(|| {
                                Icon::new(if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .xsmall()
                                .flex_none()
                            }),
                        ))
                        .children(updates),
                )
        };
        let mut activity = v_flex()
            .w_full()
            .min_w_0()
            .child(if expandable {
                let id = task.turn;
                Button::new(SharedString::from(format!("task-history-{id}")))
                    .ghost()
                    .focus_ring(false)
                    .cursor_pointer()
                    .toggled(expanded)
                    .with_size(Size::Size(px(16.)))
                    .w_full()
                    .px_3p5()
                    .py(header_padding)
                    .rounded(theme.radius_lg - px(1.))
                    .when(complete, |button| button.rounded_t(px(0.)))
                    .when(expanded, |button| button.rounded_b(px(0.)))
                    .accessibility_label(format!(
                        "{} activity for {title}. {summary}",
                        if expanded { "Collapse" } else { "Expand" }
                    ))
                    .child(header)
                    .on_click(cx.listener(move |app, _, _, cx| {
                        if let Some(view) = app.view.as_mut()
                            && !view.expanded_tasks.remove(&id)
                        {
                            view.expanded_tasks.insert(id);
                        }
                        app.remeasure_row(RowKey::Task(id), cx);
                    }))
                    .into_any_element()
            } else {
                header.px_3p5().py(header_padding).into_any_element()
            })
            .children(error.then(|| {
                div()
                    .w_full()
                    .px_3p5()
                    .pb_2()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(summary)
            }));
        if expanded && expandable {
            let mut updates = v_flex().w_full().min_w_0().gap_3().px_3p5().pb_3();
            for id in history {
                updates = updates.child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .children(conversation.item(id).and_then(|item| item.created_at).map(
                            |at| {
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(super::message_row::stamp(at))
                            },
                        ))
                        .children(self.task_message(id, conversation.reaction_for(id), cx)),
                );
            }
            activity = activity.child(updates);
        }
        let reply = if show_reply {
            if let Some(id) = task.latest {
                Some(
                    div()
                        .w_full()
                        .px_3p5()
                        .py_2()
                        .when(!complete, |reply| {
                            reply
                                .rounded(theme.radius_lg)
                                .border_1()
                                .border_color(theme.border.opacity(0.6))
                                .bg(theme.sidebar.opacity(0.6))
                        })
                        .children(self.task_message(id, conversation.reaction_for(id), cx)),
                )
            } else if complete {
                Some(
                    div()
                        .px_3p5()
                        .py_2()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Finished without a text reply."),
                )
            } else {
                None
            }
        } else {
            None
        };
        let bubble = v_flex()
            .w_full()
            .min_w_0()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border.opacity(0.6))
            .bg(theme.sidebar.opacity(0.6));
        let mut card = v_flex().relative().w_full().min_w_0().gap_2();
        card = if complete {
            card.child(
                bubble
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .px_3p5()
                            .pt_3()
                            .pb_1()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .children(bot.map(|bot| {
                                bot_avatar(
                                    SharedString::from(format!("task-avatar-{}", task.turn)),
                                    bot.avatar,
                                    px(24.),
                                )
                            }))
                            .child(div().flex_1().min_w_0().truncate().child(title)),
                    )
                    .children(reply)
                    .child(
                        div()
                            .w_full()
                            .mt_2()
                            .border_t_1()
                            .border_color(theme.border.opacity(0.6))
                            .child(activity),
                    ),
            )
        } else {
            card.child(bubble.child(activity)).children(reply)
        };
        for &id in &task.kept {
            card = card.child(
                v_flex()
                    .w_full()
                    .gap_2()
                    .px_3p5()
                    .py_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border.opacity(0.6))
                    .bg(theme.sidebar.opacity(0.6))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Kept from this task"),
                    )
                    .children(self.task_message(id, conversation.reaction_for(id), cx)),
            );
        }
        let animate = !cx.reduce_motion()
            && view
                .reply_transitions
                .get(&task.turn)
                .is_some_and(|at| at.elapsed() < Duration::from_millis(320));
        let card = if animate {
            card.with_animation(
                SharedString::from(format!("task-settled-{}", task.turn)),
                Animation::new(Duration::from_millis(280)).with_max_fps(60.),
                |card, t| {
                    let ease = 1. - (1. - t).powi(3);
                    card.opacity(0.3 + 0.7 * ease).top(px(5. * (1. - ease)))
                },
            )
            .into_any_element()
        } else {
            card.into_any_element()
        };
        Some(card)
    }

    fn task_message(
        &self,
        id: Uuid,
        reaction: Option<crate::model::ReactionKind>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
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
            .child(div().w_full().pr(px(64.)).child(text.render(cx)))
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
        Some(match reaction {
            Some(kind) => super::message_row::with_reaction(body.into_any_element(), kind, cx),
            None => body.into_any_element(),
        })
    }
}

/// A light that sweeps under a working task's text, one lap per 1.7 s like the avatar's halo.
fn work_sweep(turn: Uuid, color: Hsla, still: bool) -> impl IntoElement {
    let light = div()
        .absolute()
        .top_0()
        .h_full()
        .w(relative(0.4))
        .rounded_full()
        .bg(color);
    div()
        .relative()
        .w_full()
        .h(px(2.))
        .mt_1p5()
        .rounded_full()
        .overflow_hidden()
        .bg(color.opacity(0.08))
        .child(if still {
            light.left(relative(0.3)).opacity(0.4).into_any_element()
        } else {
            light
                .with_animation(
                    SharedString::from(format!("task-sweep-{turn}")),
                    Animation::new(Duration::from_millis(1700)).repeat(),
                    |light, t| {
                        light
                            .left(relative(1.4 * t - 0.4))
                            .opacity((std::f32::consts::PI * t).sin())
                    },
                )
                .into_any_element()
        })
}
