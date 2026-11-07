use super::ElseApp;
use super::task_reply::state::format_elapsed;
use crate::avatar::bot_avatar;
use crate::model::{Delegation, DelegationStatus};
use chrono::Utc;
use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::time::Duration;
use uuid::Uuid;

/// What the card says about a group of tasks sent in one attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Working,
    Replied,
    Failed,
    Stopped,
}

impl Phase {
    fn of(group: &[&Delegation]) -> Self {
        if group.iter().any(|d| !d.status.settled()) {
            Self::Working
        } else if group.iter().any(|d| d.status == DelegationStatus::Failed) {
            Self::Failed
        } else if group
            .iter()
            .all(|d| d.status == DelegationStatus::Cancelled)
        {
            Self::Stopped
        } else {
            Self::Replied
        }
    }
}

fn status_label(status: DelegationStatus) -> &'static str {
    match status {
        DelegationStatus::Queued => "Queued",
        DelegationStatus::Running => "Working",
        DelegationStatus::Completed => "Replied",
        DelegationStatus::Failed => "Failed",
        DelegationStatus::Cancelled => "Stopped",
    }
}

impl ElseApp {
    pub(super) fn render_handoff(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        self.handoff_card(id, cx)
            .unwrap_or_else(|| div().into_any_element())
    }

    fn handoff_card(&self, id: Uuid, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let (_, group) = view.conversation.as_ref()?.handoff(id)?;
        let theme = cx.theme().clone();
        let phase = Phase::of(&group);
        let live = phase == Phase::Working;
        let bot = |id: Uuid| self.bots.iter().find(|bot| bot.id == id);
        let name =
            |d: &Delegation| bot(d.target_bot_id).map_or(d.target_name.clone(), |b| b.name.clone());
        let sender = bot(view.bot_id);
        let sender_name = sender.map_or("This bot".to_owned(), |b| b.name.clone());
        let start = group.iter().map(|d| d.created_at).min()?;
        let end = if live {
            Utc::now()
        } else {
            group
                .iter()
                .filter_map(|d| d.finished_at)
                .max()
                .unwrap_or(start)
        };
        let elapsed = format_elapsed((end - start).num_seconds().max(0));
        let council = group.len() > 1;
        let answered = group.iter().filter(|d| d.status.settled()).count();
        let accent = match phase {
            Phase::Working => theme.primary,
            Phase::Replied => theme.success,
            Phase::Failed => theme.danger,
            Phase::Stopped => theme.muted_foreground,
        };
        let avatar = |d: &Delegation, size: f32| {
            bot(d.target_bot_id).map(|b| {
                let avatar = bot_avatar(
                    SharedString::from(format!("handoff-{id}-{}", d.target_bot_id)),
                    b.avatar,
                    px(size),
                );
                if d.status.settled() {
                    avatar.still()
                } else {
                    avatar.working()
                }
            })
        };

        if !live {
            let (title, detail) = match (council, phase) {
                (true, Phase::Replied) => (
                    "Council answered".to_owned(),
                    format!("{answered} of {}", group.len()),
                ),
                (true, _) => (
                    "Council ended".to_owned(),
                    format!("{answered} of {}", group.len()),
                ),
                (false, Phase::Failed) => {
                    (format!("{} couldn't finish", name(group[0])), String::new())
                }
                (false, Phase::Stopped) => (format!("{} stopped", name(group[0])), String::new()),
                (false, _) => (format!("{} replied", name(group[0])), String::new()),
            };
            let icon = match phase {
                Phase::Failed => IconName::CircleX,
                Phase::Stopped => IconName::CircleOff,
                _ => IconName::CircleCheck,
            };
            return Some(
                h_flex()
                    .id(SharedString::from(format!("handoff-{id}")))
                    .max_w_full()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .pl_1()
                    .pr_3()
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border.opacity(0.6))
                    .bg(theme.sidebar.opacity(0.6))
                    .text_sm()
                    .child(h_flex().flex_none().children(
                        group.iter().take(3).enumerate().filter_map(|(ix, d)| {
                            avatar(d, 20.).map(|a| div().when(ix > 0, |d| d.ml(px(-6.))).child(a))
                        }),
                    ))
                    .child(Icon::new(icon).xsmall().text_color(accent))
                    .child(div().min_w_0().truncate().child(title))
                    .when(!detail.is_empty(), |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child(detail),
                        )
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(elapsed),
                    )
                    .into_any_element(),
            );
        }

        let wire = {
            let line = div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(3.))
                .h(px(2.))
                .rounded_full()
                .bg(theme.border);
            let dot = div()
                .absolute()
                .top_0()
                .size(px(8.))
                .rounded_full()
                .bg(accent);
            let dot = if cx.reduce_motion() {
                dot.left(relative(0.5)).into_any_element()
            } else {
                dot.with_animation(
                    SharedString::from(format!("handoff-wire-{id}")),
                    Animation::new(Duration::from_millis(1600)).repeat(),
                    |dot, t| {
                        let ease = (1. - (t * std::f32::consts::PI).cos()) / 2.;
                        let fade = (t * std::f32::consts::PI).sin().clamp(0., 1.);
                        dot.left(relative(ease * 0.96)).opacity(fade)
                    },
                )
                .into_any_element()
            };
            div()
                .relative()
                .flex_1()
                .h(px(8.))
                .mx_3()
                .child(line)
                .child(dot)
        };
        let targets = if council {
            v_flex()
                .flex_none()
                .gap_1p5()
                .children(group.iter().map(|d| {
                    h_flex()
                        .gap_2()
                        .items_center()
                        .text_xs()
                        .children(avatar(d, 22.))
                        .child(div().font_weight(FontWeight::MEDIUM).child(name(d)))
                        .child(
                            div()
                                .text_color(if d.status == DelegationStatus::Failed {
                                    theme.danger
                                } else {
                                    theme.muted_foreground
                                })
                                .child(status_label(d.status)),
                        )
                }))
                .into_any_element()
        } else {
            div()
                .flex_none()
                .children(avatar(group[0], 40.))
                .into_any_element()
        };
        let title = if council {
            "The council is working on it".to_owned()
        } else if group[0].status == DelegationStatus::Queued {
            format!("Handing off to {}", name(group[0]))
        } else {
            format!("{} is on it", name(group[0]))
        };
        let footer = if council {
            format!("{answered} of {} answered", group.len())
        } else {
            format!("{sender_name} continues when {} replies", name(group[0]))
        };
        let open = (!council)
            .then(|| bot(group[0].target_bot_id))
            .flatten()
            .map(|b| {
                let target = b.id;
                Button::new(SharedString::from(format!("handoff-open-{id}")))
                    .ghost()
                    .xsmall()
                    .focus_ring(false)
                    .cursor_pointer()
                    .label("Open chat")
                    .accessibility_label(format!("Open chat with {}", b.name))
                    .on_click(
                        cx.listener(move |app, _, window, cx| app.open_bot(target, window, cx)),
                    )
            });
        Some(
            v_flex()
                .id(SharedString::from(format!("handoff-{id}")))
                .w_full()
                .rounded(theme.radius_lg)
                .border_1()
                .border_color(theme.border.opacity(0.6))
                .bg(theme.sidebar.opacity(0.6))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .px_3p5()
                        .pt_3p5()
                        .pb_2p5()
                        .children(sender.map(|b| {
                            bot_avatar(
                                SharedString::from(format!("handoff-from-{id}")),
                                b.avatar,
                                px(26.),
                            )
                            .still()
                        }))
                        .child(wire)
                        .child(targets),
                )
                .child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .px_3p5()
                        .pb_3()
                        .child(
                            h_flex()
                                .w_full()
                                .min_w_0()
                                .items_baseline()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(title),
                                )
                                .child(
                                    div()
                                        .flex_none()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(elapsed),
                                ),
                        )
                        .child(
                            div()
                                .w_full()
                                .text_sm()
                                .line_height(rems(1.25))
                                .max_h(rems(2.5))
                                .overflow_hidden()
                                .text_color(theme.muted_foreground)
                                .child(
                                    group[0]
                                        .task
                                        .split_whitespace()
                                        .collect::<Vec<_>>()
                                        .join(" "),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap_2()
                        .px_3p5()
                        .py_2()
                        .border_t_1()
                        .border_color(theme.border.opacity(0.6))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(footer),
                        )
                        .children(open),
                )
                .into_any_element(),
        )
    }
}
