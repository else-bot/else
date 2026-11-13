use super::conversation::Spot;
use super::pending_send::Delivery;
use super::reply::Author;
use super::row_text::RowText;
use super::{ElseApp, RowKey};
use crate::avatar::bot_avatar;
use crate::controls::{Control as _, Scale, Tier};
use crate::model::ReactionKind;
use chrono::{DateTime, Datelike as _, Local, TimeZone, Utc};
use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::button::Button;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Theme, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// Bot-side cards use the full shared reading column.
pub(super) const BOT_GUTTER: Pixels = px(0.);

/// Timestamps and other trailing metadata sit at this share of the foreground
/// in dark themes.
pub(super) const META_OPACITY: f32 = 0.45;

/// The colour of timestamps and other trailing metadata. A faded foreground
/// stays quiet on a dark page, but on a light one it falls below 4.5:1, so
/// light themes use their readable secondary text instead.
pub(super) fn meta_color(theme: &Theme) -> Hsla {
    if theme.mode.is_dark() {
        theme.foreground.opacity(META_OPACITY)
    } else {
        theme.muted_foreground
    }
}

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
                                let theme = cx.theme();
                                super::action_card::event_row(theme)
                                    .child(super::action_card::event_icon(IconName::Clock))
                                    .child(super::action_card::event_verb(theme, run.label()))
                                    .child(super::action_card::event_object(
                                        theme,
                                        run.name.clone(),
                                    ))
                                    .child(super::action_card::event_meta(theme, stamp(run.due_at)))
                                    .child(routine_details(
                                        SharedString::from(format!(
                                            "routine-{}-{}",
                                            run.routine_id,
                                            run.due_at.timestamp()
                                        )),
                                        cx,
                                    ))
                            }),
                    )
                    .child(body)
                    .children(row.footer_stamp.map(|at| {
                        div()
                            .text_size(px(12.))
                            .text_color(meta_color(cx.theme()))
                            .child(clock(at))
                    }))
                    .into_any_element()
            } else {
                body
            };
            let (top, bottom) = self.row_rhythm(ix);
            return column
                .pt(top)
                .pb(bottom)
                .children(row.day.map(|at| day_separator(at, cx)))
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
        let actionable = matches!(
            row.key,
            RowKey::Question(_)
                | RowKey::SecretRequest(_)
                | RowKey::Draft(_)
                | RowKey::OwnerNotice(_)
        );
        // A routine's rows sit on the page like any other reply; its banner names the run.
        let frame = gpui_kit::component::v_flex()
            .w_full()
            .children(
                turn.routine
                    .as_ref()
                    .filter(|_| group.first && !follows_task)
                    .map(|run| routine_banner(run, cx)),
            )
            .child(div().w_full().child(body));
        column
            .when(group.first, |column| column.pt_1p5())
            .when(group.last, |column| column.pb_1p5())
            .children(row.day.map(|at| day_separator(at, cx)))
            .children(row.unread_start.then(|| unread_divider(cx)))
            .child(
                div()
                    .w_full()
                    .when(!actionable, |frame| frame.pr(BOT_GUTTER))
                    .child(frame),
            )
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
        let footer = row.footer_stamp.filter(|_| !grouped).map(clock);
        let theme = cx.theme().clone();
        // The owner's bubble hugs its text up to three quarters of the column, with
        // the same 12×16 padding whether or not it carries an attachment. The text
        // widget may draw part of that padding itself.
        let card = |text: &RowText, user: bool| {
            if user {
                let (inset_x, inset_y) = text.inset();
                let (pad_x, pad_y) = (px(16.) - inset_x, px(12.) - inset_y);
                div()
                    .min_w_0()
                    .when_some(text.natural_width(cx), |card, width| {
                        card.w(width + pad_x * 2.)
                    })
                    .px(pad_x)
                    .py(pad_y)
                    .rounded(theme.radius_lg)
                    // The owner's words sit a step above the page: lighter
                    // than it in both themes.
                    .bg(if theme.mode.is_dark() {
                        theme.secondary
                    } else {
                        theme.popover
                    })
                    .child(text.render(cx))
            } else {
                div().w_full().min_w_0().py_1().child(text.render(cx))
            }
        };
        let side = |card: AnyElement| div().w_full().min_w_0().child(card);
        let note = |text: String, error: bool| {
            div()
                .text_size(px(12.))
                .text_color(if error {
                    theme.danger
                } else {
                    meta_color(&theme)
                })
                .child(text)
        };
        Some(match key {
            RowKey::Item(id) => {
                let conversation = view.conversation.as_ref()?;
                let item = conversation.item(id)?;
                if let Some(update) = &item.routine_update {
                    return Some(
                        super::action_card::event_row(&theme)
                            .child(super::action_card::event_icon(IconName::Clock))
                            .child(super::action_card::event_verb(&theme, "Updated routine"))
                            .child(super::action_card::event_object(
                                &theme,
                                update.name.clone(),
                            ))
                            .child(routine_details(
                                SharedString::from(format!("routine-update-{id}")),
                                cx,
                            ))
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
                        .when(!user, |bubble| bubble.w_full())
                        .when(user, |bubble| {
                            bubble.flex().justify_end().max_w(relative(0.75))
                        })
                        .child(if user {
                            card
                        } else {
                            side(card).into_any_element()
                        })
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
                let card = if user {
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .justify_end()
                        .child(card)
                        .into_any_element()
                } else {
                    card
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
                    column = column.child(
                        h_flex().w_full().min_w_0().justify_end().child(
                            card(text, true)
                                .max_w(relative(0.75))
                                .id(SharedString::from(format!("pending-{request_id}"))),
                        ),
                    );
                    if !settled {
                        column = column.children(footer.map(|at| beside(true, note(at, false))));
                    }
                }
                let subject = mark.map_or_else(String::new, |mark| format!("{mark} "));
                let action = |id: &str, label: &'static str| {
                    Button::new(SharedString::from(format!("{id}-{request_id}")))
                        .control(Tier::Ghost, Scale::Compact, false, cx)
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
    super::action_card::event_row(theme)
        .child(super::action_card::event_icon(IconName::Clock))
        .child(super::action_card::event_verb(theme, run.label()))
        .child(super::action_card::event_object(theme, run.name.clone()))
        .child(super::action_card::event_meta(
            theme,
            run.due_at
                .with_timezone(&Local)
                .format("%e %b · %H:%M %Z")
                .to_string(),
        ))
        .child(routine_details(
            SharedString::from(format!(
                "routine-{}-{}",
                run.routine_id,
                run.due_at.timestamp()
            )),
            cx,
        ))
        .into_any_element()
}

fn routine_details(id: SharedString, cx: &mut Context<ElseApp>) -> Button {
    Button::new(id)
        .control(Tier::Ghost, Scale::Compact, false, cx)
        .label("View")
        .accessibility_label("View routines in bot details")
        .on_click(cx.listener(|app, _, window, cx| {
            if !app.settings_open {
                app.toggle_settings(window, cx);
            }
        }))
}

/// The two families of one-line activity rows. A run of one family reads
/// as one block; the next family starts a new block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventKind {
    /// Task exchanges between bots, and the councils and hand-offs that
    /// started them.
    Exchange,
    /// Changes to bots, rules and routines.
    Change,
}

/// Rows 6 px from their neighbours; activity rows of one family 4 px apart
/// (a 40 px pitch), and 16 px from the next family. Each row's spacing
/// depends only on the row above it, so appending a row never changes the
/// measured height of the rows before it.
const ROW_PAD: Pixels = px(6.);
const EVENT_PAD: Pixels = px(2.);
const EVENT_GROUP_GAP: Pixels = px(16.);

impl ElseApp {
    fn event_kind(&self, ix: usize) -> Option<EventKind> {
        let view = self.view.as_ref()?;
        let row = view.rows.get(ix)?;
        if row.routine.is_some() {
            return None;
        }
        if row.activity.is_some() || matches!(row.key, RowKey::Handoff(_)) {
            return Some(EventKind::Exchange);
        }
        let RowKey::Item(id) = row.key else {
            return None;
        };
        let item = view.conversation.as_ref()?.item(id)?;
        (item.routine_update.is_some() || item.bot_created.is_some() || item.rules_update.is_some())
            .then_some(EventKind::Change)
    }

    /// The top and bottom padding of row `ix`.
    fn row_rhythm(&self, ix: usize) -> (Pixels, Pixels) {
        let above = ix.checked_sub(1).and_then(|above| self.event_kind(above));
        match (self.event_kind(ix), above) {
            (Some(kind), Some(previous)) if kind == previous => (EVENT_PAD, EVENT_PAD),
            (Some(_), Some(_)) => (EVENT_GROUP_GAP - EVENT_PAD, EVENT_PAD),
            (Some(_), None) => (ROW_PAD, EVENT_PAD),
            (None, Some(_)) => (ROW_PAD * 2. - EVENT_PAD, ROW_PAD),
            (None, None) => (ROW_PAD, ROW_PAD),
        }
    }
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

/// A transcript stamp: the time only, because a day separator names the day.
pub(super) fn clock(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%H:%M").to_string()
}

fn day_label_in<Tz: TimeZone>(at: DateTime<Utc>, now: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let at = at.with_timezone(&now.timezone());
    let days = (now.date_naive() - at.date_naive()).num_days();
    match days {
        0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        _ if at.year() == now.year() => at.format("%A, %-d %B").to_string(),
        _ => at.format("%-d %B %Y").to_string(),
    }
}

fn day_separator(at: DateTime<Utc>, cx: &App) -> Div {
    h_flex()
        .w_full()
        .justify_center()
        .pt_4()
        .pb_2()
        .text_size(px(12.))
        .text_color(meta_color(cx.theme()))
        .child(day_label_in(at, Local::now()))
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
    use super::{day_label_in, stamp_in, turn_label};
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

    #[test]
    fn day_separators_name_recent_days_and_other_years() {
        let zone = FixedOffset::east_opt(2 * 3600).unwrap();
        let now = "2026-11-04T12:00:00Z"
            .parse::<DateTime<Utc>>()
            .unwrap()
            .with_timezone(&zone);
        let at = |text: &str| text.parse::<DateTime<Utc>>().unwrap();
        assert_eq!(day_label_in(at("2026-11-03T22:30:00Z"), now), "Today");
        assert_eq!(day_label_in(at("2026-11-03T21:30:00Z"), now), "Yesterday");
        assert_eq!(
            day_label_in(at("2026-10-29T10:00:00Z"), now),
            "Thursday, 29 October"
        );
        assert_eq!(
            day_label_in(at("2025-12-31T21:59:00Z"), now),
            "31 December 2025"
        );
    }
}
