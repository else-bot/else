//! A bot's question to the owner, shown as a card in the conversation: the
//! question or the action to approve, optional context such as a draft or
//! the exact command, and one button per choice. The server records the
//! answer; the card follows the turn's events, so an answer given elsewhere,
//! a newer owner message (skipped), or the turn ending (withdrawn) disables
//! it here too. A click on a card the server already settled is refused.

use gpui_kit::component::FocusableExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::*;
use uuid::Uuid;

use super::{Connection, ElseApp, RowKey};
use crate::api::ClientError;
use crate::model::{
    APPROVE, APPROVE_FOREVER, APPROVE_ROUTINE, APPROVE_TASK, APPROVE_TOOL, DENY, Question,
    QuestionAnswer,
};

mod choice;
pub(super) mod mcp;

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

fn heading(question: &Question, bot_name: &str) -> String {
    match question.kind.as_str() {
        "destructive" => format!("{bot_name} wants to do something that is hard to undo"),
        "external" => format!("{bot_name} wants to take a sensitive action"),
        _ => format!("{bot_name} has a question"),
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
                "Approved. This tool and action type are allowed in future chat requests.".into()
            }
            (true, Some(APPROVE_TASK)) => question.task_scope().map_or_else(
                || "Approved. This task and action type are allowed in future chats with this bot.".into(),
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
        if question.mcp_connection().is_some() {
            return self.render_mcp_card(question, cx);
        }
        if !question.is_approval() {
            return self.render_choice_card(question, submit, cx);
        }
        let theme = cx.theme().clone();
        let bot_name = self
            .bots
            .iter()
            .find(|b| b.id == view.bot_id)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        let note = |text: String| {
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(text)
        };
        let pending = question.is_pending();
        let locked = !pending
            || submit.is_some_and(QuestionForm::locks)
            || self.connection != Connection::Connected;
        let mut card = v_flex()
            .w_full()
            .gap_2()
            .p_3()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(note(heading(question, &bot_name)))
            .child(div().font_semibold().child(question.title.clone()));
        if let Some(context) = &question.context {
            card = card.child(
                div()
                    .w_full()
                    .px_2p5()
                    .py_2()
                    .rounded(theme.radius)
                    .bg(theme.background)
                    .text_sm()
                    .whitespace_normal()
                    .child(context.clone()),
            );
        }
        card = card.child(note(match question.kind.as_str() {
            _ if question.choices.iter().any(|c| c.id == APPROVE_TASK) => question.task_scope().map_or_else(
                || "Always allow remembers this task and action type for future chats with this bot.".to_owned(),
                |scope| format!("Always allow remembers \"{}\" on \"{}\" for this action type in future chats with this bot. Other tasks and targets still need approval.", scope.task, scope.target),
            ),
            _ if question.choices.iter().any(|c| c.id == APPROVE_TOOL) => "Approve once covers this action. Always allow remembers this integration tool and action type for this bot's chat requests. Other tools and action types still need approval.".to_owned(),
            "external" => "Approve once covers exactly this sensitive action.".to_owned(),
            _ if question.choices.iter().any(|c| c.id == APPROVE_ROUTINE) => "Always allow for this routine remembers destructive approval for its saved task on future runs. Other routines and chat requests still need approval. Changing the task clears this permission.".to_owned(),
            _ if question.choices.iter().any(|c| c.id == APPROVE_FOREVER) => "Approve once covers exactly this action. Always allow lets this bot take destructive actions without asking.".to_owned(),
            _ => "Approval covers exactly this action, once.".to_owned(),
        }));
        let mut buttons = h_flex().w_full().flex_wrap().justify_end().gap_2();
        for choice in &question.choices {
            let label = match submit
                .filter(|s| s.answer.contains(&choice.id))
                .map(|s| &s.state)
            {
                Some(SubmitState::Sending) => "Sending…".to_owned(),
                Some(SubmitState::Recorded) => "Recorded".to_owned(),
                _ if choice.id == APPROVE_FOREVER => "Always allow for this bot".to_owned(),
                _ => choice.label.clone(),
            };
            let choice_id = choice.id.clone();
            let button = Button::new(SharedString::from(format!("question-{id}-{}", choice.id)))
                .focus_ring(false)
                .cursor_pointer()
                .label(label)
                .small()
                .disabled(locked)
                .on_click(cx.listener(move |app, _, window, cx| {
                    app.answer_question(id, QuestionAnswer::Single(choice_id.clone()), window, cx)
                }));
            let button = if choice.id == APPROVE {
                button.primary()
            } else {
                button.ghost()
            };
            buttons = buttons.child(button);
        }
        card = card.child(buttons);
        if let Some(SubmitState::Failed(error)) = submit.map(|s| &s.state) {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        if !pending {
            card = card.child(note(outcome(question, &bot_name)));
        }
        card.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{heading, letter, outcome};
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
        assert!(
            heading(&question("external", "pending", None), "Mail").contains("sensitive action")
        );
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
}
