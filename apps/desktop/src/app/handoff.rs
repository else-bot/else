use super::task_reply::state::format_elapsed;
use super::transcript::RowKey;
use crate::app::conversation::ConversationView;
use crate::app::transcript::CardKey;
use crate::controls::{Control as _, Scale, Tier};
use chrono::Utc;
use else_wire::delegations::{Delegation, DelegationStatus};
use gpui_kit::component::button::Button;
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

impl ConversationView {
    pub(super) fn render_handoff(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        self.handoff_row(id, cx)
            .unwrap_or_else(|| div().into_any_element())
    }

    fn handoff_row(&self, id: Uuid, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, group) = self.conversation.as_ref()?.handoff(id)?;
        let theme = cx.theme();
        let phase = Phase::of(&group);
        let expanded = self.expanded_councils.contains(&id);
        let name = |d: &Delegation| {
            self.inputs
                .bots
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
        let row =
            super::action_card::event_button(SharedString::from(format!("handoff-details-{id}")))
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
                        .child(super::action_card::event_end(
                            theme,
                            super::action_card::EventEnd::Meta(meta.into()),
                            if expanded {
                                super::action_card::Disclosure::Expanded
                            } else {
                                super::action_card::Disclosure::Collapsed
                            },
                        )),
                )
                .on_click(cx.listener(move |app, _, _, cx| {
                    if !app.expanded_councils.remove(&id) {
                        app.expanded_councils.insert(id);
                    }
                    app.remeasure_row(RowKey::Card(CardKey::Handoff(id)), cx);
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
                    DelegationStatus::Unknown => "Status unavailable",
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
                                        !self.inputs.bots.iter().any(|bot| bot.id == target),
                                        cx,
                                    )
                                    .label("Open chat")
                                    .accessibility_label(format!(
                                        "Open chat with {}",
                                        name(delegation)
                                    ))
                                    .on_click(self.on_app(move |app, _, window, cx| {
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
