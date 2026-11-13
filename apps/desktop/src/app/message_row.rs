use super::conversation::Spot;
use super::pending_send::Delivery;
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
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, StyledExt as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// Bot-side cards use the full shared reading column.
pub(super) const BOT_GUTTER: Pixels = px(0.);

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
        let sender = self
            .view
            .as_ref()
            .and_then(|view| view.conversation.as_ref())
            .and_then(|c| row.key.task_sender(c));
        let body = match sender {
            Some(sender) => gpui_kit::component::v_flex()
                .w_full()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("From {sender}'s task")),
                )
                .child(body)
                .into_any_element(),
            None => body,
        };
        let body = match self.view.as_ref().and_then(|view| view.revealed.as_ref()) {
            Some(reveal) if row.contains(reveal.row) => match reveal.spot {
                Spot::Row => div()
                    .w_full()
                    .rounded(cx.theme().radius_lg)
                    .bg(cx.theme().primary.opacity(0.16 * reveal.shown))
                    .child(body)
                    .into_any_element(),
                Spot::Passage { .. } => CenterReveal {
                    body,
                    list: self.view.as_ref().map(|view| view.list.clone()),
                }
                .into_any_element(),
            },
            _ => body,
        };
        let column = super::view::reading_column();
        let Some(group) = row.routine else {
            let body = if let Some(task) = row.task.as_ref() {
                gpui_kit::component::v_flex()
                    .w_full()
                    .pr(BOT_GUTTER)
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
                                    .child(div().flex_1().min_w_0().truncate().child(format!(
                                        "{} · {}",
                                        run.label(),
                                        run.name
                                    )))
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
            .child(div().w_full().pr(BOT_GUTTER).child(frame))
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
            let card = div().w_full().min_w_0();
            if user {
                card.px_3p5()
                    .py_2()
                    .rounded(theme.radius_lg)
                    .bg(theme.secondary)
                    .child(text.render(cx))
            } else {
                card.py_1().child(text.render(cx))
            }
        };
        let side = |card: AnyElement| div().w_full().min_w_0().child(card);
        let note = |text: String, error: bool| {
            div()
                .text_size(px(12.))
                .text_color(if error {
                    theme.danger
                } else {
                    theme.foreground.opacity(0.48)
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
                if let Some(update) = &item.rules_update {
                    return Some(self.render_rules_update(id, view.bot_id, update, footer, cx));
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
                let mut column = gpui_kit::component::v_flex().w_full().min_w_0().gap_1();
                let card = card(text, user).id(SharedString::from(format!("message-{id}")));
                if !user
                    && run.first
                    && turn.and_then(|t| t.routine.as_ref()).is_none()
                    && let Some(bot) = self.bots.iter().find(|b| b.id == view.bot_id)
                {
                    column = column.child(
                        h_flex()
                            .gap_1p5()
                            .items_center()
                            .text_size(px(13.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(bot_avatar("message-author", bot.avatar, px(18.)).still())
                            .child(bot.name.clone()),
                    );
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
                        .child(side(card))
                        .child(
                            div()
                                .absolute()
                                .top_1()
                                .when(user, |this| this.left(px(-28.)))
                                .when(!user, |this| this.right(px(4.)))
                                .invisible()
                                .group_hover("message-bubble", |this| this.visible())
                                .child(self.message_actions(view.bot_id, id, author, cx)),
                        )
                        .into_any_element(),
                    None => side(card).into_any_element(),
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
            RowKey::SkillInstall(id) => side(self.render_skill_card(id, cx)).into_any_element(),
            RowKey::SecretRequest(id) => side(self.render_secret_card(id, cx)).into_any_element(),
            RowKey::Question(id) => side(self.render_question_card(id, cx)).into_any_element(),
            RowKey::Draft(id) => side(self.render_draft_card(id, cx)).into_any_element(),
            RowKey::SocialSetup(id) => side(self.render_social_card(id, cx)).into_any_element(),
            RowKey::Handoff(id) => self.render_handoff(id, cx),
            RowKey::ComputerHandoff(id) => self.render_handoff_row(id, cx),
            RowKey::OwnerNotice(id) => side(self.render_notice_card(id, cx)?).into_any_element(),
            RowKey::Pending(request_id) => {
                let pending = self
                    .selected
                    .and_then(|bot| self.pending.get(&bot))
                    .and_then(|list| list.iter().find(|p| p.request_id == request_id))?;
                let text = view.texts.get(&key)?;
                let mark = pending.reaction.map(|r| r.kind.mark());
                let settled = matches!(
                    pending.delivery(),
                    Delivery::Unknown { .. } | Delivery::Rejected { .. }
                );
                let mut column = gpui_kit::component::v_flex().w_full().min_w_0().gap_1();
                if mark.is_none() {
                    column = column.child(side(
                        card(text, true)
                            .id(SharedString::from(format!("pending-{request_id}")))
                            .into_any_element(),
                    ));
                    if !settled {
                        column = column.children(footer.map(|at| beside(true, note(at, false))));
                    }
                }
                let subject = mark.map_or_else(String::new, |mark| format!("{mark} "));
                let action = |id: &str, label: &'static str| {
                    Button::new(SharedString::from(format!("{id}-{request_id}")))
                        .focus_ring(false)
                        .cursor_pointer()
                        .link()
                        .xsmall()
                        .label(label)
                };
                let failed = |status: &str, color: Hsla, reason: &str, retry: bool| {
                    h_flex()
                        .flex_wrap()
                        .justify_end()
                        .items_center()
                        .gap_x_2()
                        .text_xs()
                        .child(div().flex_none().size(px(6.)).rounded_full().bg(color))
                        .child({
                            let reason = SharedString::from(reason.to_owned());
                            div()
                                .id(SharedString::from(format!("failure-{request_id}")))
                                .text_color(color)
                                .child(format!("{subject}{status}"))
                                .tooltip(move |window, cx| {
                                    Tooltip::new(reason.clone()).build(window, cx)
                                })
                        })
                        .child(
                            h_flex()
                                .flex_none()
                                .gap_x_2()
                                .child(div().text_color(theme.muted_foreground).child("·"))
                                .children(retry.then(|| {
                                    action("retry", "Retry").on_click(cx.listener(
                                        move |app, _, window, cx| {
                                            app.retry_pending(request_id, window, cx)
                                        },
                                    ))
                                }))
                                .children((mark.is_none() && !retry).then(|| {
                                    action("restore", "Edit").on_click(cx.listener(
                                        move |app, _, window, cx| {
                                            app.take_failed(request_id, true, window, cx)
                                        },
                                    ))
                                }))
                                .child(action("dismiss", "Dismiss").on_click(cx.listener(
                                    move |app, _, window, cx| {
                                        app.take_failed(request_id, false, window, cx)
                                    },
                                ))),
                        )
                };
                let line = match pending.delivery() {
                    Delivery::Sending => note(format!("{subject}Sending…"), false),
                    Delivery::Resolving { .. } => {
                        note(format!("{subject}Checking delivery…"), false)
                    }
                    Delivery::Unknown { reason } => {
                        failed("Not confirmed", theme.warning, reason, true)
                    }
                    Delivery::Rejected { reason } => {
                        failed("Not sent", theme.danger, reason, false)
                    }
                };
                column = column.child(beside(true, line));
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
                .child(run.label())
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
    use crate::model::QuestionUse;
    let (label, error) = status_label(turn);
    if !matches!(turn.status.as_str(), "failed" | "uncertain" | "interrupted") {
        return (label, error);
    }
    let used = |pick: fn(&QuestionUse) -> bool| {
        turn.questions
            .iter()
            .any(|q| q.used.as_ref().is_some_and(pick))
    };
    let note = if used(|u| matches!(u, QuestionUse::Completed { .. })) {
        "An approved action had already run."
    } else if used(|u| matches!(u, QuestionUse::Started { .. })) {
        "An approved action had started; its outcome is unknown."
    } else {
        return (label, error);
    };
    let label = label.trim_end().trim_end_matches('.');
    (format!("{label}. {note}"), error)
}

fn status_label(turn: &crate::model::Turn) -> (String, bool) {
    let detail = turn
        .error
        .as_ref()
        .and_then(|e| e.message.clone())
        .unwrap_or_default();
    match turn.status.as_str() {
        "queued" => ("Queued".into(), false),
        "preparing" | "sending" | "running" => ("Working".into(), false),
        "waiting" if turn.waits_on_job() => ("Working".into(), false),
        "cancelling" => ("Stopping…".into(), false),
        "waiting"
            if turn
                .skill_installs
                .iter()
                .any(|r| r.status == crate::model::SkillInstallStatus::Pending) =>
        {
            ("Waiting for your skill approval…".into(), false)
        }
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
            let open = turn.open_delegations();
            let mut bots: Vec<(uuid::Uuid, &str)> = Vec::new();
            for task in &open {
                if !bots.iter().any(|(id, _)| *id == task.target_bot_id) {
                    bots.push((task.target_bot_id, &task.target_name));
                }
            }
            let names: Vec<&str> = bots.into_iter().map(|(_, name)| name).collect();
            let label = crate::sidebar::status::waiting_for(&names, open.len());
            (format!("{label}…"), false)
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

/// Centers in the transcript the line a revealed passage asks to scroll to.
///
/// The request is not passed on: the list would only bring the line to its
/// edge, by laying its rows out a second time in the same frame, which
/// registers their accessibility nodes twice. The list scrolls after the frame
/// instead.
struct CenterReveal {
    body: AnyElement,
    list: Option<ListState>,
}

impl IntoElement for CenterReveal {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for CenterReveal {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.body.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.body.prepaint(window, cx);
        if let Some(line) = window.take_autoscroll()
            && let Some(list) = self.list.clone()
        {
            let distance = line.center().y - window.content_mask().bounds.center().y;
            if distance.abs() > px(1.) {
                window.defer(cx, move |_, _| list.scroll_by(distance));
            }
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.body.paint(window, cx);
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
    fn a_failed_turn_says_whether_an_approved_action_ran() {
        let failed = |used: serde_json::Value| {
            serde_json::from_value(serde_json::json!({
                "id": uuid::Uuid::nil(),
                "request_id": uuid::Uuid::nil(),
                "turn_no": 1,
                "provider_id": "claude",
                "status": "failed",
                "error": { "code": "provider_policy",
                           "message": "Claude reported native agent activity (task_started)" },
                "effect_uncertain": false,
                "questions": [{
                    "id": uuid::Uuid::nil(), "kind": "external", "title": "Call github issue_write",
                    "choices": [], "status": "answered", "answer": "approve", "used": used
                }]
            }))
            .unwrap()
        };
        let reason = "Failed. Claude reported native agent activity (task_started)";
        let at = "2026-11-13T07:14:47Z";
        for (used, label) in [
            (serde_json::json!(null), reason.to_owned()),
            (
                serde_json::json!({ "outcome": "error", "at": at }),
                reason.to_owned(),
            ),
            (
                serde_json::json!({ "outcome": "completed", "at": at }),
                format!("{reason}. An approved action had already run."),
            ),
            (
                serde_json::json!({ "outcome": "started", "at": at }),
                format!("{reason}. An approved action had started; its outcome is unknown."),
            ),
        ] {
            assert_eq!(turn_label(&failed(used)), (label, true));
        }
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
