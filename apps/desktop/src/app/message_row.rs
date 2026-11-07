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
        if let RowKey::Item(id) = row.key
            && let Some(view) = self.view.as_ref()
            && let Some(conversation) = view.conversation.as_ref()
            && let Some(item) = conversation.item(id)
            && conversation.is_feedback_acknowledgement(item)
        {
            let bot = view.bot_id;
            let position = crate::api::ReplyPosition {
                item_seq: item.seq,
                text_bytes: item.text.len() as i64,
            };
            let owner = cx.entity().downgrade();
            return div()
                .w_full()
                .h_0()
                .on_prepaint(move |bounds, window, cx| {
                    let owner = owner.clone();
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
                .into_any_element();
        }
        let body = if row.activity.is_some() {
            self.render_bot_activity(&row, cx)
        } else if let Some(task) = row.task.as_ref() {
            self.render_task_reply(task, cx)
        } else {
            self.render_row_content(&row, cx)
        };
        let Some(body) = body else {
            return div().into_any_element();
        };
        let column = super::view::reading_column();
        let Some(group) = row.routine else {
            let body = if let Some(task) = row.task.as_ref() {
                gpui_kit::component::v_flex()
                    .w_full()
                    .pr(px(60.))
                    .gap_2()
                    .children(
                        self.view
                            .as_ref()
                            .and_then(|view| view.conversation.as_ref())
                            .and_then(|conversation| conversation.turn(task.turn))
                            .and_then(|turn| turn.routine.as_ref())
                            .map(|run| {
                                h_flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(Icon::new(IconName::Clock).xsmall().flex_none())
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(format!("Scheduled routine · {}", run.name)),
                                    )
                                    .child(stamp(run.due_at))
                            }),
                    )
                    .child(body)
                    .children(row.footer_stamp.map(|at| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(stamp(at))
                    }))
                    .into_any_element()
            } else {
                body
            };
            return column
                .py_1p5()
                .children(row.unread_start.then(|| unread_divider(cx)))
                .child(body)
                .into_any_element();
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
        let follows_task = ix
            .checked_sub(1)
            .and_then(|ix| view.rows.get(ix))
            .is_some_and(|row| row.key == RowKey::Task(group.turn));
        let theme = cx.theme().clone();
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
                    .filter(|_| group.first && !follows_task)
                    .map(|run| routine_banner(run, cx)),
            )
            .child(div().w_full().px_1().pt_1().child(body))
            .when(group.last, |frame| frame.pb_1());
        column
            .when(group.first, |column| column.pt_1p5())
            .when(group.last, |column| column.pb_1p5())
            .children(row.unread_start.then(|| unread_divider(cx)))
            .child(frame)
            .into_any_element()
    }

    pub(super) fn render_turn_status(
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
                        .child(label),
                ),
        )
    }

    pub(super) fn render_row_content(
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
                    .child(text.render(cx))
            } else {
                card.px_3p5()
                    .py_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border.opacity(0.6))
                    .bg(theme.sidebar.opacity(0.6))
                    .child(text.render(cx))
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
                let conversation = view.conversation.as_ref()?;
                let item = conversation.item(id)?;
                if let Some(update) = &item.routine_update {
                    return Some(
                        h_flex()
                            .w_full()
                            .justify_center()
                            .items_center()
                            .gap_1p5()
                            .py_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Updated routine")
                            .child(Icon::new(IconName::Clock).size_3p5())
                            .child(div().min_w_0().truncate().child(update.name.clone()))
                            .into_any_element(),
                    );
                }
                if let Some(created) = &item.bot_created {
                    return Some(self.render_bot_created(id, created, footer, cx));
                }
                let text = view.texts.get(&key)?;
                let user = item.kind == "user";
                let turn = conversation.turn(item.turn_id);
                let run = conversation.run(id);
                if item.reaction.is_some() {
                    return None;
                }
                // Children stretch so bubbles get a definite width; notes
                // are aligned to the bubble's side.
                let mut column = gpui_kit::component::v_flex().w_full().gap_1();
                let mut card = card(text, user).id(SharedString::from(format!("message-{id}")));
                if item.bot_task.is_some() || item.bot_reply.is_some() {
                    card = self
                        .render_bot_message(item, text, cx)?
                        .id(SharedString::from(format!("message-{id}")));
                } else if !user
                    && run.first
                    && turn.and_then(|t| t.routine.as_ref()).is_none()
                    && let Some(bot) = self.bots.iter().find(|b| b.id == view.bot_id)
                {
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
                    Some(kind) => with_reaction(card, kind, cx),
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
                column.into_any_element()
            }
            RowKey::Task(id) => {
                let turn = view.conversation.as_ref()?.turn(id)?;
                gpui_kit::component::v_flex()
                    .w_full()
                    .gap_1()
                    .child(self.render_turn_status(turn, view.bot_id, cx)?)
                    .children(footer.map(|at| note(at, false)))
                    .into_any_element()
            }
            RowKey::SecretRequest(id) => self.render_secret_card(id, cx),
            RowKey::Question(id) => self.render_question_card(id, cx),
            RowKey::Draft(id) => self.render_draft_card(id, cx),
            RowKey::SocialSetup(id) => self.render_social_card(id, cx),
            RowKey::Handoff(id) => self.render_handoff(id, cx),
            RowKey::Pending(request_id) => {
                let pending = self
                    .selected
                    .and_then(|bot| self.pending.get(&bot))
                    .and_then(|list| list.iter().find(|p| p.request_id == request_id))?;
                let text = view.texts.get(&key)?;
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

pub(super) fn with_reaction(
    message: AnyElement,
    kind: ReactionKind,
    cx: &mut Context<ElseApp>,
) -> AnyElement {
    const SIZE: f32 = 30.;
    let theme = cx.theme();
    div()
        .relative()
        .w_full()
        .pt(px(SIZE / 2.))
        .pr(px(SIZE / 2.))
        .child(message)
        .child(
            h_flex()
                .absolute()
                .top_0()
                .right_0()
                .size(px(SIZE))
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

pub(super) fn turn_label(turn: &crate::model::Turn) -> (String, bool) {
    let detail = turn
        .error
        .as_ref()
        .and_then(|e| e.message.clone())
        .unwrap_or_default();
    match turn.status.as_str() {
        "queued" => ("Queued".into(), false),
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
        "waiting" if turn.social_setups.iter().any(|setup| setup.is_pending()) => {
            ("Waiting for you to connect an account…".into(), false)
        }
        "waiting" => {
            let mut waiting_on = turn
                .delegations
                .iter()
                .filter(|d| !d.status.settled())
                .map(|d| d.target_name.as_str())
                .collect::<Vec<_>>();
            waiting_on.dedup();
            match waiting_on.as_slice() {
                [] => ("Waiting for another bot…".into(), false),
                [name] => (format!("Waiting for {name}…"), false),
                many => (format!("Waiting for {} bots…", many.len()), false),
            }
        }
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

fn beside(user: bool, note: Div) -> Div {
    let row = h_flex().w_full().child(note);
    if user { row.justify_end() } else { row }
}

fn unread_divider(cx: &App) -> Div {
    let color = cx.theme().primary;
    let line = || div().flex_1().h(px(1.)).bg(color.opacity(0.3));
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .pt_2()
        .pb_4()
        .text_xs()
        .text_color(color)
        .child(line())
        .child(div().flex_none().child("NEW"))
        .child(line())
}

pub(super) fn stamp(at: DateTime<Utc>) -> String {
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
    use super::{stamp_in, turn_label};
    use chrono::{DateTime, FixedOffset, Utc};

    #[test]
    fn social_setup_waits_for_the_owner_to_connect() {
        let turn = serde_json::from_value(serde_json::json!({
            "id": uuid::Uuid::nil(),
            "request_id": uuid::Uuid::nil(),
            "turn_no": 1,
            "provider_id": "synthetic",
            "status": "waiting",
            "error": null,
            "effect_uncertain": false,
            "social_setups": [{
                "id": uuid::Uuid::nil(),
                "provider": "telegram",
                "reason": "Summarize mentions",
                "status": "pending",
                "connection_id": null
            }]
        }))
        .unwrap();
        assert_eq!(
            turn_label(&turn),
            ("Waiting for you to connect an account…".into(), false)
        );
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
