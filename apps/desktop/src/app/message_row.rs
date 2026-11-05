use super::reply::Author;
use super::row_text::RowText;
use super::{ElseApp, RowKey};
use crate::avatar::bot_avatar;
use crate::model::ReactionKind;
use chrono::{DateTime, Datelike as _, Local, TimeZone, Utc};
use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::time::Duration;

impl ElseApp {
    pub(super) fn render_row(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(row) = self
            .view
            .as_ref()
            .and_then(|view| view.rows.get(ix))
            .cloned()
        else {
            return div().into_any_element();
        };
        let Some(body) = self.render_row_content(&row, cx) else {
            return div().into_any_element();
        };
        let column = super::view::reading_column();
        let Some(group) = row.routine else {
            return column.py_1p5().child(body).into_any_element();
        };
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let Some(conversation) = view.conversation.as_ref() else {
            return div().into_any_element();
        };
        let Some(turn) = conversation.turn(group.turn) else {
            return div().into_any_element();
        };
        let theme = cx.theme().clone();
        let footer = if group.tail {
            let at = row.footer_stamp.map(stamp);
            let status = self.render_turn_status(turn, view.bot_id, cx);
            (status.is_some() || at.is_some()).then(|| {
                h_flex()
                    .w_full()
                    .items_center()
                    .flex_wrap()
                    .gap_2()
                    .px_3p5()
                    .pt_2()
                    .pb_2p5()
                    .children(status)
                    .child(div().flex_1())
                    .children(
                        at.map(|at| div().text_xs().text_color(theme.muted_foreground).child(at)),
                    )
            })
        } else {
            None
        };
        let frame = gpui_kit::component::v_flex()
            .w_full()
            .border_l_1()
            .border_r_1()
            .border_color(theme.primary.opacity(0.25))
            .bg(theme.sidebar.opacity(0.6))
            .when(group.first, |frame| {
                frame.border_t_1().rounded_t(theme.radius_lg)
            })
            .when(group.last, |frame| {
                frame.border_b_1().rounded_b(theme.radius_lg)
            })
            .children(
                turn.routine
                    .as_ref()
                    .filter(|_| group.first)
                    .map(|run| routine_banner(run, cx)),
            )
            .child(div().w_full().px_1().pt_1().child(body))
            .children(footer)
            .when(group.last, |frame| frame.pb_1());
        column
            .when(group.first, |column| column.pt_1p5())
            .when(group.last, |column| column.pb_1p5())
            .child(frame)
            .into_any_element()
    }

    fn render_turn_status(
        &self,
        turn: &crate::model::Turn,
        bot: uuid::Uuid,
        cx: &mut Context<Self>,
    ) -> Option<Div> {
        let (label, error) = turn_label(turn);
        if label.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let active = matches!(turn.status.as_str(), "preparing" | "sending" | "running");
        Some(
            h_flex()
                .gap_1p5()
                .items_center()
                .children(self.bots.iter().find(|b| active && b.id == bot).map(|bot| {
                    bot_avatar(
                        SharedString::from(format!("working-{}", turn.id)),
                        bot.avatar,
                        px(20.),
                    )
                    .working()
                }))
                .child(
                    h_flex()
                        .text_xs()
                        .text_color(if error {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        })
                        .child(label)
                        .children(active.then(|| working_dots(turn.id))),
                ),
        )
    }

    fn render_row_content(
        &mut self,
        row: &super::conversation::TranscriptRow,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        let key = row.key;
        let grouped = row.routine.is_some();
        let footer = row.footer_stamp.filter(|_| !grouped).map(stamp);
        let theme = cx.theme().clone();
        let card = |text: &RowText, user: bool| {
            let card = div().w_full();
            if user {
                card.px_3p5()
                    .py_2()
                    .rounded(theme.radius_lg)
                    .bg(theme.secondary)
                    .child(text.render())
            } else {
                card.px_3p5()
                    .py_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border.opacity(0.6))
                    .bg(theme.sidebar.opacity(0.6))
                    .child(text.render())
            }
        };
        // Wrapped text needs a definite width.
        let side = |user: bool, card: AnyElement| {
            let side = div().w_full();
            let side = if grouped {
                side
            } else if user {
                side.pl(relative(0.15))
            } else {
                side.pr(px(60.))
            };
            side.child(card)
        };
        let reacted = |card: AnyElement, kind: ReactionKind| {
            const HEIGHT: f32 = 30.;
            div()
                .relative()
                .w_full()
                .pt(px(HEIGHT / 2.))
                .child(card)
                .child(
                    h_flex()
                        .absolute()
                        .top_0()
                        .right_0()
                        .h(px(HEIGHT))
                        .min_w(px(HEIGHT + 4.))
                        .px_1p5()
                        .justify_center()
                        .rounded_full()
                        .bg(theme.secondary)
                        .border_1()
                        .border_color(theme.border)
                        .text_size(px(16.))
                        .line_height(px(20.))
                        .child(kind.mark()),
                )
                .into_any_element()
        };
        let note = |text: String, error: bool| {
            div()
                .text_xs()
                .text_color(if error {
                    theme.danger
                } else {
                    theme.muted_foreground
                })
                .child(text)
        };
        Some(match key {
            RowKey::Item(id) => {
                let Some(conversation) = view.conversation.as_ref() else {
                    return None;
                };
                let Some(item) = conversation.item(id) else {
                    return None;
                };
                let Some(text) = view.texts.get(&key) else {
                    return None;
                };
                let user = item.kind == "user";
                let turn = conversation.turn(item.turn_id);
                let run = conversation.run(id);
                let status = turn
                    .filter(|_| row.routine.is_none())
                    .filter(|turn| {
                        conversation
                            .items()
                            .rev()
                            .find(|i| i.turn_id == turn.id)
                            .is_some_and(|i| i.id == id)
                    })
                    .and_then(|turn| self.render_turn_status(turn, view.bot_id, cx));
                // A reaction's feedback prompt is generated for the bot. The
                // badge on the reacted message stands for it.
                if item.reaction.is_some() {
                    return status.map(|status| {
                        gpui_kit::component::v_flex()
                            .w_full()
                            .child(status)
                            .into_any_element()
                    });
                }
                // Children stretch so bubbles get a definite width; notes
                // are aligned to the bubble's side.
                let mut column = gpui_kit::component::v_flex().w_full().gap_1();
                let mut card = card(text, user).id(SharedString::from(format!("message-{id}")));
                let exchange = item
                    .bot_task
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
                    });
                if let Some((sender_id, sender_name, kind)) = exchange {
                    let sender = self.bots.iter().find(|b| b.id == sender_id);
                    let recipient = self.bots.iter().find(|b| b.id == view.bot_id);
                    let label = h_flex()
                        .gap_1p5()
                        .items_center()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.muted_foreground)
                        .children(
                            sender.map(|b| bot_avatar("task-sender", b.avatar, px(18.)).still()),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(format!(
                            "{} → {} · {kind}",
                            sender_name,
                            recipient.map_or("Bot", |b| b.name.as_str())
                        )));
                    card = div()
                        .id(SharedString::from(format!("message-{id}")))
                        .w_full()
                        .p_3()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.primary.opacity(0.3))
                        .bg(theme.primary.opacity(0.05))
                        .child(label)
                        .children(item.bot_reply.as_ref().map(|reply| {
                            div()
                                .pt_2()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(format!("Task: {}", reply.prompt))
                        }))
                        .child(div().pt_2().child(
                            if item.bot_reply.is_some() && item.text.trim().is_empty() {
                                div().child("Finished without a reply.").into_any_element()
                            } else {
                                text.render()
                            },
                        ));
                } else if !user && run.first && turn.and_then(|t| t.routine.as_ref()).is_none() {
                    if let Some(bot) = self.bots.iter().find(|b| b.id == view.bot_id) {
                        column = column.child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(bot_avatar("message-author", bot.avatar, px(18.)).still())
                                .child(bot.name.clone()),
                        );
                    }
                }
                if grouped {
                    card = card
                        .border_0()
                        .rounded(theme.radius)
                        .bg(theme.secondary.opacity(0.4))
                        .pr(px(60.));
                }
                let card = if conversation
                    .latest_reply()
                    .is_some_and(|reply| reply.id == id)
                {
                    let owner = cx.entity().downgrade();
                    let bot = view.bot_id;
                    let position = crate::api::ReplyPosition {
                        item_seq: item.seq,
                        text_bytes: text.rendered_bytes(cx).unwrap_or(0) as i64,
                    };
                    card.relative().on_prepaint(move |bounds, window, cx| {
                        window.defer(cx, move |window, cx| {
                            let _ = owner.update(cx, |app, cx| {
                                app.reply_laid_out(
                                    bot,
                                    super::conversation::ReplyLayout { position, bounds },
                                    window,
                                    cx,
                                );
                            });
                        });
                    })
                } else {
                    card
                };
                let card = match Author::of(&item.kind) {
                    Some(author) => {
                        let (owner, bot) = (cx.entity().downgrade(), view.bot_id);
                        card.context_menu(move |menu, window, cx| {
                            Self::message_menu(owner.clone(), bot, id, author, menu, window, cx)
                        })
                        .into_any_element()
                    }
                    None => card.into_any_element(),
                };
                let card = match conversation.reaction_for(id).filter(|_| !user) {
                    Some(kind) => reacted(card, kind),
                    None => card,
                };
                let card = match Author::of(&item.kind) {
                    Some(author) => div()
                        .relative()
                        .group("message-bubble")
                        .w_full()
                        .child(side(user, card))
                        .child(
                            div()
                                .absolute()
                                .top_1()
                                .when(user, |this| this.left(relative(0.15)).ml(px(-28.)))
                                .when(!user, |this| this.right(px(4.)))
                                .invisible()
                                .group_hover("message-bubble", |this| this.visible())
                                .child(self.message_actions(view.bot_id, id, author, cx)),
                        )
                        .into_any_element(),
                    None => side(user, card).into_any_element(),
                };
                column = column
                    .child(card)
                    .children(footer.map(|at| beside(user, note(at, false))));
                column = column.children(status);
                column.into_any_element()
            }
            RowKey::SecretRequest(id) => self.render_secret_card(id, cx),
            RowKey::Question(id) => self.render_question_card(id, cx),
            RowKey::Draft(id) => self.render_draft_card(id, cx),
            RowKey::Pending(request_id) => {
                let Some(pending) = self
                    .selected
                    .and_then(|bot| self.pending.get(&bot))
                    .and_then(|list| list.iter().find(|p| p.request_id == request_id))
                else {
                    return None;
                };
                let Some(text) = view.texts.get(&key) else {
                    return None;
                };
                let mark = pending.reaction.map(|r| r.kind.mark());
                let mut column = gpui_kit::component::v_flex().w_full().gap_1();
                if mark.is_none() {
                    column = column
                        .child(side(
                            true,
                            card(text, true)
                                .id(SharedString::from(format!("pending-{request_id}")))
                                .into_any_element(),
                        ))
                        .children(footer.map(|at| beside(true, note(at, false))));
                }
                let subject = mark.map_or_else(String::new, |mark| format!("{mark} "));
                if let Some(reason) = &pending.rejected {
                    return Some(
                        column
                            .child(note(format!("{subject}Not sent. {reason}"), true).w_full())
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_end()
                                    .gap_2()
                                    .children(mark.is_none().then(|| {
                                        Button::new(SharedString::from(format!(
                                            "restore-{request_id}"
                                        )))
                                        .focus_ring(false)
                                        .cursor_pointer()
                                        .label("Restore to composer")
                                        .xsmall()
                                        .on_click(
                                            cx.listener(move |app, _, window, cx| {
                                                app.take_rejected(request_id, true, window, cx)
                                            }),
                                        )
                                    }))
                                    .child(
                                        Button::new(SharedString::from(format!(
                                            "dismiss-{request_id}"
                                        )))
                                        .focus_ring(false)
                                        .cursor_pointer()
                                        .label("Dismiss")
                                        .xsmall()
                                        .ghost()
                                        .on_click(
                                            cx.listener(move |app, _, window, cx| {
                                                app.take_rejected(request_id, false, window, cx)
                                            }),
                                        ),
                                    ),
                            )
                            .into_any_element(),
                    );
                }
                column = match &pending.unknown {
                    None => column.child(
                        h_flex()
                            .w_full()
                            .justify_end()
                            .child(note(format!("{subject}Sending…"), false)),
                    ),
                    Some(reason) => column
                        .child(note(format!("{subject}Not confirmed. {reason}"), true).w_full())
                        .child(
                            h_flex().w_full().justify_end().gap_2().child(
                                Button::new(SharedString::from(format!("retry-{request_id}")))
                                    .focus_ring(false)
                                    .cursor_pointer()
                                    .label("Retry")
                                    .xsmall()
                                    .on_click(cx.listener(move |app, _, window, cx| {
                                        app.retry_pending(request_id, window, cx)
                                    })),
                            ),
                        ),
                };
                column.into_any_element()
            }
        })
    }
}

fn routine_banner(run: &crate::model::RoutineRun, cx: &mut Context<ElseApp>) -> AnyElement {
    let theme = cx.theme();
    gpui_kit::component::v_flex()
        .w_full()
        .gap_1p5()
        .px_3p5()
        .py_2p5()
        .rounded_t(theme.radius_lg)
        .bg(theme.primary.opacity(0.07))
        .child(
            h_flex()
                .items_center()
                .gap_2()
                .text_xs()
                .text_color(theme.primary)
                .child(Icon::new(IconName::Clock).size_3p5())
                .child("Scheduled routine")
                .child(
                    div()
                        .flex_1()
                        .text_right()
                        .text_color(theme.muted_foreground)
                        .child(
                            run.due_at
                                .with_timezone(&Local)
                                .format("%e %b · %H:%M %Z")
                                .to_string(),
                        ),
                ),
        )
        .child(div().text_sm().font_semibold().child(run.name.clone()))
        .into_any_element()
}

/// A short public description of a turn's state. Returns (text, is_error).
pub(super) fn turn_label(turn: &crate::model::Turn) -> (String, bool) {
    let detail = turn
        .error
        .as_ref()
        .and_then(|e| e.message.clone())
        .unwrap_or_default();
    match turn.status.as_str() {
        "queued" => ("Queued".into(), false),
        // The status row animates the trailing dots.
        "preparing" | "sending" | "running" => ("Working".into(), false),
        "cancelling" => ("Stopping…".into(), false),
        "waiting" if turn.secret_requests.iter().any(|r| r.is_pending()) => (
            "Waiting for you to answer the secret request…".into(),
            false,
        ),
        "waiting"
            if turn
                .outgoing_drafts
                .iter()
                .any(|d| d.status == crate::model::DraftStatus::Pending) =>
        {
            ("Waiting for your approval…".into(), false)
        }
        "waiting"
            if turn
                .questions
                .iter()
                .any(|q| q.is_pending() && q.is_approval()) =>
        {
            ("Waiting for your approval…".into(), false)
        }
        "waiting" if turn.questions.iter().any(|q| q.is_pending()) => {
            ("Waiting for your answer…".into(), false)
        }
        // The main bot handed part of the request to another bot.
        "waiting" => ("Waiting for another bot…".into(), false),
        "interrupted" => (
            if turn.error.as_ref().is_some_and(|e| e.code == "cancelled") {
                "Stopped before it ran".into()
            } else {
                "Stopped".into()
            },
            false,
        ),
        "failed" => (format!("Failed. {detail}"), true),
        "uncertain" => (format!("Outcome unknown. {detail}"), true),
        _ => (String::new(), false),
    }
}

/// Three dots that brighten in a left-to-right wave after "Working".
fn working_dots(turn: impl std::fmt::Display) -> impl IntoElement {
    h_flex().text_xs().children((0..3).map(move |ix| {
        div().child(".").with_animation(
            SharedString::from(format!("working-dot-{turn}-{ix}")),
            Animation::new(Duration::from_millis(1200))
                .repeat_synced()
                .with_max_fps(30.),
            move |dot, t| dot.opacity(dot_opacity(t, ix)),
        )
    }))
}

/// Opacity of dot `ix` at normalized loop time `t`: dim, with one pulse each loop.
fn dot_opacity(t: f32, ix: usize) -> f32 {
    let phase = (t - ix as f32 * 0.2).rem_euclid(1.);
    let pulse = if phase < 0.6 {
        (phase / 0.6 * std::f32::consts::PI).sin()
    } else {
        0.
    };
    0.25 + 0.75 * pulse
}

fn beside(user: bool, note: Div) -> Div {
    let row = h_flex().w_full().child(note);
    if user { row.justify_end() } else { row }
}

fn stamp(at: DateTime<Utc>) -> String {
    stamp_in(at, Local::now())
}

fn stamp_in<Tz: TimeZone>(at: DateTime<Utc>, now: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let at = at.with_timezone(&now.timezone());
    if at.year() == now.year() {
        at.format("%-d %b, %H:%M").to_string()
    } else {
        at.format("%-d %b %Y, %H:%M").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{dot_opacity, stamp_in};
    use chrono::{DateTime, FixedOffset, Utc};

    #[test]
    fn working_dots_pulse_left_to_right() {
        assert!(dot_opacity(0.3, 0) > 0.99);
        assert!(dot_opacity(0.3, 2) < dot_opacity(0.3, 1));
        assert!(dot_opacity(0.55, 2) > dot_opacity(0.55, 0));
        assert!((0..3).all(|ix| (0..100).all(|t| {
            let o = dot_opacity(t as f32 / 100., ix);
            (0.25..=1.).contains(&o)
        })));
    }

    #[test]
    fn stamps_use_the_viewer_zone_and_name_another_year() {
        let zone = FixedOffset::east_opt(2 * 3600).unwrap();
        let now = "2026-11-04T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .unwrap()
            .with_timezone(&zone);
        let at = |text: &str| text.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(stamp_in(at("2026-11-04T07:05:00Z"), now), "4 Nov, 09:05");
        assert_eq!(stamp_in(at("2026-01-31T23:30:00Z"), now), "1 Feb, 01:30");
        assert_eq!(
            stamp_in(at("2025-12-31T21:59:00Z"), now),
            "31 Dec 2025, 23:59"
        );
    }
}
