//! A bot's question to the owner, shown as a card in the conversation: the
//! question or the action to approve, optional context such as a draft or
//! the exact command, and one button per choice. The server records the
//! answer; the card follows the turn's events, so an answer given elsewhere,
//! a newer owner message (skipped), or the turn ending (withdrawn) disables
//! it here too. A click on a card the server already settled is refused.

use gpui_kit::assets::IconName;
use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _, Theme, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::{Connection, ElseApp, RowKey};
use crate::api::ClientError;
use crate::model::{
    APPROVE, APPROVE_FOREVER, APPROVE_ROUTINE, APPROVE_TASK, APPROVE_TOOL, DENY, Question,
    QuestionAnswer,
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

    pub(super) fn render_question_card(&self, id: Uuid, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = self.view.as_ref() else {
            return div().into_any_element();
        };
        let Some(question) = view.conversation.as_ref().and_then(|c| c.question(id)) else {
            return div().into_any_element();
        };
        let submit = view.question_submits.get(&id);
        if !question.is_approval() {
            return self.render_choice_card(question, submit, cx);
        }
        let bot_name = self
            .bots
            .iter()
            .find(|b| b.id == view.bot_id)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        if question.mcp_connection().is_some() || question.email_connection().is_some() {
            return self.render_connection_card(question, &bot_name, cx);
        }
        self.render_approval_card(question, submit, &bot_name, cx)
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

/// An inset block for a card's exact command, request, or explanation.
fn inset(text: String, theme: &Theme) -> Div {
    div()
        .w_full()
        .px_3()
        .py_2p5()
        .rounded(theme.radius)
        .bg(theme.background)
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .line_height(relative(1.6))
        .whitespace_normal()
        .child(text)
}

/// A red callout for an error the owner can act on.
fn callout(text: String, theme: &Theme) -> Div {
    h_flex()
        .w_full()
        .items_start()
        .gap_2p5()
        .px_3()
        .py_2p5()
        .rounded(theme.radius)
        .bg(theme.danger.opacity(0.1))
        .text_color(theme.danger)
        .text_xs()
        .child(Icon::new(IconName::TriangleAlert).size_4().flex_none())
        .child(div().flex_1().min_w_0().whitespace_normal().child(text))
}

#[cfg(test)]
mod tests {
    use super::{letter, outcome};
    use crate::model::{APPROVE, DENY, Question, QuestionAnswer, QuestionChoice};
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
