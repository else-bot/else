use super::conversation::Spot;
use super::pending_send::Delivery;
use super::reply::Author;
use super::row_text::RowText;
use super::{ElseApp, RowKey};
use crate::app::transcript::CardKey;
use crate::avatar::bot_avatar;
use crate::controls::{Control as _, Scale, Tier};
use crate::model::{ItemKind, ReactionKind, TurnStatus};
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

/// A bot message's text stops short of its hover actions (two small buttons
/// and the gap between them), so the actions sit beside the words.
pub(super) const BOT_ACTIONS_GUTTER: Pixels = px(64.);

/// Timestamps and other trailing metadata sit at this share of the foreground
/// in dark themes.
pub(super) const META_OPACITY: f32 = 0.55;

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
        window: &mut Window,
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
        if let RowKey::Gap(above) = row.key {
            return self.render_gap(above, cx);
        }
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
            self.render_task_reply(task, row.footer_stamp, window, cx)
        } else {
            self.render_row_content(&row, window, cx)
        };
        let Some(body) = body else {
            return div().into_any_element();
        };
        let banner = self
            .view
            .as_ref()
            .and_then(|view| view.conversation.as_ref())
            .and_then(|c| super::transcript::opened_mail(c, row.opens_turn))
            .and_then(|turn| super::email_reader::mail_banner(turn, &self.bots, cx));
        let body = match banner {
            Some(banner) => gpui_kit::component::v_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .child(banner)
                .child(body)
                .into_any_element(),
            None => body,
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
                            .and_then(|turn| Some((turn.id, turn.routine.as_ref()?)))
                            .map(|(turn, run)| routine_run(turn, run, true, cx)),
                    )
                    .child(body)
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
        let follows_name = self.follows_name(ix);
        let actionable = matches!(row.key, RowKey::Card(card) if card.needs_owner());
        // A routine's rows sit on the page like any other reply; its banner names the run.
        let frame = gpui_kit::component::v_flex()
            .w_full()
            .children(
                turn.routine
                    .as_ref()
                    .filter(|_| group.first && !follows_task)
                    .map(|run| routine_run(turn.id, run, true, cx)),
            )
            .child(div().w_full().child(body));
        column
            .when(group.first, |column| {
                column.pt(if follows_name {
                    LABEL_GAP - ROW_PAD
                } else {
                    ROW_PAD
                })
            })
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
        let active = turn.status.is_working();
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
                )
                .children(model_remedy(turn, bot, cx))
                .children(self.routine_retry(turn, bot, cx)),
        )
    }

    /// Retry beside the failure of a routine's latest run, as in bot
    /// details, which it opens to show how the retry goes.
    pub(super) fn routine_retry(
        &self,
        turn: &crate::model::Turn,
        bot: uuid::Uuid,
        cx: &Context<Self>,
    ) -> Option<Button> {
        let run = turn.routine.as_ref()?;
        let conversation = self.view.as_ref()?.conversation.as_ref()?;
        let routine = (conversation.routines().iter()).find(|r| r.id == run.routine_id)?;
        let latest = (routine.last_run.as_ref()).is_some_and(|last| last.due_at == run.due_at);
        let id = routine.id;
        (latest && super::settings::routines::retryable(routine)).then(|| {
            Button::new(SharedString::from(format!("routine-retry-{}", turn.id)))
                .control(
                    Tier::Ghost,
                    Scale::Compact,
                    self.connection != super::Connection::Connected,
                    cx,
                )
                .label("Retry")
                .accessibility_label(format!("Retry {} now", routine.name))
                .on_click(cx.listener(move |app, event, window, cx| {
                    app.retry_routine(bot, id, window, cx);
                    open_routines(app, event, window, cx);
                }))
        })
    }

    pub(super) fn render_row_content(
        &mut self,
        row: &super::transcript::TranscriptRow,
        window: &Window,
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
                    .when_some(text.natural_width(window, cx), |card, width| {
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
            RowKey::Gap(_) => return None,
            RowKey::QuietRun(turn) => {
                let run = view.conversation.as_ref()?.turn(turn)?.routine.as_ref()?;
                routine_run(turn, run, false, cx).into_any_element()
            }
            RowKey::QuietRuns(first) => {
                let open = super::transcript::group_open(&row.quiet_runs, &view.open_quiet);
                quiet_runs(
                    view.conversation.as_ref()?,
                    first,
                    &row.quiet_runs,
                    open,
                    cx,
                )?
                .into_any_element()
            }
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
                            .child(super::action_card::event_end(
                                &theme,
                                super::action_card::EventEnd::Actions(vec![routine_details(
                                    SharedString::from(format!("routine-update-{id}")),
                                    cx,
                                )]),
                                super::action_card::Disclosure::None,
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
                let user = item.kind == ItemKind::User;
                let turn = conversation.turn(item.turn_id);
                if item.reaction.is_some() {
                    return None;
                }
                // Children stretch so bubbles get a definite width; notes
                // are aligned to the bubble's side.
                let mut column = gpui_kit::component::v_flex().w_full().min_w_0().gap_1();
                let card = card(text, user).id(SharedString::from(format!("message-{id}")));
                if !user
                    && row.starts_run
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
                if !row.folded.is_empty() {
                    column = column.children(self.render_fold(id, &row.folded, cx));
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
                let card = match Author::of(item.kind) {
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
                let card = match Author::of(item.kind) {
                    Some(Author::Bot) => div()
                        .relative()
                        .group("message-bubble")
                        .w_full()
                        .child(side(card).pr(BOT_ACTIONS_GUTTER))
                        .child(
                            div()
                                .absolute()
                                .top_1()
                                .right_0()
                                .invisible()
                                .group_hover("message-bubble", |this| this.visible())
                                .child(self.message_actions(view.bot_id, id, cx)),
                        )
                        .into_any_element(),
                    Some(Author::Owner) => div()
                        .flex()
                        .justify_end()
                        .max_w(relative(0.75))
                        .child(card)
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
            RowKey::Card(card) => self.render_card(card, CardPlacement::Row, window, cx)?,
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
                let bot = view.bot_id;
                let failed = |status: String,
                              color: Hsla,
                              reason: Option<&str>,
                              retry: bool,
                              switch: bool| {
                    h_flex()
                        .flex_wrap()
                        .justify_end()
                        .items_center()
                        .gap_x_2()
                        .text_xs()
                        .child(div().flex_none().size(px(6.)).rounded_full().bg(color))
                        .child(
                            div()
                                .id(SharedString::from(format!("failure-{request_id}")))
                                .text_color(color)
                                .child(format!("{subject}{status}"))
                                .when_some(reason, |label, reason| {
                                    let reason = SharedString::from(reason.to_owned());
                                    label.tooltip(move |window, cx| {
                                        Tooltip::new(reason.clone()).build(window, cx)
                                    })
                                }),
                        )
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
                                .children(switch.then(|| {
                                    switch_model(format!("switch-model-{request_id}"), bot, cx)
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
                    Delivery::Unknown { reason } => failed(
                        "Not confirmed".into(),
                        theme.warning,
                        Some(reason),
                        true,
                        false,
                    ),
                    Delivery::Rejected { reason, code }
                        if code.as_deref() == Some(PROVIDER_UNAVAILABLE) =>
                    {
                        failed(
                            format!("Not sent. {reason}"),
                            theme.danger,
                            None,
                            false,
                            true,
                        )
                    }
                    Delivery::Rejected { reason, .. } => {
                        failed("Not sent".into(), theme.danger, Some(reason), false, false)
                    }
                };
                column = column.child(beside(true, line));
                column.into_any_element()
            }
        })
    }
}

/// Where an owner action card renders: as its own transcript row, or inside
/// its task's card after the reply it followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CardPlacement {
    Row,
    InTask,
}

impl ElseApp {
    pub(super) fn render_card(
        &self,
        key: CardKey,
        placement: CardPlacement,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let card = match key {
            CardKey::SkillInstall(id) => self.render_skill_card(id, cx),
            CardKey::SecretRequest(id) => self.render_secret_card(id, cx),
            CardKey::Question(id) => self.render_question_card(id, placement, window, cx),
            CardKey::Draft(id) => self.render_draft_card(id, placement, cx),
            CardKey::SocialSetup(id) => self.render_social_card(id, cx),
            CardKey::Handoff(id) => self.render_handoff(id, cx),
            CardKey::ComputerHandoff(id) => self.render_handoff_row(id, cx),
            CardKey::OwnerNotice(id) => self.render_notice_card(id, cx)?,
        };
        Some(if key.is_boxed() {
            div().w_full().min_w_0().child(card).into_any_element()
        } else {
            card
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

/// The row naming a routine run, with when it was due in the row's slot.
/// `shown` says whether a no-news run's replies show below it.
fn routine_run(
    turn: uuid::Uuid,
    run: &crate::model::RoutineRun,
    shown: bool,
    cx: &mut Context<ElseApp>,
) -> Button {
    let theme = cx.theme();
    let (meta, disclosure, action) = if run.no_news {
        (
            format!("No news · {}", stamp(run.due_at)),
            if shown {
                super::action_card::Disclosure::Expanded
            } else {
                super::action_card::Disclosure::Collapsed
            },
            if shown {
                "Hide the run's replies"
            } else {
                "Show the run's replies"
            },
        )
    } else {
        (
            stamp(run.due_at),
            super::action_card::Disclosure::None,
            "View routines in bot details",
        )
    };
    let button = super::action_card::event_button(SharedString::from(format!(
        "routine-{}-{}",
        run.routine_id,
        run.due_at.timestamp()
    )))
    .accessibility_label(format!(
        "{} {}, {}{}. {action}",
        run.label(),
        run.name,
        stamp(run.due_at),
        if run.no_news { ", no news" } else { "" }
    ))
    .child(
        super::action_card::event_row(theme)
            .child(super::action_card::event_icon(IconName::Clock))
            .child(super::action_card::event_verb(theme, run.label()))
            .child(super::action_card::event_object(theme, run.name.clone()))
            .child(super::action_card::event_end(
                theme,
                super::action_card::EventEnd::Meta(meta.into()),
                disclosure,
            )),
    );
    if run.no_news {
        let key = RowKey::QuietRun(turn);
        button.on_click(cx.listener(move |app, _, window, cx| app.toggle_quiet(key, window, cx)))
    } else {
        button.on_click(cx.listener(open_routines))
    }
}

/// One row for consecutive no-news runs of a routine: how many, and when
/// the last was due. It shows or folds the list of runs.
fn quiet_runs(
    conversation: &crate::model::Conversation,
    first: uuid::Uuid,
    runs: &[uuid::Uuid],
    open: bool,
    cx: &mut Context<ElseApp>,
) -> Option<Button> {
    let theme = cx.theme();
    let run = conversation.turn(*runs.last()?)?.routine.as_ref()?;
    let count = runs.len();
    let key = RowKey::QuietRuns(first);
    Some(
        super::action_card::event_button(SharedString::from(format!("quiet-runs-{first}")))
            .accessibility_label(format!(
                "{} {}, no news {count} times, last {}. {} the runs",
                run.label(),
                run.name,
                stamp(run.due_at),
                if open { "Hide" } else { "Show" }
            ))
            .child(
                super::action_card::event_row(theme)
                    .child(super::action_card::event_icon(IconName::Clock))
                    .child(super::action_card::event_verb(theme, run.label()))
                    .child(super::action_card::event_object(theme, run.name.clone()))
                    .child(super::action_card::event_end(
                        theme,
                        super::action_card::EventEnd::Meta(
                            format!("No news × {count} · last {}", stamp(run.due_at)).into(),
                        ),
                        if open {
                            super::action_card::Disclosure::Expanded
                        } else {
                            super::action_card::Disclosure::Collapsed
                        },
                    )),
            )
            .on_click(cx.listener(move |app, _, window, cx| app.toggle_quiet(key, window, cx))),
    )
}

fn open_routines(
    app: &mut ElseApp,
    _: &ClickEvent,
    window: &mut Window,
    cx: &mut Context<ElseApp>,
) {
    if !app.details.open() {
        app.toggle_settings(window, cx);
    }
}

fn routine_details(id: SharedString, cx: &mut Context<ElseApp>) -> Button {
    super::action_card::event_action(id, "View", false, cx)
        .accessibility_label("View routines in bot details")
        .on_click(cx.listener(open_routines))
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

/// Rows 6 px from their neighbours, action cards 8 px (16 px between two
/// stacked cards); activity rows of one family 4 px apart (a 40 px pitch),
/// and 16 px from the next family. Each row's spacing depends only on the
/// row itself and the row above it, so appending a row never changes the
/// measured height of the rows before it.
const ROW_PAD: Pixels = px(6.);
const CARD_PAD: Pixels = px(8.);
const EVENT_PAD: Pixels = px(2.);
const EVENT_GROUP_GAP: Pixels = px(16.);
const LABEL_GAP: Pixels = px(8.);

impl ElseApp {
    fn event_kind(&self, ix: usize) -> Option<EventKind> {
        let view = self.view.as_ref()?;
        let row = view.rows.get(ix)?;
        if row.routine.is_some() {
            return None;
        }
        if let Some(activity) = &row.activity {
            // An exchange showing its reply spaces like a message.
            return self
                .exchange_is_line(activity)
                .then_some(EventKind::Exchange);
        }
        if matches!(row.key, RowKey::Card(CardKey::Handoff(_))) {
            return Some(EventKind::Exchange);
        }
        if matches!(row.key, RowKey::QuietRun(_) | RowKey::QuietRuns(_)) {
            return Some(EventKind::Change);
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
        let pad = if self.is_card(ix) { CARD_PAD } else { ROW_PAD };
        if self.follows_name(ix) {
            return (LABEL_GAP - ROW_PAD, pad);
        }
        match (self.event_kind(ix), above) {
            (Some(kind), Some(previous)) if kind == previous => (EVENT_PAD, EVENT_PAD),
            (Some(_), Some(_)) => (EVENT_GROUP_GAP - EVENT_PAD, EVENT_PAD),
            (Some(_), None) => (ROW_PAD, EVENT_PAD),
            (None, Some(_)) => (pad * 2. - EVENT_PAD, pad),
            (None, None) => (pad, pad),
        }
    }

    /// Whether row `ix` sits right under a task row that shows only its
    /// bot's name; the row then reads as that name's content, 8 px below it.
    fn follows_name(&self, ix: usize) -> bool {
        let Some(view) = self.view.as_ref() else {
            return false;
        };
        let Some(above) = ix.checked_sub(1).and_then(|above| view.rows.get(above)) else {
            return false;
        };
        let (Some(task), Some(conversation)) = (above.task.as_ref(), view.conversation.as_ref())
        else {
            return false;
        };
        conversation
            .turn(task.turn)
            .is_some_and(|turn| super::task_reply::shows_only_name(task, turn))
    }

    fn is_card(&self, ix: usize) -> bool {
        self.view
            .as_ref()
            .and_then(|view| view.rows.get(ix))
            .is_some_and(|row| matches!(row.key, RowKey::Card(card) if card.is_boxed()))
    }
}

/// The API's code for a bot whose provider cannot run now, such as a CLI
/// that is signed out on the server.
const PROVIDER_UNAVAILABLE: &str = "provider_unavailable";

/// A Switch model action for a turn that failed because its provider
/// cannot run now.
pub(super) fn model_remedy(
    turn: &crate::model::Turn,
    bot: uuid::Uuid,
    cx: &Context<ElseApp>,
) -> Option<Button> {
    (turn.status == TurnStatus::Failed
        && turn
            .error
            .as_ref()
            .is_some_and(|e| e.code == PROVIDER_UNAVAILABLE))
    .then(|| switch_model(format!("switch-model-{}", turn.id), bot, cx))
}

fn switch_model(id: String, bot: uuid::Uuid, cx: &Context<ElseApp>) -> Button {
    Button::new(SharedString::from(id))
        .control(Tier::Ghost, Scale::Compact, false, cx)
        .label("Switch model")
        .on_click(cx.listener(move |app, _, window, cx| app.switch_model(bot, window, cx)))
}

pub(super) fn turn_label(turn: &crate::model::Turn) -> (String, bool) {
    use crate::model::QuestionUse;
    let (label, error) = status_label(turn);
    if !turn.status.is_unsuccessful() {
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

fn waiting_label(turn: &crate::model::Turn) -> String {
    let open = turn.open_delegations();
    let mut bots: Vec<(uuid::Uuid, &str)> = Vec::new();
    for task in &open {
        if !bots.iter().any(|(id, _)| *id == task.target_bot_id) {
            bots.push((task.target_bot_id, &task.target_name));
        }
    }
    let names: Vec<&str> = bots.into_iter().map(|(_, name)| name).collect();
    let jobs = turn.mcp_waits.len() + turn.computer_job_waits.len();
    crate::sidebar::status::waiting_for(&names, open.len(), jobs)
}

fn status_label(turn: &crate::model::Turn) -> (String, bool) {
    let detail = turn
        .error
        .as_ref()
        .and_then(|e| e.message.clone())
        .unwrap_or_default();
    let continued = turn.limit_continued.map(|c| {
        format!(
            "Continued after the {}-minute limit ({}/{})",
            c.minutes, c.count, c.max
        )
    });
    match turn.status {
        TurnStatus::Queued if turn.taken_by_reply() => (String::new(), false),
        TurnStatus::Queued | TurnStatus::Preparing | TurnStatus::Sending | TurnStatus::Running
            if continued.is_some() =>
        {
            (continued.unwrap_or_default(), false)
        }
        TurnStatus::Queued => ("Queued".into(), false),
        TurnStatus::Preparing | TurnStatus::Sending | TurnStatus::Running => {
            ("Working".into(), false)
        }
        TurnStatus::Waiting if turn.waits_on_job() => (waiting_label(turn), false),
        TurnStatus::Cancelling => ("Stopping…".into(), false),
        TurnStatus::Waiting
            if turn
                .skill_installs
                .iter()
                .any(|r| r.status == crate::model::SkillInstallStatus::Pending) =>
        {
            ("Waiting for your skill approval…".into(), false)
        }
        TurnStatus::Waiting if turn.secret_requests.iter().any(|r| r.is_pending()) => (
            "Waiting for you to answer the secret request…".into(),
            false,
        ),
        TurnStatus::Waiting
            if turn
                .outgoing_drafts
                .iter()
                .any(|d| d.status == crate::model::DraftStatus::Pending) =>
        {
            ("Waiting for your approval…".into(), false)
        }
        TurnStatus::Waiting
            if turn
                .questions
                .iter()
                .any(|q| q.is_pending() && q.is_approval()) =>
        {
            ("Waiting for your approval…".into(), false)
        }
        TurnStatus::Waiting if turn.questions.iter().any(|q| q.is_pending()) => {
            ("Waiting for your answer…".into(), false)
        }
        TurnStatus::Waiting if turn.social_setups.iter().any(|setup| setup.is_pending()) => {
            ("Waiting for you to connect an account…".into(), false)
        }
        TurnStatus::Waiting => (format!("{}…", waiting_label(turn)), false),
        TurnStatus::Interrupted => (
            if turn.error.as_ref().is_some_and(|e| e.code == "cancelled") {
                "Stopped before it ran".into()
            } else {
                "Stopped".into()
            },
            false,
        ),
        TurnStatus::Failed => (format!("Failed. {detail}"), true),
        TurnStatus::Uncertain => (format!("Outcome unknown. {detail}"), true),
        TurnStatus::Completed | TurnStatus::Unknown => (String::new(), false),
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

/// The time alone today, the date as well on another day.
pub(super) fn short_stamp(at: DateTime<Utc>) -> String {
    short_stamp_in(at, Local::now())
}

fn short_stamp_in<Tz: TimeZone>(at: DateTime<Utc>, now: DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    if at.with_timezone(&now.timezone()).date_naive() == now.date_naive() {
        at.with_timezone(&now.timezone())
            .format("%H:%M")
            .to_string()
    } else {
        stamp_in(at, now)
    }
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
    use super::{day_label_in, short_stamp_in, stamp_in, turn_label};
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
    fn a_joined_message_is_queued_until_the_reply_takes_it() {
        let queued = |steer_status: serde_json::Value| {
            serde_json::from_value(serde_json::json!({
                "id": uuid::Uuid::nil(),
                "request_id": uuid::Uuid::nil(),
                "turn_no": 2,
                "provider_id": "claude",
                "status": "queued",
                "error": null,
                "effect_uncertain": false,
                "steered_into": uuid::Uuid::nil(),
                "steer_status": steer_status,
            }))
            .unwrap()
        };
        for status in ["pending", "sending"] {
            assert_eq!(
                turn_label(&queued(serde_json::json!(status))),
                ("Queued".into(), false)
            );
        }
        assert_eq!(
            turn_label(&queued(serde_json::json!("delivered"))),
            (String::new(), false)
        );
    }

    #[test]
    fn a_turn_continued_after_the_attempt_limit_says_so_until_it_ends() {
        let turn = |status: &str| {
            serde_json::from_value(serde_json::json!({
                "id": uuid::Uuid::nil(),
                "request_id": uuid::Uuid::nil(),
                "turn_no": 1,
                "provider_id": "codex",
                "status": status,
                "error": null,
                "effect_uncertain": false,
                "limit_continued": { "count": 2, "max": 4, "minutes": 30 },
            }))
            .unwrap()
        };
        for status in ["queued", "running"] {
            assert_eq!(
                turn_label(&turn(status)),
                ("Continued after the 30-minute limit (2/4)".into(), false)
            );
        }
        assert_eq!(turn_label(&turn("completed")), (String::new(), false));
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
        assert_eq!(short_stamp_in(at("2026-11-04T07:05:00Z"), now), "09:05");
        assert_eq!(
            short_stamp_in(at("2026-11-03T22:30:00Z"), now),
            "00:30",
            "the viewer's day, not UTC's"
        );
        assert_eq!(
            short_stamp_in(at("2026-11-03T21:30:00Z"), now),
            "3 Nov, 23:30"
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
