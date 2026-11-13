use super::ElseApp;
use super::conversation::RowKey;
use super::task_reply::state::format_elapsed;
use crate::model::{Delegation, DelegationStatus};
use chrono::Utc;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::component::{Disableable as _, FocusableExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

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
            .all(|d| d.status == DelegationStatus::Completed)
        {
            Self::Replied
        } else {
            Self::Stopped
        }
    }
}

impl ElseApp {
    pub(super) fn render_handoff(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        self.handoff_row(id, cx)
            .unwrap_or_else(|| div().into_any_element())
    }

    fn handoff_row(&self, id: Uuid, cx: &mut Context<Self>) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let (_, group) = view.conversation.as_ref()?.handoff(id)?;
        let theme = cx.theme();
        let phase = Phase::of(&group);
        let expanded = view.expanded_councils.contains(&id);
        let name = |d: &Delegation| {
            self.bots
                .iter()
                .find(|b| b.id == d.target_bot_id)
                .map_or(d.target_name.clone(), |b| b.name.clone())
        };
        let start = group.iter().map(|d| d.created_at).min()?;
        let end = if phase == Phase::Working {
            Utc::now()
        } else {
            group
                .iter()
                .filter_map(|d| d.finished_at)
                .max()
                .unwrap_or(start)
        };
        let elapsed = format_elapsed((end - start).num_seconds().max(0));
        let replied = group
            .iter()
            .filter(|d| d.status == DelegationStatus::Completed)
            .count();
        let label = if group.len() > 1 {
            "Council".to_owned()
        } else {
            name(group[0])
        };
        let status = match phase {
            Phase::Working => "Working",
            Phase::Replied => "Replied",
            Phase::Failed => "Failed",
            Phase::Stopped => "Stopped",
        };
        let faces = self.face_stack(
            &format!("handoff-{id}"),
            group.iter().map(|d| d.target_bot_id),
            px(16.),
        );
        let mut row = super::action_card::event_row(theme)
            .children(faces)
            .child(
                Button::new(SharedString::from(format!("handoff-details-{id}")))
                    .ghost()
                    .small()
                    .focus_ring(false)
                    .cursor_pointer()
                    .flex_1()
                    .min_w_0()
                    .justify_start()
                    .accessibility_label(format!(
                        "{label}: {status}. {} tasks. Expand details",
                        group.len()
                    ))
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_2()
                            .child(
                                div()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(label),
                            )
                            .child(div().text_color(theme.muted_foreground).child(status))
                            .child(
                                Icon::new(if expanded {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .xsmall(),
                            ),
                    )
                    .on_click(cx.listener(move |app, _, _, cx| {
                        if let Some(view) = app.view.as_mut()
                            && !view.expanded_councils.remove(&id)
                        {
                            view.expanded_councils.insert(id);
                        }
                        app.remeasure_row(RowKey::Handoff(id), cx);
                    })),
            )
            .when(group.len() > 1, |row| {
                row.child(format!("{replied}/{}", group.len()))
            })
            .child(div().flex_none().text_xs().child(elapsed));
        if group.len() == 1
            && let Some(bot) = self
                .bots
                .iter()
                .find(|bot| bot.id == group[0].target_bot_id)
        {
            let target = bot.id;
            row = row.child(
                Button::new(SharedString::from(format!("handoff-open-{id}")))
                    .ghost()
                    .small()
                    .focus_ring(false)
                    .cursor_pointer()
                    .label("Open chat")
                    .accessibility_label(format!("Open chat with {}", bot.name))
                    .on_click(
                        cx.listener(move |app, _, window, cx| app.open_bot(target, window, cx)),
                    ),
            );
        }
        let mut content = v_flex().w_full().min_w_0().child(row);
        if expanded {
            let mut details = v_flex().w_full().min_w_0().gap_3().p_4();
            for delegation in group {
                let target = delegation.target_bot_id;
                let state = match delegation.status {
                    DelegationStatus::Queued => "Queued",
                    DelegationStatus::Running => "Working",
                    DelegationStatus::Completed => "Replied",
                    DelegationStatus::Failed => "Failed",
                    DelegationStatus::Cancelled => "Stopped",
                };
                details = details.child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_sm()
                                        .child(format!("{} · {state}", name(delegation))),
                                )
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "handoff-member-{id}-{target}"
                                    )))
                                    .small()
                                    .ghost()
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .label("Open chat")
                                    .accessibility_label(format!(
                                        "Open chat with {}",
                                        name(delegation)
                                    ))
                                    .disabled(!self.bots.iter().any(|bot| bot.id == target))
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.open_bot(target, window, cx)
                                    })),
                                ),
                        )
                        .child(
                            div()
                                .text_sm()
                                .whitespace_normal()
                                .child(delegation.task.clone()),
                        )
                        .children(delegation.progress.as_ref().map(|progress| {
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .whitespace_normal()
                                .child(progress.clone())
                        }))
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("{} updates", delegation.updates)),
                        ),
                );
            }
            content = content.child(details);
        }
        Some(content.into_any_element())
    }
}
