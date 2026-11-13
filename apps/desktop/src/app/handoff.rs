use super::ElseApp;
use super::conversation::RowKey;
use super::task_reply::state::format_elapsed;
use crate::controls::{Control as _, Scale, Tier};
use crate::model::{Delegation, DelegationStatus};
use chrono::Utc;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
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
        // "Asked Research", or "Asked council Research & Code Reviewer".
        let (verb, object) = if group.len() > 1 {
            (
                "Asked council",
                group
                    .iter()
                    .map(|d| name(d))
                    .collect::<Vec<_>>()
                    .join(" & "),
            )
        } else {
            ("Asked", label.clone())
        };
        let status = match phase {
            Phase::Working => "Working",
            Phase::Replied => "Replied",
            Phase::Failed => "Failed",
            Phase::Stopped => "Stopped",
        };
        let faces = self.event_faces(
            &format!("handoff-{id}"),
            group.iter().map(|d| d.target_bot_id),
        );
        let mut meta = vec![status.to_owned()];
        if group.len() > 1 {
            meta.push(format!("{replied}/{}", group.len()));
        }
        meta.push(elapsed);
        let meta = meta.join(" · ");
        let row = Button::new(SharedString::from(format!("handoff-details-{id}")))
            .ghost()
            .focus_ring(false)
            .cursor_pointer()
            .w_full()
            .h_auto()
            .p_0()
            .accessibility_label(format!(
                "{label}: {meta}. {} tasks. {} details",
                group.len(),
                if expanded { "Hide" } else { "Show" }
            ))
            .child(
                super::action_card::event_row(theme)
                    .child(faces)
                    .child(super::action_card::event_verb(theme, verb))
                    .child(super::action_card::event_object(theme, object))
                    .child(super::action_card::event_meta(theme, meta))
                    .child(super::action_card::event_chevron(expanded)),
            )
            .on_click(cx.listener(move |app, _, _, cx| {
                if let Some(view) = app.view.as_mut()
                    && !view.expanded_councils.remove(&id)
                {
                    view.expanded_councils.insert(id);
                }
                app.remeasure_row(RowKey::Handoff(id), cx);
            }));
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
                                    .control(
                                        Tier::Ghost,
                                        Scale::Compact,
                                        !self.bots.iter().any(|bot| bot.id == target),
                                        cx,
                                    )
                                    .label("Open chat")
                                    .accessibility_label(format!(
                                        "Open chat with {}",
                                        name(delegation)
                                    ))
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
