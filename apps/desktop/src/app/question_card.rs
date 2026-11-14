//! A bot's question to the owner, shown as a card in the conversation: the
//! question or the action to approve, optional context such as a draft or
//! the exact command, and one button per choice. The server records the
//! answer; the card follows the turn's events, so an answer given elsewhere,
//! a newer owner message (skipped), or the turn ending (withdrawn) disables
//! it here too. A click on a card the server already settled is refused.

use crate::controls::{Control as _, Scale, Tier};
use gpui_kit::assets::IconName;
use gpui_kit::base::ElementExt as _;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, Theme, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::message_row::CardPlacement;
use super::{Connection, ElseApp, RowKey};
use crate::api::ClientError;
use crate::model::{
    APPROVE, APPROVE_FOREVER, APPROVE_ROUTINE, APPROVE_TASK, APPROVE_TOOL, DENY, Question,
    QuestionAnswer, QuestionUse,
};

mod approval;
mod choice;
pub(super) mod connection;

pub(super) use choice::WriteIn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct QuestionForm {
    pub(super) answer: QuestionAnswer,
    pub(super) state: SubmitState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SubmitState {
    Choosing,
    Sending,
    Recorded,
    Failed(String),
}

impl QuestionForm {
    fn locks(&self) -> bool {
        matches!(self.state, SubmitState::Sending | SubmitState::Recorded)
    }
}

fn outcome(question: &Question, bot_name: &str) -> String {
    use super::message_row::stamp;
    let decision = decision(question, bot_name);
    match question.used {
        Some(QuestionUse::Completed { at }) => format!("{decision} Ran at {}.", stamp(at)),
        Some(QuestionUse::Error { at }) => {
            format!("{decision} Tried at {}; it returned an error.", stamp(at))
        }
        Some(QuestionUse::Started { at }) => {
            format!(
                "{decision} Started at {}; no result recorded yet.",
                stamp(at)
            )
        }
        None => decision,
    }
}

fn decision(question: &Question, bot_name: &str) -> String {
    match question.status.as_str() {
        "answered" => match (question.is_approval(), question.answer.as_ref().and_then(QuestionAnswer::single)) {
            (true, Some(APPROVE)) => "Approved once.".into(),
            (true, Some(APPROVE_FOREVER)) => {
                format!("Approved, and destructive actions are always allowed for {bot_name}.")
            }
            (true, Some(APPROVE_ROUTINE)) => {
                "Approved. Destructive actions are allowed for this routine's saved task on future runs."
                    .into()
            }
            (true, Some(APPROVE_TOOL)) => {
                question.mcp_tool_scope().map_or_else(
                    || "Approved. This tool and action type are allowed in future chat requests.".into(),
                    |(integration, tool)| format!("Always allowed for this bot: {integration} · {tool} for this action type."),
                )
            }
            (true, Some(APPROVE_TASK)) => question.task_scope().map_or_else(
                || "Approved. This task and action type are allowed in future chats and scheduled routine runs of this bot.".into(),
                |scope| format!("Always allowed for this bot: {} on {}.", scope.task, scope.target),
            ),
            (true, Some(DENY)) => "Denied.".into(),
            _ => format!(
                "You chose {}.",
                question
                    .chosen()
                    .map(|c| c.label.clone())
                    .unwrap_or_default()
            ),
        },
        "skipped" => "Skipped: you sent a new message instead. Nothing was approved.".into(),
        "cancelled" => "Withdrawn: the request ended before you answered.".into(),
        _ => String::new(),
    }
}

fn letter(choice_id: &str) -> String {
    choice_id.to_uppercase()
}

impl ElseApp {
    fn toggle_question_details(&mut self, id: Uuid, cx: &mut Context<Self>) {
        if let Some(view) = self.view.as_mut()
            && !view.expanded_questions.remove(&id)
        {
            view.expanded_questions.insert(id);
        }
        self.remeasure_row(RowKey::Question(id), cx);
    }

    pub(super) fn sync_question_submits(&mut self) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let conversation = view.conversation.as_ref();
        view.question_submits.retain(|id, _| {
            conversation
                .and_then(|c| c.question(*id))
                .is_some_and(Question::is_pending)
        });
        view.approval_scopes.retain(|id, _| {
            conversation
                .and_then(|c| c.question(*id))
                .is_some_and(Question::is_pending)
        });
    }

    fn answer_question(
        &mut self,
        question: Uuid,
        answer: QuestionAnswer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let client = self.client.clone();
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let bot = view.bot_id;
        let pending = view
            .conversation
            .as_ref()
            .and_then(|c| c.question(question))
            .is_some_and(Question::is_pending);
        if !pending
            || view
                .question_submits
                .get(&question)
                .is_some_and(QuestionForm::locks)
        {
            return;
        }
        view.expanded_questions.remove(&question);
        view.question_submits.insert(
            question,
            QuestionForm {
                answer: answer.clone(),
                state: SubmitState::Sending,
            },
        );
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = client.answer_question(bot, question, answer).await;
            let _ = this.update_in(cx, |app, _window, cx| {
                app.question_answered(bot, question, result, cx)
            });
        });
        task.detach();
        self.remeasure_row(RowKey::Question(question), cx);
    }

    fn question_answered(
        &mut self,
        bot: Uuid,
        question: Uuid,
        result: Result<(), ClientError>,
        cx: &mut Context<Self>,
    ) {
        if matches!(&result, Err(ClientError::Unauthorized)) {
            self.connection = Connection::InvalidCredentials(ClientError::Unauthorized.to_string());
        }
        let Some(view) = self.view.as_mut().filter(|v| v.bot_id == bot) else {
            return;
        };
        let Some(submit) = view.question_submits.get_mut(&question) else {
            return;
        };
        submit.state = match result {
            Ok(()) => SubmitState::Recorded,
            Err(ClientError::Api { code, message, .. }) if code == "routine_changed" => {
                SubmitState::Failed(message)
            }
            Err(ClientError::Api { status: 409, .. }) => SubmitState::Failed(
                "This card was already answered, skipped, or withdrawn. Nothing changed.".into(),
            ),
            Err(err) => SubmitState::Failed(err.to_string()),
        };
        self.remeasure_row(RowKey::Question(question), cx);
    }

    pub(super) fn render_question_card(
        &self,
        id: Uuid,
        placement: CardPlacement,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let Some(question) = view.conversation.as_ref().and_then(|c| c.question(id)) else {
            return div().into_any_element();
        };
        let submit = view.question_submits.get(&id);
        let bot_name = self
            .bots
            .iter()
            .find(|b| b.id == view.bot_id)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        let waiting = question.is_pending() && !submit.is_some_and(QuestionForm::locks);
        let card = if !question.is_approval() {
            self.render_choice_card(question, submit, &bot_name, placement, window, cx)
        } else if question.mcp_connection().is_some() || question.email_connection().is_some() {
            self.render_connection_card(question, &bot_name, cx)
        } else {
            self.render_approval_card(question, submit, &bot_name, cx)
        };
        if waiting {
            self.sighted(RowKey::Question(id), card, cx)
        } else {
            card
        }
    }
}

impl ElseApp {
    /// The conversation rows still waiting on the owner's answer, in
    /// timeline order: questions, approvals, and credential requests.
    fn waiting_question_rows(&self) -> Vec<usize> {
        let Some(view) = self.view.as_ref() else {
            return Vec::new();
        };
        let Some(conversation) = view.conversation.as_ref() else {
            return Vec::new();
        };
        view.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| match row.key {
                RowKey::Question(id) => {
                    conversation.question(id).is_some_and(Question::is_pending)
                        && !view
                            .question_submits
                            .get(&id)
                            .is_some_and(QuestionForm::locks)
                }
                RowKey::SecretRequest(id) => {
                    conversation
                        .secret_request(id)
                        .is_some_and(|request| request.is_pending())
                        && !view.answered.contains(&id)
                        && !view.secret_forms.get(&id).is_some_and(|form| form.locks())
                }
                _ => false,
            })
            .map(|(ix, _)| ix)
            .collect()
    }

    fn jump_to_question(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.waiting_question_rows().first().copied() else {
            return;
        };
        let Some(view) = self.view.as_mut() else {
            return;
        };
        view.list.scroll_to(ListOffset {
            item_ix: ix,
            offset_in_item: px(0.),
        });
        let reveal =
            super::conversation::Reveal::new(view.rows[ix].key, super::conversation::Spot::Row);
        self.start_reveal(reveal, cx);
    }

    /// The strip above the composer while the bot waits on the owner. It
    /// shows only while no waiting card is on screen, so the card itself is
    /// the one signal when the owner can see it.
    pub(super) fn render_question_bar(
        &self,
        bot_name: &str,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let rows = self.waiting_question_rows();
        let view = self.view.as_ref()?;
        let mut sightings = view.sightings.0.borrow_mut();
        let painted = std::mem::take(&mut sightings.painted);
        let viewport = view.list.viewport_bounds();
        sightings.hidden = rows.iter().any(|ix| {
            painted
                .get(&view.rows[*ix].key)
                .is_some_and(|bounds| shows(*bounds, viewport))
        });
        drop(sightings);
        if sightings_hide(view) {
            return None;
        }
        let waiting = rows.len();
        let first = *rows.first()?;
        let secret = self
            .view
            .as_ref()
            .is_some_and(|view| matches!(view.rows[first].key, RowKey::SecretRequest(_)));
        let theme = cx.theme();
        Some(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_3()
                .items_center()
                .px_3()
                .py_1p5()
                .rounded(theme.radius)
                .bg(theme.warning.opacity(0.12))
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.warning)
                        .child(Icon::new(IconName::Pause).small()),
                )
                .child(
                    h_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_1p5()
                        .text_sm()
                        .child(
                            div()
                                .flex_none()
                                .font_semibold()
                                .child(format!("{bot_name} is paused")),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme.muted_foreground)
                                .child(if waiting == 1 {
                                    "· waiting on your answer".to_owned()
                                } else {
                                    format!("· waiting on {waiting} answers")
                                }),
                        ),
                )
                .child(
                    Button::new("jump-to-question")
                        .control(Tier::Ghost, Scale::Compact, false, cx)
                        .icon(IconName::ArrowDown)
                        .label(if secret {
                            "Jump to request"
                        } else {
                            "Jump to question"
                        })
                        .on_click(cx.listener(|app, _, _, cx| app.jump_to_question(cx))),
                )
                .into_any_element(),
        )
    }
}

/// How much of a waiting card must show before the paused strip steps aside.
const VISIBLE_ENOUGH: f32 = 48.;

/// Where each card that can pause the bot was painted in the latest frame,
/// and whether the paused strip stepped aside for one of them. The list
/// paints only rows near the viewport, so a card missing here is off screen.
#[derive(Clone, Default)]
pub(super) struct Sightings(std::rc::Rc<std::cell::RefCell<Painted>>);

#[derive(Default)]
struct Painted {
    painted: std::collections::HashMap<RowKey, Bounds<Pixels>>,
    hidden: bool,
}

fn sightings_hide(view: &super::conversation::ConversationView) -> bool {
    view.sightings.0.borrow().hidden
}

/// Whether enough of a card shows inside the conversation viewport to act on.
fn shows(card: Bounds<Pixels>, viewport: Bounds<Pixels>) -> bool {
    let shown = card.bottom().min(viewport.bottom()) - card.top().max(viewport.top());
    shown >= px(VISIBLE_ENOUGH).min(card.size.height)
}

impl ElseApp {
    /// Records where a pausing card was painted. When that flips whether
    /// the paused strip should show, the app renders again to follow it.
    pub(super) fn sighted(
        &self,
        key: RowKey,
        card: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return card;
        };
        let sightings = view.sightings.clone();
        let list = view.list.clone();
        let owner = cx.entity().downgrade();
        div()
            .w_full()
            .child(card)
            .on_prepaint(move |bounds, window, cx| {
                sightings.0.borrow_mut().painted.insert(key, bounds);
                window.defer(cx, move |_, cx| {
                    let viewport = list.viewport_bounds();
                    let state = sightings.0.borrow();
                    let any = state
                        .painted
                        .values()
                        .any(|bounds| shows(*bounds, viewport));
                    // Showing a card hides the strip; losing the last one
                    // brings it back. Settled cards never set `hidden`.
                    if any != state.hidden {
                        drop(state);
                        let _ = owner.update(cx, |_, cx| cx.notify());
                    }
                });
            })
            .into_any_element()
    }
}

/// The rounded, tinted square that leads every card header.
fn tile(icon: IconName, color: Hsla) -> Div {
    h_flex()
        .size_9()
        .flex_none()
        .justify_center()
        .rounded(px(9.))
        .bg(color.opacity(0.16))
        .text_color(color)
        .child(Icon::new(icon).size_4())
}

/// A status pill in the task card's style, with working dots while busy.
fn pill(id: Uuid, label: &'static str, color: Hsla, working: bool, still: bool) -> Div {
    h_flex()
        .flex_none()
        .items_center()
        .gap_1()
        .px_2()
        .rounded_full()
        .border_1()
        .border_color(color.opacity(0.3))
        .bg(color.opacity(0.08))
        .text_xs()
        .text_color(color)
        .child(label)
        .when(working, |pill| {
            pill.child(super::view::working_dots(id, color, still))
        })
}

/// The one-line summary a settled card folds into; clicking it shows the
/// card's context.
fn receipt(
    id: Uuid,
    done: bool,
    title: String,
    outcome: String,
    expanded: bool,
    theme: &Theme,
) -> Button {
    Button::new(SharedString::from(format!("question-receipt-{id}")))
        .focus_ring(false)
        .ghost()
        .small()
        .w_full()
        .h_auto()
        .px_4()
        .py_3()
        .rounded(theme.radius_lg)
        .cursor_pointer()
        .accessibility_label(format!("{title} {outcome}"))
        .child(
            h_flex()
                .w_full()
                .gap_3()
                .child(
                    Icon::new(if done {
                        IconName::Check
                    } else {
                        IconName::CircleOff
                    })
                    .size_4()
                    .text_color(if done {
                        theme.success
                    } else {
                        theme.muted_foreground
                    }),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_0p5()
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .text_left()
                                .whitespace_normal()
                                .when(!done, |title| title.text_color(theme.muted_foreground))
                                .child(title),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_left()
                                .whitespace_normal()
                                .text_color(theme.muted_foreground)
                                .child(outcome),
                        ),
                )
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size_3()
                    .text_color(theme.muted_foreground),
                ),
        )
}

/// A red callout for an error the owner can act on.
fn callout(text: String, theme: &Theme) -> Div {
    super::action_card::failure(theme, text)
}

#[cfg(test)]
mod tests {
    use super::{letter, outcome, shows};
    use gpui_kit::{Bounds, point, px, size};

    #[test]
    fn the_paused_strip_steps_aside_only_while_enough_of_a_card_shows() {
        let viewport = Bounds::new(point(px(0.), px(100.)), size(px(700.), px(400.)));
        let card =
            |top: f32, height: f32| Bounds::new(point(px(0.), px(top)), size(px(700.), px(height)));
        assert!(shows(card(200., 200.), viewport));
        assert!(
            shows(card(450., 200.), viewport),
            "50 px peek above the composer"
        );
        assert!(
            !shows(card(470., 200.), viewport),
            "30 px peek is not enough"
        );
        assert!(!shows(card(-150., 200.), viewport), "scrolled away above");
        assert!(!shows(card(520., 200.), viewport), "below the viewport");
        assert!(
            shows(card(300., 30.), viewport),
            "a short card counts when whole"
        );
    }
    use crate::model::{APPROVE, DENY, Question, QuestionAnswer, QuestionChoice, QuestionUse};
    use uuid::Uuid;

    fn question(kind: &str, status: &str, answer: Option<&str>) -> Question {
        let choices = if kind == "choice" {
            vec![
                QuestionChoice {
                    id: "a".into(),
                    label: "Plain text".into(),
                },
                QuestionChoice {
                    id: "b".into(),
                    label: "HTML digest".into(),
                },
            ]
        } else {
            vec![
                QuestionChoice {
                    id: APPROVE.into(),
                    label: "Approve once".into(),
                },
                QuestionChoice {
                    id: DENY.into(),
                    label: "Deny".into(),
                },
            ]
        };
        Question {
            id: Uuid::nil(),
            kind: kind.into(),
            title: "t".into(),
            action: None,
            context: None,
            choices,
            multiple: false,
            status: status.into(),
            answer: answer.map(|id| QuestionAnswer::Single(id.to_owned())),
            settled_at: None,
            used: None,
        }
    }

    #[test]
    fn settled_cards_say_what_happened_and_grant_nothing_when_skipped() {
        assert_eq!(
            outcome(&question("choice", "answered", Some("b")), "Mail"),
            "You chose HTML digest."
        );
        assert_eq!(
            outcome(&question("external", "answered", Some(DENY)), "Mail"),
            "Denied."
        );
        assert!(
            outcome(&question("destructive", "skipped", None), "Mail")
                .contains("Nothing was approved")
        );
        assert!(outcome(&question("choice", "cancelled", None), "Mail").starts_with("Withdrawn"));
        assert_eq!(outcome(&question("choice", "pending", None), "Mail"), "");
        assert_eq!(letter("c"), "C");
    }

    #[test]
    fn an_approval_says_whether_its_call_ran() {
        let mut card = question("external", "answered", Some(APPROVE));
        assert_eq!(outcome(&card, "Mail"), "Approved once.");
        let at = "2026-11-13T07:14:47Z".parse().unwrap();
        let stamp = super::super::message_row::stamp(at);
        for (used, said) in [
            (
                QuestionUse::Completed { at },
                format!("Approved once. Ran at {stamp}."),
            ),
            (
                QuestionUse::Error { at },
                format!("Approved once. Tried at {stamp}; it returned an error."),
            ),
            (
                QuestionUse::Started { at },
                format!("Approved once. Started at {stamp}; no result recorded yet."),
            ),
        ] {
            card.used = Some(used);
            assert_eq!(outcome(&card, "Mail"), said);
        }
    }

    #[test]
    fn task_approval_names_the_saved_category_and_target() {
        let mut card = question("destructive", "answered", Some(crate::model::APPROVE_TASK));
        for action in [
            serde_json::json!({ "host": "ssh omarchy-m1 tailscale status", "task_scope": {
                "task": "Manage Tailscale", "target": "SSH host omarchy-m1"
            }}),
            serde_json::json!({ "permission": { "scope": "task", "kind": "destructive",
                "task": "Manage Tailscale", "target": "SSH host omarchy-m1"
            }}),
        ] {
            card.action = Some(serde_json::from_value(action).unwrap());
            assert_eq!(
                outcome(&card, "Tech Manager"),
                "Always allowed for this bot: Manage Tailscale on SSH host omarchy-m1."
            );
        }
    }

    #[test]
    fn tool_approval_keeps_its_scope_visible_without_context() {
        let mut card = question("external", "answered", Some(crate::model::APPROVE_TOOL));
        for action in [
            serde_json::json!({"tool": "mcp_request", "args": {
                "id": "tickets", "method": "tools/call", "tool": "create_ticket", "effect": "external"
            }}),
            serde_json::json!({"permission": {
                "scope": "mcp_tool", "mcp_id": "tickets", "tool": "create_ticket", "kind": "external"
            }}),
        ] {
            card.action = Some(serde_json::from_value(action).unwrap());
            assert_eq!(card.mcp_tool_scope(), Some(("tickets", "create_ticket")));
            assert_eq!(
                outcome(&card, "Tracker"),
                "Always allowed for this bot: tickets · create_ticket for this action type."
            );
        }
    }
}
