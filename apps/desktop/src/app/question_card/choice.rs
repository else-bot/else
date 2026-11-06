use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, FocusableExt as _, Icon, Sizable as _, StyledExt as _,
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use uuid::Uuid;

use super::{Connection, ElseApp, QuestionForm, RowKey, SubmitState, letter};
use crate::model::{Question, QuestionAnswer};

impl ElseApp {
    fn toggle_question_choice(&mut self, id: Uuid, choice: &str, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let Some(question) = view.conversation.as_ref().and_then(|c| c.question(id)) else {
            return;
        };
        if !question.is_pending()
            || !question.multiple
            || self.connection != Connection::Connected
            || !question.choices.iter().any(|c| c.id == choice)
            || view
                .question_submits
                .get(&id)
                .is_some_and(QuestionForm::locks)
        {
            return;
        }
        let form = view
            .question_submits
            .entry(id)
            .or_insert_with(|| QuestionForm {
                answer: QuestionAnswer::Multiple(Vec::new()),
                state: SubmitState::Choosing,
            });
        let QuestionAnswer::Multiple(selected) = &mut form.answer else {
            return;
        };
        if selected.iter().any(|id| id == choice) {
            selected.retain(|id| id != choice);
        } else {
            selected.push(choice.to_owned());
        }
        form.state = SubmitState::Choosing;
        self.remeasure_row(RowKey::Question(id), cx);
    }

    pub(super) fn render_choice_card(
        &self,
        question: &Question,
        form: Option<&QuestionForm>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let id = question.id;
        let submitted = form.is_some_and(QuestionForm::locks);
        let completed = !question.is_pending() || submitted;
        let locked = completed || self.connection != Connection::Connected;
        let answer = question.answer.as_ref().or_else(|| form.map(|f| &f.answer));
        let selected: Vec<_> = question
            .choices
            .iter()
            .filter(|choice| answer.is_some_and(|a| a.contains(&choice.id)))
            .collect();
        let mut card =
            v_flex()
                .w_full()
                .gap_3()
                .p_4()
                .rounded(theme.radius_lg)
                .bg(theme.secondary)
                .child(div().text_xs().text_color(theme.muted_foreground).child(
                    if question.multiple {
                        "Choose any"
                    } else if question.choices.is_empty() {
                        "Reply in chat"
                    } else {
                        "Choose one"
                    },
                ))
                .child(
                    div()
                        .font_semibold()
                        .whitespace_normal()
                        .when(completed, |title| title.text_color(theme.muted_foreground))
                        .child(question.title.clone()),
                );
        if let Some(context) = &question.context {
            card = card.child(
                div()
                    .text_sm()
                    .whitespace_normal()
                    .text_color(theme.muted_foreground)
                    .child(context.clone()),
            );
        }
        let mut rows = v_flex().w_full().gap_1();
        if completed {
            if question.status == "answered" || (question.is_pending() && submitted) {
                for choice in &selected {
                    let badge = h_flex()
                        .size_6()
                        .flex_shrink_0()
                        .justify_center()
                        .rounded(theme.radius)
                        .bg(theme.success.opacity(0.16))
                        .text_color(theme.success)
                        .text_xs();
                    let badge = if question.multiple {
                        badge.child(Icon::new(IconName::Check).xsmall())
                    } else {
                        badge.child(letter(&choice.id))
                    };
                    rows = rows.child(
                        h_flex().gap_2p5().py_1().child(badge).child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .whitespace_normal()
                                .child(choice.label.clone()),
                        ),
                    );
                }
            }
            let (icon, status) = match question.status.as_str() {
                "skipped" => (IconName::MessageSquare, "Continued in chat".to_owned()),
                "cancelled" => (IconName::LockKeyhole, "This question is closed".to_owned()),
                _ if form.is_some_and(|f| matches!(f.state, SubmitState::Sending)) => {
                    (IconName::LoaderCircle, "Sending…".to_owned())
                }
                _ if question.multiple => (
                    IconName::Check,
                    format!(
                        "{} {} selected",
                        selected.len(),
                        if selected.len() == 1 {
                            "action"
                        } else {
                            "actions"
                        }
                    ),
                ),
                _ => (IconName::Check, "Answer sent".to_owned()),
            };
            rows = rows.child(
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(icon).xsmall())
                    .child(status),
            );
        } else {
            for choice in &question.choices {
                let choice_id = choice.id.clone();
                if question.multiple {
                    let checked = answer.is_some_and(|a| a.contains(&choice.id));
                    rows = rows.child(
                        Checkbox::new(SharedString::from(format!("question-{id}-{}", choice.id)))
                            .focus_ring(false)
                            .small()
                            .checked(checked)
                            .disabled(locked)
                            .w_full()
                            .p_2()
                            .gap_2p5()
                            .rounded(theme.radius)
                            .when(!locked, |row| row.cursor_pointer())
                            .when(checked, |row| row.bg(theme.primary.opacity(0.1)))
                            .when(!locked && !checked, |row| {
                                row.hover(|style| style.bg(theme.accent))
                            })
                            .accessibility_label(choice.label.clone())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_sm()
                                    .whitespace_normal()
                                    .child(choice.label.clone()),
                            )
                            .on_change(cx.listener(move |app, _, _, cx| {
                                app.toggle_question_choice(id, &choice_id, cx);
                            })),
                    );
                } else {
                    rows = rows.child(
                        Button::new(SharedString::from(format!("question-{id}-{}", choice.id)))
                            .focus_ring(false)
                            .ghost()
                            .small()
                            .disabled(locked)
                            .w_full()
                            .h_auto()
                            .px_2()
                            .py_2p5()
                            .when(!locked, |row| row.cursor_pointer())
                            .accessibility_label(format!(
                                "{}. {}",
                                letter(&choice.id),
                                choice.label
                            ))
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2p5()
                                    .child(
                                        h_flex()
                                            .size_6()
                                            .flex_shrink_0()
                                            .justify_center()
                                            .rounded(theme.radius)
                                            .bg(theme.muted)
                                            .text_color(theme.muted_foreground)
                                            .text_xs()
                                            .child(letter(&choice.id)),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_left()
                                            .whitespace_normal()
                                            .child(choice.label.clone()),
                                    ),
                            )
                            .on_click(cx.listener(move |app, _, window, cx| {
                                app.answer_question(
                                    id,
                                    QuestionAnswer::Single(choice_id.clone()),
                                    window,
                                    cx,
                                );
                            })),
                    );
                }
            }
            let hint = if question.multiple {
                "Choose one or more, then click Done."
            } else if question.choices.is_empty() {
                "Reply in chat to answer this question."
            } else {
                "Choose an answer, or reply in chat."
            };
            let mut footer = h_flex().w_full().gap_3().flex_wrap().child(
                div()
                    .flex_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(hint),
            );
            if question.multiple {
                let answer =
                    QuestionAnswer::Multiple(selected.iter().map(|c| c.id.clone()).collect());
                let disabled = locked || selected.is_empty();
                footer = footer.child(
                    Button::new(SharedString::from(format!("question-{id}-done")))
                        .focus_ring(false)
                        .primary()
                        .small()
                        .disabled(disabled)
                        .when(!disabled, |button| button.cursor_pointer())
                        .label(format!("Done · {} selected", selected.len()))
                        .on_click(cx.listener(move |app, _, window, cx| {
                            app.answer_question(id, answer.clone(), window, cx);
                        })),
                );
            }
            rows = rows.child(footer);
        }
        card = card.child(rows);
        if let Some(SubmitState::Failed(error)) = form.map(|f| &f.state) {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone()),
            );
        }
        card.into_any_element()
    }
}
