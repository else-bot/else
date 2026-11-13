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
        let delegates: Vec<Uuid> =
            if task.state == ReplyState::Waiting && !error && turn.waits_on_bots() {
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
            px(12.),
        );
        let handed_off = delegate_faces.is_some();
        let show_reply =
            task.latest_kept || !matches!(task.state, ReplyState::Queued | ReplyState::Working);
        let mut history = task.history.clone();
        if !show_reply && let Some(id) = task.latest {
            history.push(id);
        }
        let expandable = !history.is_empty();
        let owner_waiting = task.state == ReplyState::Waiting
            && (turn.waits_on_owner()
                || turn
                    .owner_notices
                    .iter()
                    .any(|notice| notice.status == crate::model::OwnerNoticeStatus::Pending));
        let meta = theme.foreground.opacity(super::message_row::META_OPACITY);
        let chevron = || {
            Icon::new(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .xsmall()
            .flex_none()
        };
        let updates = expandable.then(|| {
            let count = history.len();
            format!("{count} {}", if count == 1 { "update" } else { "updates" })
        });
        // Every assistant turn opens with the same name row as a plain reply.
        let name_row = h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap_1p5()
            .text_size(px(13.))
            .children(bot.map(|bot| {
                let avatar = bot_avatar(
                    SharedString::from(format!("task-avatar-{}", task.turn)),
                    bot.avatar,
                    px(18.),
                );
                let avatar = if working {
                    avatar.working()
                } else {
                    avatar.still()
                };
                div()
                    .relative()
                    .flex_none()
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
                            .border_1()
                            .border_color(theme.background)
                            .child(badge)
                    }))
            }))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(title.clone()),
            )
            .child(div().flex_1())
            .children(elapsed.clone().filter(|_| working || error).map(|elapsed| {
                div()
                    .flex_none()
                    .text_size(px(12.))
                    .text_color(meta)
                    .child(elapsed)
            }));
        // One quiet line says where the turn stands; it opens earlier updates when there are any.
        let status = if owner_waiting {
            expandable.then(|| {
                h_flex()
                    .items_center()
                    .gap_1()
                    .text_size(px(12.))
                    .text_color(meta)
                    .child("Earlier updates")
                    .child(chevron())
            })
        } else if complete {
            Some(
                h_flex()
                    .items_center()
                    .gap_1()
                    .text_size(px(12.))
                    .text_color(meta)
                    .child(
                        Icon::new(IconName::Check)
                            .xsmall()
                            .flex_none()
                            .text_color(theme.success),
                    )
                    .child(summary.clone())
                    .children(elapsed.map(|elapsed| format!("· {elapsed}")))
                    .children(updates.map(|updates| format!("· {updates}")))
                    .children(expandable.then(chevron)),
            )
        } else if error {
            Some(
                h_flex()
                    .min_w_0()
                    .items_start()
                    .gap_1p5()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(
                        div().flex_none().pt(px(3.)).child(
                            Icon::new(IconName::CircleX)
                                .size_4()
                                .text_color(theme.danger),
                        ),
                    )
                    .child(div().min_w_0().text_left().child(summary.clone()))
                    .children(updates.map(|updates| {
                        h_flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .pt(px(2.))
                            .text_size(px(12.))
                            .text_color(meta)
                            .child(updates)
                            .child(chevron())
                    })),
            )
        } else {
            let label = task.state.label();
            let detail = if summary != label {
                summary.clone()
            } else {
                label.to_owned()
            };
            Some(
                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap_2()
                    .text_size(px(13.))
                    .text_color(if working {
                        theme.foreground.opacity(0.85)
                    } else {
                        theme.muted_foreground
                    })
                    .when(working, |line| {
                        line.child(working_dots(task.turn, theme.link, cx.reduce_motion()))
                    })
                    .children(delegate_faces)
                    .child(div().min_w_0().truncate().child(detail))
                    .children(updates.map(|updates| {
                        h_flex()
                            .flex_none()
                            .items_center()
                            .gap_1()
                            .text_size(px(12.))
                            .text_color(meta)
                            .child(format!("· {updates}"))
                            .child(chevron())
                    })),
            )
        };
        let status = status.map(|status| {
            if expandable {
                let id = task.turn;
                Button::new(SharedString::from(format!("task-history-{id}")))
                    .ghost()
                    .small()
                    .focus_ring(false)
                    .cursor_pointer()
                    .toggled(expanded)
                    .h_auto()
                    .py_0p5()
                    .px_1p5()
                    .ml(px(-6.))
                    .accessibility_label(format!(
                        "{} activity for {title}. {summary}",
                        if expanded { "Collapse" } else { "Expand" }
                    ))
                    .child(status)
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
                status.into_any_element()
            }
        });
        let earlier = (expanded && expandable).then(|| {
            let mut updates = v_flex()
                .w_full()
                .min_w_0()
                .gap_3()
                .pl_3()
                .border_l_1()
                .border_color(theme.border);
            for id in history {
                updates = updates.child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .children(conversation.item(id).and_then(|item| item.created_at).map(
                            |at| {
                                div()
                                    .text_size(px(12.))
                                    .text_color(meta)
                                    .child(super::message_row::stamp(at))
                            },
                        ))
                        .children(self.task_message(id, conversation.reaction_for(id), cx)),
                );
            }
            updates
        });
        let reply = if show_reply {
            if let Some(id) = task.latest {
                self.task_message(id, conversation.reaction_for(id), cx)
            } else if complete {
                Some(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Finished without a text reply.")
                        .into_any_element(),
                )
            } else {
                None
            }
        } else {
            None
        };
        let mut card = v_flex()
            .relative()
            .w_full()
            .min_w_0()
            .gap_1p5()
            .child(name_row);
        card = if complete {
            card.children(reply).children(earlier).children(status)
        } else {
            card.children(status).children(earlier).children(reply)
        };
        for &id in &task.kept {
            card = card.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .pt_1()
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(meta)
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

/// Three dots that pulse in turn beside a working turn's label, one lap per 1.2 s.
fn working_dots(turn: Uuid, color: Hsla, still: bool) -> impl IntoElement {
    h_flex()
        .flex_none()
        .items_center()
        .gap(px(3.))
        .children((0..3).map(move |ix| {
            let dot = div().size(px(4.)).rounded_full().bg(color);
            if still {
                dot.opacity(0.4 + 0.3 * ix as f32).into_any_element()
            } else {
                dot.with_animation(
                    SharedString::from(format!("task-dots-{turn}-{ix}")),
                    Animation::new(Duration::from_millis(1200)).repeat(),
                    move |dot, t| {
                        let phase = (t - ix as f32 / 6.).rem_euclid(1.);
                        let lift = (std::f32::consts::PI * (phase * 2.).min(1.)).sin();
                        dot.opacity(0.3 + 0.7 * lift)
                    },
                )
                .into_any_element()
            }
        }))
}
